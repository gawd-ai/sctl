//! Monitoring loop: a long-lived tokio task per config, under a supervisor.
//!
//! [`start`] is the one start path, at boot and for a push (ADR-010 decision
//! 6). It runs the loop under the library's supervisor, which rebuilds a
//! panicked loop after 5 s from the config the unit then holds, and starts
//! nothing in an agent that started in safe mode. A push of another config
//! aborts the monitor and starts a new one; a push of the config held keeps
//! it.
//!
//! Every tick (5 s) starts each due target that has no check in flight and
//! does not wait for them, so a slow target holds back neither the tick nor
//! the other targets. At most 3 checks run at once, and the semaphore serves
//! its permits in order. A check runs under a deadline from the moment it
//! takes its permit. Its target stays in flight, in the Infra state, until
//! its result is applied.
//!
//! At the end of a tick a round is counted, the `rounds` of `GET /api/info`
//! (ADR-010 decision 5), while every running check is within its deadline
//! plus one tick of taking its permit. A monitor that is gone, blocked on
//! the Infra lock, stuck on a check or panicking in its tick moves no
//! `rounds`; one that checks moves it every tick.
//!
//! A result is applied only while the unit still holds the config the check
//! ran under, the target in it. Due times and cooldowns use the monotonic
//! clock, so a step of the wall clock cannot stop the checks. A check that
//! panics leaves its target `unknown`, saying the agent failed to check it,
//! and never fires a recovery. A good answer slower than the target's
//! threshold never makes it Up (ADR-010 decision 8).
//!
//! A recovery (ADR-010 decision 9) starts only on a failed check of the
//! monitor that leaves its target Down, within its cooldown and
//! `max_retries`. Under the Infra lock the result is folded and the Down
//! signalled first, so the run never holds it back: its push leaves within
//! about a second, and the command may start before it does. The run goes
//! on in a task of its own, outside the monitor's task set, and the tick
//! checks no target while its run is in progress. Nothing but its own
//! 300 s limit or the agent's stop ends it: no push, rebuilt monitor or
//! removed config.
//!
//! Check now ([`check_now`], ADR-010 decision 10) is a reading: it runs under
//! the same deadline, its result is applied as a monitor check's is, and it
//! counts no round and starts no recovery. During a recovery run of its
//! target it still reads the target and moves its record, its status
//! included, while the target's recovery count and cooldown stand until the
//! run ends. The run's outcome then applies them: a target that reads Up or
//! Degraded came back during the run and starts afresh; one that reads Down
//! or `unknown` keeps the count and the cooldown the run began.
//!
//! `SCTL_INFRA_TEST_STALL_SECS`, for tests and benches, stops the loop that
//! many seconds after it starts, so a stopped monitor can be seen while the
//! agent answers.

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use futures_util::FutureExt as _;
use tokio::sync::{Mutex, Semaphore};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{Instant, MissedTickBehavior};
use tracing::{debug, error, info, warn};

use super::checks::{self, CheckContext, CheckResult};
use super::{
    now_iso, now_unix_ms, CheckSpec, DataSample, Flight, InfraConfig, InfraState, InfraTarget,
    RecoveryConfig, RecoveryLogEntry, RecoverySnapshot, RunGuard, SavedRun, TargetState,
    TargetStatus,
};

/// How many checks may run at once. Small: these devices have one core and
/// the checks fork processes.
const CHECK_CONCURRENCY: usize = 3;
/// How often the monitor looks for due targets and counts a round.
const TICK: Duration = Duration::from_secs(5);
/// Hard ceiling on one check, whatever its own timeouts say.
const CHECK_DEADLINE: Duration = Duration::from_secs(90);
/// A recovery run's own limit. Nothing else ends a run, and a managed
/// upgrade waits at most this long for one before it hands off.
pub const RECOVERY_LIMIT: Duration = Duration::from_mins(5);
/// A recovery's stdout and stderr are each cut to this many bytes.
const RECOVERY_OUTPUT_BYTES: usize = 512;
/// The bench's stall hook: the loop stops ticking this many seconds after
/// it starts.
const STALL_ENV: &str = "SCTL_INFRA_TEST_STALL_SECS";
/// The detail of a target whose check panicked.
const CHECK_PANICKED: &str =
    "AGENT ERROR: the agent failed to check this target (its check panicked; see the agent's log)";
/// What the log says of a run whose record did not reach the disk.
const NOT_RECORDED: &str =
    "ERROR: the run's record did not reach the disk; the recovery did not run";

/// The monitor's clock.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timing {
    /// How often it ticks.
    pub tick: Duration,
    /// How long a check may run once it holds its permit.
    pub deadline: Duration,
    /// When the loop stops ticking, after it starts (the stall hook).
    pub stall_after: Option<Duration>,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            tick: TICK,
            deadline: CHECK_DEADLINE,
            stall_after: None,
        }
    }
}

impl Timing {
    /// The monitor's own timing, with the stall hook read from the
    /// environment when the monitor starts.
    fn from_env() -> Self {
        let raw = std::env::var(STALL_ENV).ok();
        let stall_after = raw.as_deref().and_then(stall_secs);
        if let (Some(raw), None) = (&raw, stall_after) {
            warn!("{STALL_ENV}={raw:?} is not a number of seconds; ignored");
        }
        Self {
            stall_after,
            ..Self::default()
        }
    }
}

/// The stall hook's value: whole seconds.
fn stall_secs(raw: &str) -> Option<Duration> {
    raw.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// Runs one target's check. It is called during the tick, which spawns the
/// future it returns; the monitor's own runs [`checks::run_check_with`], and
/// a test gives its own.
pub(crate) type Checker =
    Arc<dyn Fn(&InfraTarget, CheckContext) -> BoxFuture<'static, CheckResult> + Send + Sync>;

/// The checker every monitor of the agent runs.
fn run_checks() -> Checker {
    Arc::new(|target: &InfraTarget, ctx: CheckContext| {
        let spec = target.check.clone();
        async move { checks::run_check_with(&spec, &ctx).await }.boxed()
    })
}

/// How the unit's monitors and Check now are built: the agent's own clock,
/// its stall hook read from the environment at each start, and its own
/// checks; or a test's.
#[derive(Clone, Default)]
pub(crate) enum Kit {
    #[default]
    Agent,
    /// A test's clock and checks.
    #[cfg(test)]
    Given { timing: Timing, checker: Checker },
}

impl Kit {
    fn timing(&self) -> Timing {
        match self {
            Self::Agent => Timing::from_env(),
            #[cfg(test)]
            Self::Given { timing, .. } => *timing,
        }
    }

    fn checker(&self) -> Checker {
        match self {
            Self::Agent => run_checks(),
            #[cfg(test)]
            Self::Given { checker, .. } => checker.clone(),
        }
    }

    /// How long one check may run: the monitor's checks and Check now.
    fn deadline(&self) -> Duration {
        match self {
            Self::Agent => CHECK_DEADLINE,
            #[cfg(test)]
            Self::Given { timing, .. } => timing.deadline,
        }
    }
}

/// Start the monitor for the config the unit holds, supervised, unless the
/// agent started in safe mode, the unit holds no config, or a monitor runs
/// already. The one start path, at boot and for a push (ADR-010 decision
/// 6). `state` is `infra`'s, locked by the caller. True when it started one.
pub fn start(infra: &Arc<Mutex<InfraState>>, state: &mut InfraState) -> bool {
    if state.safe_mode() || state.config.is_none() {
        return false;
    }
    if state
        .monitor_handle
        .as_ref()
        .is_some_and(|handle| !handle.is_finished())
    {
        return false;
    }
    state.monitor_handle = Some(supervised(infra.clone()));
    true
}

/// The monitor under the library's supervisor. Each build reads the config
/// the unit holds then, so a loop rebuilt after a panic watches that config,
/// never a copy kept from before. With no config held there is nothing to
/// watch, and the supervisor ends.
fn supervised(infra: Arc<Mutex<InfraState>>) -> JoinHandle<()> {
    crate::supervise::spawn_supervised(
        "infra_monitor",
        crate::supervise::RESTART_DELAY,
        move || {
            let infra = infra.clone();
            async move {
                let built = {
                    let state = infra.lock().await;
                    state
                        .config
                        .clone()
                        .map(|config| (config, state.config_generation, state.monitor_kit.clone()))
                };
                let Some((config, generation, kit)) = built else {
                    return;
                };
                Monitor::new(infra, config, generation, kit.timing(), kit.checker())
                    .run()
                    .await;
            }
        },
    )
}

/// A monitor of `config` with its clock and its checker given, outside any
/// supervisor, for the tests.
#[cfg(test)]
pub(crate) fn spawn_monitor_with(
    infra: Arc<Mutex<InfraState>>,
    config: InfraConfig,
    timing: Timing,
    checker: Checker,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let generation = infra.lock().await.config_generation;
        Monitor::new(infra, config, generation, timing, checker)
            .run()
            .await;
    })
}

/// One monitor: its config, its clock, and the checks it started. Its
/// checks end with it: aborting the monitor aborts them, and each frees its
/// target as it goes. A recovery run is not one of them.
struct Monitor {
    infra: Arc<Mutex<InfraState>>,
    config: Arc<InfraConfig>,
    /// The config generation its checks ran under.
    generation: u64,
    timing: Timing,
    checker: Checker,
    limiter: Arc<Semaphore>,
    /// When each target's last check was started, on the tick's schedule.
    last_start: HashMap<String, Instant>,
    checks: JoinSet<()>,
}

impl Monitor {
    fn new(
        infra: Arc<Mutex<InfraState>>,
        config: InfraConfig,
        generation: u64,
        timing: Timing,
        checker: Checker,
    ) -> Self {
        Self {
            infra,
            config: Arc::new(config),
            generation,
            timing,
            checker,
            limiter: Arc::new(Semaphore::new(CHECK_CONCURRENCY)),
            last_start: HashMap::new(),
            checks: JoinSet::new(),
        }
    }

    async fn run(mut self) {
        info!(
            "Infra monitor started: {} targets, config v{}",
            self.config.targets.len(),
            self.config.version
        );
        if let Some(after) = self.timing.stall_after {
            warn!(
                "{STALL_ENV}: this Infra monitor stops ticking {}s after it starts \
                 (tests and benches only)",
                after.as_secs()
            );
        }
        let digest = self.infra.lock().await.digest();
        let started = Instant::now();
        let mut interval = tokio::time::interval(self.timing.tick);
        // A tick held up (by the lock) is not made up for in a burst.
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            let at = interval.tick().await;
            if self
                .timing
                .stall_after
                .is_some_and(|after| started.elapsed() >= after)
            {
                warn!("{STALL_ENV}: the Infra monitor stopped ticking; no round is counted");
                // The checks in flight end on their own; nothing starts again.
                return std::future::pending().await;
            }
            self.reap();
            if self.tick(at).await {
                digest.count_round();
            }
        }
    }

    /// One tick: start each due target that has no check in flight, then
    /// say whether the round counts. It holds the Infra lock while it
    /// schedules, so a lock held for good holds the tick and no round is
    /// counted.
    async fn tick(&mut self, at: Instant) -> bool {
        let guard = self.infra.lock().await;
        let runs = guard.runs();
        for target in &self.config.targets {
            let every = Duration::from_secs(
                target
                    .interval_secs
                    .unwrap_or(self.config.check_interval_secs),
            );
            if self
                .last_start
                .get(&target.id)
                .is_some_and(|last| at.saturating_duration_since(*last) < every)
            {
                continue; // not due yet
            }
            if runs.contains(&target.id) {
                // Its recovery runs: due again at the first tick after the
                // run has ended.
                debug!(
                    "Infra: {}'s recovery run is in progress; not checked",
                    target.id
                );
                continue;
            }
            let Some(flight) = guard.in_flight.start(&target.id) else {
                // Still being checked: due again at the next tick.
                debug!("Infra: {} is still in flight; not started again", target.id);
                continue;
            };
            self.last_start.insert(target.id.clone(), at);
            let check = (self.checker)(target, context_for(&guard, target));
            let job = Job {
                target: target.clone(),
                config_version: self.config.version,
                generation: self.generation,
                deadline: self.timing.deadline,
            };
            self.checks.spawn(run_check(
                self.infra.clone(),
                job,
                check,
                flight,
                self.limiter.clone(),
            ));
        }
        let permits = guard.in_flight.permit_times();
        drop(guard);
        round_counts(
            Instant::now(),
            &permits,
            self.timing.deadline + self.timing.tick,
        )
    }

    /// Collect the checks that ended. A check's own panic is caught where
    /// it runs; one past it (in applying the result) is logged here.
    fn reap(&mut self) {
        while let Some(ended) = self.checks.try_join_next() {
            if let Err(e) = ended {
                if e.is_panic() {
                    error!("Infra: a check task panicked after its check: {e}");
                }
            }
        }
    }
}

/// The round rule (ADR-010 decision 5): a tick that ends at `now` counts a
/// round only while every running check took its permit within `limit`,
/// the check deadline plus one tick. A check that still waits for a permit,
/// or has given it back, is not running, so it is not in `permits`.
pub(crate) fn round_counts(now: Instant, permits: &[Instant], limit: Duration) -> bool {
    permits
        .iter()
        .all(|at| now.saturating_duration_since(*at) <= limit)
}

/// One check of one target, as it was started.
#[derive(Debug, Clone)]
struct Job {
    target: InfraTarget,
    /// The version of the config it ran under.
    config_version: u32,
    /// The generation of that config: its result is applied only while the
    /// unit still holds it.
    generation: u64,
    deadline: Duration,
}

/// How a check ended.
enum Outcome {
    Answered(CheckResult),
    /// It panicked; the panic's text.
    Panicked(String),
    /// No answer within its deadline.
    TimedOut,
}

impl Outcome {
    /// Run `check` under `deadline`, catching its panic.
    async fn of(check: BoxFuture<'static, CheckResult>, deadline: Duration) -> Self {
        match tokio::time::timeout(deadline, AssertUnwindSafe(check).catch_unwind()).await {
            Ok(Ok(result)) => Self::Answered(result),
            Ok(Err(panic)) => Self::Panicked(panic_text(panic.as_ref()).to_string()),
            Err(_) => Self::TimedOut,
        }
    }
}

/// Whose reading a result is: only a failed check of the monitor starts a
/// recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reading {
    /// A check the monitor started.
    Monitor,
    /// A person's Check now.
    CheckNow,
}

/// One check of the monitor, from its permit to its result applied. It
/// holds its target's place in flight until the result is in, and its
/// permit only while the check runs. A recovery it starts holds no place:
/// the tick skips the target while the run is in progress, and Check now
/// still reads it.
async fn run_check(
    infra: Arc<Mutex<InfraState>>,
    job: Job,
    check: BoxFuture<'static, CheckResult>,
    flight: Flight,
    limiter: Arc<Semaphore>,
) {
    let Ok(permit) = limiter.acquire_owned().await else {
        return;
    };
    flight.took_permit(Instant::now());
    debug!("Checking target {}: {}", job.target.id, job.target.name);
    let outcome = Outcome::of(check, job.deadline).await;
    drop(permit);
    // No longer running: the round rule stops reading its permit time.
    flight.left_permit();
    let recovery = {
        let mut state = infra.lock().await;
        let (result, recovery) = settle(&mut state, &job, outcome, Reading::Monitor);
        flight.publish(&result);
        // Out of flight only once its result is in.
        drop(flight);
        recovery
    };
    if let Some(run) = recovery {
        // The Down is signalled and the lock released: the run goes on in a
        // task of its own.
        drop(tokio::spawn(run_recovery(infra, run)));
    }
}

/// Apply a check's outcome, when the unit still holds the config the check
/// ran under and the target is in it, and say what it answers. Returns the
/// recovery run that a failed check of the monitor started, if any.
fn settle(
    state: &mut InfraState,
    job: &Job,
    outcome: Outcome,
    reading: Reading,
) -> (CheckResult, Option<RecoveryRun>) {
    let target = &job.target;
    let holds = state.holds(job.generation, &target.id);
    let result = match outcome {
        Outcome::Answered(result) => result,
        Outcome::TimedOut => CheckResult::failed(format!(
            "CHECK DEADLINE: no answer within {}s",
            job.deadline.as_secs()
        )),
        Outcome::Panicked(text) => {
            error!(
                "Infra: the check of {} ({}) panicked: {text}",
                target.id, target.name
            );
            if holds {
                apply_check_panic(state, target, job.config_version);
            }
            return (CheckResult::failed(CHECK_PANICKED), None);
        }
    };
    if !holds {
        debug!(
            "Infra: the config changed during the check of {}; its result is not applied",
            target.id
        );
        return (result, None);
    }
    let run = apply_result(
        state,
        target,
        result.clone(),
        job.config_version,
        Instant::now(),
        reading,
    );
    (result, run)
}

/// What a panic said, for the log.
fn panic_text(panic: &(dyn std::any::Any + Send)) -> &str {
    panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("no message")
}

/// What a target's check needs from shared state: the pin store location,
/// its credential, and the session cached from the last run.
fn context_for(state: &InfraState, target: &InfraTarget) -> CheckContext {
    let CheckSpec::HttpApi { credential_id, .. } = &target.check else {
        return CheckContext::default();
    };
    CheckContext {
        data_dir: state
            .config_path
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        credential: credential_id
            .as_ref()
            .and_then(|id| state.credentials.get(id).cloned()),
        session: state.sessions.get(&target.id).cloned(),
    }
}

// ─── Check now ───────────────────────────────────────────────────────

/// Why Check now did not run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckNowError {
    /// The unit holds no config.
    NoConfig,
    /// The config the unit holds has no such target.
    UnknownTarget,
}

/// Check now (`POST /api/infra/check/{target_id}`, ADR-010 decision 10): a
/// reading of one target, under the monitor's deadline, applied as a
/// monitor check's is. It counts no round and never starts a recovery. A
/// target has at most one check in flight: while one is, Check now waits
/// for it, at most the deadline, and answers its result, and no second
/// check runs. A recovery run is not a check: during one Check now reads
/// the target, and its result moves the record while the target's
/// recovery count and cooldown stand until the run ends. A result is
/// applied only while the unit still holds the config the check ran under,
/// the target in it; otherwise it is answered and nothing is applied.
pub async fn check_now(
    infra: &Arc<Mutex<InfraState>>,
    target_id: &str,
) -> Result<CheckResult, CheckNowError> {
    let mut waiting_since: Option<Instant> = None;
    loop {
        let (taken, deadline) = {
            let state = infra.lock().await;
            let config = state.config.as_ref().ok_or(CheckNowError::NoConfig)?;
            let target = config
                .targets
                .iter()
                .find(|t| t.id == target_id)
                .ok_or(CheckNowError::UnknownTarget)?;
            let deadline = state.monitor_kit.deadline();
            let taken = state.in_flight.take_or_wait(target_id).map(|flight| {
                let check = (state.monitor_kit.checker())(target, context_for(&state, target));
                let job = Job {
                    target: target.clone(),
                    config_version: config.version,
                    generation: state.config_generation,
                    deadline,
                };
                (job, check, flight)
            });
            (taken, deadline)
        };
        let mut answer = match taken {
            Ok((job, check, flight)) => return Ok(check_now_with(infra, &job, check, flight).await),
            Err(answer) => answer,
        };
        // A check of the target is in flight: its result is the answer.
        let since = *waiting_since.get_or_insert_with(Instant::now);
        let left = deadline.saturating_sub(since.elapsed());
        let waited = tokio::time::timeout(left, answer.wait_for(Option::is_some))
            .await
            .map(|answered| answered.ok().and_then(|result| Option::clone(&result)));
        match waited {
            Ok(Some(result)) => return Ok(result),
            // It ended without a result, cancelled with its monitor: the
            // place is free, so take it.
            Ok(None) => {}
            Err(_) => {
                return Ok(CheckResult::failed(format!(
                    "CHECK DEADLINE: the check in flight gave no answer within {}s",
                    deadline.as_secs()
                )));
            }
        }
    }
}

/// Check now's own check, from its start to its result applied.
async fn check_now_with(
    infra: &Arc<Mutex<InfraState>>,
    job: &Job,
    check: BoxFuture<'static, CheckResult>,
    flight: Flight,
) -> CheckResult {
    // A running check, as the round rule reads it: one held past its
    // deadline plus a tick stops the rounds, as the monitor's do.
    flight.took_permit(Instant::now());
    let outcome = Outcome::of(check, job.deadline).await;
    flight.left_permit();
    let mut state = infra.lock().await;
    let (result, recovery) = settle(&mut state, job, outcome, Reading::CheckNow);
    debug_assert!(recovery.is_none(), "Check now never starts a recovery");
    drop(recovery);
    flight.publish(&result);
    result
}

// ─── Results and the state machine ───────────────────────────────────

/// What a check that panicked leaves (ADR-010 decision 5): the target reads
/// `unknown`, its detail saying the agent failed to check it. The agent's
/// own defect never counts against the target and never fires its
/// recovery: the counters start again from zero, and the recovery count
/// stands.
fn apply_check_panic(state: &mut InfraState, target: &InfraTarget, config_version: u32) {
    let current = state.results.targets.get(&target.id);
    let shown = current.is_some();
    let prev_status = current.map_or(TargetStatus::Unknown, |t| t.status);
    let version_before = state.results.config_version;
    let ts = now_iso();
    let since = match current {
        Some(t) if t.status == TargetStatus::Unknown => t.since.clone(),
        _ => ts.clone(),
    };
    // A profile's last good snapshot stays visible, as after a failed check.
    let data = current
        .and_then(|t| t.data.clone())
        .filter(|_| matches!(target.check, CheckSpec::HttpApi { .. }));
    state.results.targets.insert(
        target.id.clone(),
        TargetState {
            status: TargetStatus::Unknown,
            latency_ms: None,
            since,
            consecutive_ok: 0,
            consecutive_fail: 0,
            consecutive_within: 0,
            last_check: ts.clone(),
            detail: CHECK_PANICKED.to_string(),
            name: target.name.clone(),
            http_status: None,
            data,
        },
    );
    state.results.ts = ts;
    state.results.config_version = config_version;
    // Whatever went wrong, do not trust the cached session past it.
    state.sessions.remove(&target.id);
    if prev_status != TargetStatus::Unknown || !shown || version_before != config_version {
        warn!(
            "Target {} ({}) status: {prev_status} → unknown: the agent failed to check it",
            target.id, target.name
        );
        state.changed();
    }
}

/// Fold one check result into the target's state: the state machine, the
/// session and the data history, a status change signalled. `now` is the
/// monotonic clock, which the recovery's cooldown reads. A failed check of
/// the monitor (`reading`) that leaves the target Down returns the recovery
/// run it starts, recorded already: the caller runs it with
/// `run_recovery`, after the Infra lock is released.
#[must_use = "a recovery run started must run"]
pub fn apply_result(
    state: &mut InfraState,
    target: &InfraTarget,
    result: CheckResult,
    config_version: u32,
    now: Instant,
    reading: Reading,
) -> Option<RecoveryRun> {
    let current = state.results.targets.get(&target.id);
    let shown = current.is_some();
    let prev_status = current.map_or(TargetStatus::Unknown, |t| t.status);
    let version_before = state.results.config_version;
    let within = result.ok
        && result
            .latency_ms
            .is_none_or(|ms| ms <= target.degraded_threshold_ms);
    let streak = Streak::after(current, result.ok, within);
    let new_status = compute_status(
        prev_status,
        result.ok,
        within,
        streak,
        target.down_after_consecutive,
        target.up_after_consecutive,
    );

    let since = if new_status == prev_status {
        current.map_or_else(now_iso, |t| t.since.clone())
    } else {
        now_iso()
    };

    // A failed API check keeps the last good snapshot visible, marked by the
    // status and counters, rather than blanking it: the consumer reads
    // `data.ts` for freshness and the status for health.
    let data = match (&result.data, current.and_then(|t| t.data.clone())) {
        (Some(d), _) => Some(d.clone()),
        (None, kept) if matches!(target.check, CheckSpec::HttpApi { .. }) => kept,
        _ => None,
    };

    let ts = now_iso();
    state.results.targets.insert(
        target.id.clone(),
        TargetState {
            status: new_status,
            latency_ms: result.latency_ms,
            since,
            consecutive_ok: streak.ok,
            consecutive_fail: streak.fail,
            consecutive_within: streak.within,
            last_check: ts.clone(),
            detail: result.detail.clone(),
            name: target.name.clone(),
            http_status: result.http_status,
            data,
        },
    );
    state.results.ts.clone_from(&ts);
    state.results.config_version = config_version;

    if let Some(session) = result.session {
        state.sessions.insert(target.id.clone(), session);
    } else if !result.ok {
        // Whatever the failure, do not trust the cached session past it.
        state.sessions.remove(&target.id);
    }
    if let Some(d) = result.data {
        state.push_data_sample(
            &target.id,
            DataSample {
                ts,
                status: new_status,
                latency_ms: result.latency_ms,
                data: d,
            },
        );
    }

    if new_status != prev_status {
        info!(
            "Target {} ({}) status: {prev_status} → {new_status}",
            target.id, target.name
        );
        if !result.ok {
            warn!("Target {} ({}): {}", target.id, target.name, result.detail);
        }
    }
    // The push rides changes of the picture only, signalled before any
    // recovery starts: a status, or the version and the target set when the
    // result is for a config the results no longer show. A check that
    // leaves them alone (a latency reading, a counter) is not one.
    if new_status != prev_status || !shown || version_before != config_version {
        state.changed();
    }

    // A good answer that moves the target out of Down or `unknown` clears
    // its recovery count (ADR-010 decision 9). A target that answers again
    // starts afresh, also when the agent failed to check it in between, or
    // restarted: a check that panics, and a start, leave the target
    // `unknown` and the count standing. During the target's recovery run
    // (a Check now) the count and the cooldown stand: the run's end applies
    // them ([`RecoveryRun::log`]).
    if result.ok
        && new_status != prev_status
        && matches!(prev_status, TargetStatus::Down | TargetStatus::Unknown)
    {
        if prev_status == TargetStatus::Down {
            info!("Target {} ({}) recovered", target.id, target.name);
        }
        if state.runs_not_ended.contains_key(&target.id) {
            debug!(
                "Target {}: its recovery run is in progress; the run's end applies its count",
                target.id
            );
        } else {
            state.clear_recovery_count(&target.id);
        }
    }

    // A recovery fires only on a failed check of the monitor that leaves
    // the target Down: the first Down and every failed check after it,
    // never a good answer and never Check now.
    if reading == Reading::Monitor && !result.ok && new_status == TargetStatus::Down {
        if let Some(recovery) = target.recovery.as_ref().filter(|r| r.enabled) {
            return start_recovery(state, &target.id, recovery, now);
        }
    }
    None
}

/// The answers in a row, the latest one counted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Streak {
    /// Good answers, slow ones included.
    ok: u32,
    /// Failures.
    fail: u32,
    /// Good answers within the threshold.
    within: u32,
}

impl Streak {
    /// The streak after an answer: `ok` says it was good, `within` that it
    /// was good and within the threshold.
    fn after(current: Option<&TargetState>, ok: bool, within: bool) -> Self {
        let before = current.map_or_else(Self::default, |t| Self {
            ok: t.consecutive_ok,
            fail: t.consecutive_fail,
            within: t.consecutive_within,
        });
        if ok {
            Self {
                ok: before.ok.saturating_add(1),
                fail: 0,
                within: if within {
                    before.within.saturating_add(1)
                } else {
                    0
                },
            }
        } else {
            Self {
                ok: 0,
                fail: before.fail.saturating_add(1),
                within: 0,
            }
        }
    }
}

/// The state machine (ADR-010 decision 8). A good answer above the
/// threshold is slow, and a slow answer never makes a target Up.
///
/// | From     | Good, within             | Good, slow | Failure                    |
/// |----------|--------------------------|------------|----------------------------|
/// | Unknown  | Up                       | Degraded   | Degraded                   |
/// | Up       | Up                       | Degraded   | Degraded                   |
/// | Degraded | Up at `up_after` in a row | Degraded  | Down at `down_after` in a row |
/// | Down     | below                    | below      | Down                       |
///
/// From Down, `up_after` good answers in a row lead to Up when all were
/// within the threshold, and to Degraded otherwise; until then the target
/// stays Down.
fn compute_status(
    prev: TargetStatus,
    ok: bool,
    within: bool,
    streak: Streak,
    down_after: u32,
    up_after: u32,
) -> TargetStatus {
    match prev {
        TargetStatus::Unknown | TargetStatus::Up => {
            if within {
                TargetStatus::Up
            } else {
                TargetStatus::Degraded
            }
        }
        TargetStatus::Degraded => {
            if within && streak.within >= up_after {
                TargetStatus::Up
            } else if !ok && streak.fail >= down_after {
                TargetStatus::Down
            } else {
                TargetStatus::Degraded
            }
        }
        TargetStatus::Down => {
            if !ok || streak.ok < up_after {
                TargetStatus::Down
            } else if within && streak.within >= up_after {
                TargetStatus::Up
            } else {
                TargetStatus::Degraded
            }
        }
    }
}

// ─── Recovery runs ───────────────────────────────────────────────────

/// `SCTL_CRED_USER_n` / `SCTL_CRED_PASS_n` for each credential id, in order
/// (n from 1); the first id this unit does not hold is the error.
pub(crate) fn recovery_env(
    store: &std::collections::HashMap<String, super::Credential>,
    ids: &[String],
) -> Result<Vec<(String, String)>, String> {
    let mut env = Vec::with_capacity(ids.len() * 2);
    for (i, id) in ids.iter().enumerate() {
        let c = store.get(id).ok_or_else(|| id.clone())?;
        env.push((format!("SCTL_CRED_USER_{}", i + 1), c.username.clone()));
        env.push((format!("SCTL_CRED_PASS_{}", i + 1), c.password.clone()));
    }
    Ok(env)
}

/// The shortest password a recovery's output is masked of: a shorter one
/// would mask common words of the output.
const MASKED_FROM_BYTES: usize = 4;

/// What a recovery's output is masked of: each password of at least 4 bytes
/// among the credentials the command names, as written and in the form HTTP
/// Basic sends it, the base64 of `username:password`, which a verbose HTTP
/// client writes to stderr and which carries no password as written.
/// Longest first, so a password that holds a shorter one is masked whole. A
/// password under 4 bytes and a username stay as they are.
fn masks<'a>(credentials: impl IntoIterator<Item = &'a super::Credential>) -> Vec<String> {
    use base64::Engine as _;
    let mut masks: Vec<String> = credentials
        .into_iter()
        .filter(|c| c.password.len() >= MASKED_FROM_BYTES)
        .flat_map(|c| {
            let basic = format!("{}:{}", c.username, c.password);
            [
                c.password.clone(),
                base64::engine::general_purpose::STANDARD.encode(basic),
            ]
        })
        .collect();
    masks.sort_unstable_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    masks.dedup();
    masks
}

/// A recovery's output with each of `masks` replaced by `***`, in order,
/// before it is logged or kept.
fn mask(text: &str, masks: &[String]) -> String {
    masks
        .iter()
        .fold(text.to_string(), |acc, s| acc.replace(s.as_str(), "***"))
}

/// `text` cut to at most `max` bytes on a character boundary, as `at.rs`
/// cuts, with `...` when something was cut. Byte `max` can fall inside a
/// character: a slice there would panic.
fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    format!("{}...", &text[..text.floor_char_boundary(max)])
}

/// A recovery output as it is logged: masked first, so a cut never leaves
/// part of a password, then cut to 512 bytes.
fn for_the_log(text: &str, masks: &[String]) -> String {
    cut(&mask(text, masks), RECOVERY_OUTPUT_BYTES)
}

/// A recovery run a failed check of the monitor started: recorded (the
/// cooldown begun, the count up, the run marked as not ended), and not yet
/// run. `run_recovery` runs it in a task of its own.
#[derive(Debug)]
pub struct RecoveryRun {
    target_id: String,
    command: String,
    credential_ids: Vec<String>,
    started_ms: u64,
    /// The run is in progress until this drops: one run per target at a
    /// time, and what a managed upgrade waits for.
    _in_progress: RunGuard,
}

/// What a run needs once the Infra lock is released.
enum Prepared {
    /// The command and the credentials it names, as environment.
    Ready {
        env: Vec<(String, String)>,
        /// What its output is masked of ([`masks`]).
        masks: Vec<String>,
    },
    /// The command cannot start: a credential is not on this unit.
    Refused(String),
}

/// How a run ended.
struct Ended {
    exit_code: i32,
    stdout: String,
    stderr: String,
}

impl RecoveryRun {
    /// Under the Infra lock: take the credentials the command names.
    fn prepare(&self, state: &InfraState) -> Prepared {
        match recovery_env(&state.credentials, &self.credential_ids) {
            Err(missing) => {
                Prepared::Refused(format!("ERROR: credential {missing} is not on this unit"))
            }
            Ok(env) => {
                let masks = masks(
                    self.credential_ids
                        .iter()
                        .filter_map(|id| state.credentials.get(id)),
                );
                Prepared::Ready { env, masks }
            }
        }
    }

    /// Without the lock, once the write of the run's record has ended: run
    /// the command only when the record reached the disk. That record is
    /// what bounds a recovery that power-cycles the unit's own supply across
    /// the restarts it causes, so without it no command starts. The run is
    /// logged all the same, and it counts as an attempt: its count and its
    /// cooldown moved when it was recorded in memory, so `max_retries` still
    /// holds while the agent runs.
    async fn execute_if_recorded(&self, recorded: bool, prepared: Prepared) -> Ended {
        if recorded {
            return self.execute(prepared).await;
        }
        warn!(
            "Recovery for {}: its record did not reach the disk; the recovery does not run",
            self.target_id
        );
        Ended {
            exit_code: -1,
            stdout: NOT_RECORDED.to_string(),
            stderr: String::new(),
        }
    }

    /// Without the lock: run the command under its 300 s limit, and mask and
    /// cut what it said. A panic here is the agent's, and still ends the
    /// run with a log entry.
    async fn execute(&self, prepared: Prepared) -> Ended {
        let (env, masks) = match prepared {
            Prepared::Refused(why) => {
                return Ended {
                    exit_code: -1,
                    stdout: why,
                    stderr: String::new(),
                }
            }
            Prepared::Ready { env, masks } => (env, masks),
        };
        info!(
            "Executing recovery for {}: {}",
            self.target_id, self.command
        );
        let limit_ms = RECOVERY_LIMIT.as_millis() as u64;
        let run = async {
            match checks::exec_with_env(&self.command, &env, limit_ms).await {
                Ok((exit_code, out, err)) => Ended {
                    exit_code,
                    stdout: for_the_log(&out, &masks),
                    stderr: for_the_log(&err, &masks),
                },
                Err(e) => Ended {
                    exit_code: -1,
                    stdout: format!("ERROR: {e}"),
                    stderr: String::new(),
                },
            }
        };
        match AssertUnwindSafe(run).catch_unwind().await {
            Ok(ended) => ended,
            Err(panic) => {
                error!(
                    "Recovery for {}: the agent failed to finish the run: {}",
                    self.target_id,
                    panic_text(panic.as_ref())
                );
                Ended {
                    exit_code: -1,
                    stdout:
                        "AGENT ERROR: the agent failed to finish this run (see the agent's log)"
                            .to_string(),
                    stderr: String::new(),
                }
            }
        }
    }

    /// Under the Infra lock: log the run, which moves `seq` and
    /// `recoveries` and is pushed, mark it ended, and apply its outcome to
    /// the count and the cooldown that readings during the run left as they
    /// were. A target that reads Up or Degraded now came back during the run
    /// (a run starts at Down, and only a good answer leaves Down): it starts
    /// afresh, as the good answer that moved it would have had it start. One
    /// that reads Down or `unknown` keeps the count and the cooldown the run
    /// began. Returns the record that saves all this, for the caller to
    /// write outside the lock.
    fn log(&self, state: &mut InfraState, ended: Ended) -> RecoverySnapshot {
        info!(
            "Recovery for {}: exit={}, output={}",
            self.target_id, ended.exit_code, ended.stdout
        );
        state.push_recovery_log(RecoveryLogEntry {
            ts: now_iso(),
            target_id: self.target_id.clone(),
            command: self.command.clone(),
            exit_code: ended.exit_code,
            stdout: ended.stdout,
            stderr: ended.stderr,
        });
        if state
            .runs_not_ended
            .get(&self.target_id)
            .is_some_and(|run| run.started_ms == self.started_ms)
        {
            state.runs_not_ended.remove(&self.target_id);
        }
        let came_back = state
            .results
            .targets
            .get(&self.target_id)
            .is_some_and(|t| matches!(t.status, TargetStatus::Up | TargetStatus::Degraded));
        if came_back && state.recovery_tracker.remove(&self.target_id).is_some() {
            info!(
                "Recovery for {}: the target answered during the run; its count is clear",
                self.target_id
            );
        }
        state.recoveries_snapshot()
    }
}

/// Start a recovery run of `target_id` if its count and its cooldown allow
/// one and none of it is in progress: the cooldown begins and the count
/// goes up now, and the run is marked as not ended. Its task saves that
/// record before the command starts.
fn start_recovery(
    state: &mut InfraState,
    target_id: &str,
    recovery: &RecoveryConfig,
    now: Instant,
) -> Option<RecoveryRun> {
    let (last_run, count) = state
        .recovery_tracker
        .get(target_id)
        .map_or((None, 0), |&(at, count)| (Some(at), count));
    if count >= recovery.max_retries {
        debug!(
            "Recovery for {target_id}: exhausted ({count}/{} retries)",
            recovery.max_retries
        );
        return None;
    }
    if let Some(last) = last_run {
        let since = now.saturating_duration_since(last);
        if since < Duration::from_secs(recovery.cooldown_secs) {
            debug!(
                "Recovery for {target_id}: cooling down ({}s of {}s)",
                since.as_secs(),
                recovery.cooldown_secs
            );
            return None;
        }
    }
    let Some(in_progress) = state.runs().start(target_id) else {
        debug!("Recovery for {target_id}: a run is in progress");
        return None;
    };
    let started_ms = now_unix_ms();
    state
        .recovery_tracker
        .insert(target_id.to_string(), (now, count + 1));
    state.runs_not_ended.insert(
        target_id.to_string(),
        SavedRun {
            started_ms,
            command: recovery.command.clone(),
        },
    );
    Some(RecoveryRun {
        target_id: target_id.to_string(),
        command: recovery.command.clone(),
        credential_ids: recovery.credential_ids.clone(),
        started_ms,
        _in_progress: in_progress,
    })
}

/// Run a recovery in a task of its own (ADR-010 decision 9). Under the
/// Infra lock it takes the credentials the command names and the record
/// that saves the run as not ended. It releases the lock, writes that
/// record, synced, on the blocking pool, and only once the record is on
/// disk runs the command, which only its 300 s limit or the agent's stop
/// ends: no push, rebuilt monitor or removed config. A run whose record did
/// not reach the disk starts no command, and is logged as such. Its output,
/// masked and cut, is logged under the lock with the run's outcome applied
/// to the count, and the record that saves it as ended is written outside
/// it. The run is in progress until that record is written: the monitor
/// checks the target again only after, and a managed upgrade's restart
/// waits for it. Check now reads the target meanwhile.
pub(crate) async fn run_recovery(infra: Arc<Mutex<InfraState>>, run: RecoveryRun) {
    let (prepared, record) = {
        let state = infra.lock().await;
        (run.prepare(&state), state.recoveries_snapshot())
    };
    // A synced write to flash can take hundreds of milliseconds: never on
    // the async worker, never under the lock the forwarder needs to send
    // the Down.
    let recorded = record.write().await;
    let ended = run.execute_if_recorded(recorded, prepared).await;
    let record = run.log(&mut *infra.lock().await, ended);
    record.write().await;
    drop(run);
}

#[cfg(test)]
#[allow(
    clippy::unreadable_literal,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
mod tests {

    #[test]
    fn recovery_env_names_each_credential_in_order() {
        let mut store = std::collections::HashMap::new();
        store.insert(
            "pdu".to_string(),
            super::super::Credential {
                username: "cyber".into(),
                password: "S3cret-pw".into(),
            },
        );
        store.insert(
            "rtr".to_string(),
            super::super::Credential {
                username: "admin".into(),
                password: "x".into(),
            },
        );
        let env = recovery_env(&store, &["pdu".into(), "rtr".into()]).unwrap();
        assert_eq!(
            env[0],
            ("SCTL_CRED_USER_1".to_string(), "cyber".to_string())
        );
        assert_eq!(env[3], ("SCTL_CRED_PASS_2".to_string(), "x".to_string()));
        assert_eq!(recovery_env(&store, &["gone".into()]).unwrap_err(), "gone");
        assert_eq!(
            mask("login S3cret-pw ok", &["S3cret-pw".into()]),
            "login *** ok"
        );
    }

    #[tokio::test]
    async fn a_recovery_command_reads_its_credentials_from_the_environment() {
        let env = vec![("SCTL_CRED_PASS_1".to_string(), "S3cret-pw".to_string())];
        let (code, out, _) = crate::infra::checks::exec_with_env(
            "printf %s \"$SCTL_CRED_PASS_1\" | wc -c",
            &env,
            5_000,
        )
        .await
        .unwrap();
        assert_eq!(code, 0);
        assert_eq!(out.trim(), "9");
    }

    use super::*;
    use serde_json::json;

    /// Apply `result` as a check of the monitor does, at `now`, and run any
    /// recovery it starts to its end, here, outside any task.
    async fn apply(
        state: &mut InfraState,
        target: &InfraTarget,
        result: CheckResult,
        config_version: u32,
        now: Instant,
    ) {
        let run = apply_result(state, target, result, config_version, now, Reading::Monitor);
        if let Some(run) = run {
            run_inline(state, run).await;
        }
    }

    /// A recovery run's parts in turn, on a state the test holds.
    async fn run_inline(state: &mut InfraState, run: RecoveryRun) {
        let prepared = run.prepare(state);
        let recorded = state.recoveries_snapshot().write().await;
        let ended = run.execute_if_recorded(recorded, prepared).await;
        run.log(state, ended).write().await;
    }

    /// A scratch `state_dir`, removed when it drops: the saved recovery
    /// counts land there.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("sctl-monitor-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &str {
            self.0.to_str().unwrap()
        }

        fn state(&self) -> InfraState {
            InfraState::new(self.path())
        }

        /// What `infra-recovery.json` holds now; `Null` when it is absent.
        fn saved(&self) -> serde_json::Value {
            std::fs::read_to_string(self.0.join("infra-recovery.json"))
                .map_or(serde_json::Value::Null, |text| {
                    serde_json::from_str(&text).unwrap()
                })
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn target(check: CheckSpec) -> InfraTarget {
        InfraTarget {
            id: "t1".into(),
            name: "Peplink".into(),
            check,
            degraded_threshold_ms: 200,
            down_after_consecutive: 3,
            up_after_consecutive: 2,
            interval_secs: None,
            recovery: None,
        }
    }

    fn api_target() -> InfraTarget {
        target(CheckSpec::HttpApi {
            base_url: "https://192.168.50.1".into(),
            profile: super::super::ApiProfile::Peplink,
            pin_sha256: None,
            credential_id: Some("c1".into()),
            timeout_ms: None,
        })
    }

    #[tokio::test]
    async fn a_good_api_check_stores_data_session_and_history() {
        let mut state = InfraState::new("/tmp/sctl-infra-test");
        let t = api_target();
        let result = CheckResult {
            ok: true,
            latency_ms: Some(40),
            detail: "API OK".into(),
            data: Some(json!({"ap_wan_up": 1})),
            session: Some("bauth=abc".into()),
            ..CheckResult::default()
        };
        apply(&mut state, &t, result, 7, Instant::now()).await;
        let ts = state.results.targets.get("t1").unwrap();
        assert_eq!(ts.status, TargetStatus::Up);
        assert_eq!(ts.data.as_ref().unwrap()["ap_wan_up"], 1);
        assert_eq!(
            state.sessions.get("t1").map(String::as_str),
            Some("bauth=abc")
        );
        assert_eq!(state.data_history.get("t1").unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_failed_api_check_keeps_the_last_snapshot_and_drops_the_session() {
        let mut state = InfraState::new("/tmp/sctl-infra-test");
        let t = api_target();
        let good = CheckResult {
            ok: true,
            latency_ms: Some(40),
            detail: "API OK".into(),
            data: Some(json!({"ap_wan_up": 1})),
            session: Some("bauth=abc".into()),
            ..CheckResult::default()
        };
        apply(&mut state, &t, good, 7, Instant::now()).await;
        apply(
            &mut state,
            &t,
            CheckResult::failed("API TIMEOUT"),
            7,
            Instant::now(),
        )
        .await;
        let ts = state.results.targets.get("t1").unwrap();
        assert_eq!(ts.status, TargetStatus::Degraded);
        assert_eq!(ts.detail, "API TIMEOUT");
        assert_eq!(
            ts.data.as_ref().unwrap()["ap_wan_up"],
            1,
            "the last reading stays visible; status says it is old"
        );
        assert!(
            !state.sessions.contains_key("t1"),
            "a failure invalidates the cached login"
        );
        assert_eq!(
            state.data_history.get("t1").unwrap().len(),
            1,
            "no sample for a failure"
        );
    }

    #[tokio::test]
    async fn the_history_ring_is_bounded() {
        let mut state = InfraState::new("/tmp/sctl-infra-test");
        let t = api_target();
        for i in 0..(super::super::MAX_DATA_HISTORY as i64 + 5) {
            let r = CheckResult {
                ok: true,
                latency_ms: Some(1),
                detail: "ok".into(),
                data: Some(json!({"i": i})),
                ..CheckResult::default()
            };
            apply(&mut state, &t, r, 1, Instant::now()).await;
        }
        let ring = state.data_history.get("t1").unwrap();
        assert_eq!(ring.len(), super::super::MAX_DATA_HISTORY);
        assert_eq!(
            ring.back().unwrap().data["i"],
            super::super::MAX_DATA_HISTORY as i64 + 4
        );
    }

    #[tokio::test]
    async fn a_status_change_signals_the_push_a_latency_reading_does_not() {
        let mut state = InfraState::new("/tmp/sctl-infra-test");
        let mut changes = state.watch_changes();
        let t = target(CheckSpec::Ping {
            host: "10.0.0.1".into(),
            timeout_ms: None,
        });
        let ok = |ms: u64| CheckResult {
            ok: true,
            latency_ms: Some(ms),
            detail: format!("PING OK {ms}ms"),
            ..CheckResult::default()
        };

        // unknown -> up
        apply(&mut state, &t, ok(3), 1, Instant::now()).await;
        assert!(changes.has_changed().unwrap());
        changes.borrow_and_update();

        // up stays up: a new latency, a new counter, a new last_check
        apply(&mut state, &t, ok(9), 1, Instant::now()).await;
        assert!(!changes.has_changed().unwrap(), "not a status change");

        // up -> degraded
        apply(
            &mut state,
            &t,
            CheckResult::failed("PING TIMEOUT"),
            1,
            Instant::now(),
        )
        .await;
        assert!(changes.has_changed().unwrap());
    }

    #[test]
    fn plain_kinds_never_carry_data() {
        let t = target(CheckSpec::Ping {
            host: "10.0.0.1".into(),
            timeout_ms: None,
        });
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut state = InfraState::new("/tmp/sctl-infra-test");
            let r = CheckResult {
                ok: true,
                latency_ms: Some(3),
                detail: "PING OK".into(),
                ..CheckResult::default()
            };
            apply(&mut state, &t, r, 1, Instant::now()).await;
            assert!(state.results.targets.get("t1").unwrap().data.is_none());
        });
    }

    // ─── The proof (ADR-010) ─────────────────────────────────────────

    use std::sync::atomic::{AtomicU32, Ordering::SeqCst};

    use super::super::{picture_hash, picture_text, InFlight, RecoveryConfig};

    /// A target checked every `every_secs`, by a ping no test sends.
    fn target_every(id: &str, every_secs: u64) -> InfraTarget {
        InfraTarget {
            id: id.into(),
            name: format!("Target {id}"),
            check: CheckSpec::Ping {
                host: "192.0.2.1".into(),
                timeout_ms: None,
            },
            degraded_threshold_ms: 200,
            down_after_consecutive: 3,
            up_after_consecutive: 2,
            interval_secs: Some(every_secs),
            recovery: None,
        }
    }

    /// An Infra state holding a config of `targets` (v7), and that config.
    fn held(targets: Vec<InfraTarget>) -> (Arc<Mutex<InfraState>>, InfraConfig) {
        let config = InfraConfig {
            version: 7,
            check_interval_secs: 60,
            targets,
        };
        let mut state = InfraState::new("/tmp/sctl-infra-proof-test");
        assert!(state.hold_config(config.clone()));
        (Arc::new(Mutex::new(state)), config)
    }

    fn good() -> CheckResult {
        CheckResult {
            ok: true,
            latency_ms: Some(3),
            detail: "OK 3ms".into(),
            ..CheckResult::default()
        }
    }

    /// A checker whose every check answers at once, and well.
    fn answers_ok() -> Checker {
        Arc::new(|_: &InfraTarget, _: CheckContext| async { good() }.boxed())
    }

    /// Wait, on the real clock, until `done`; fail after `limit`.
    async fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + limit;
        while !done() {
            assert!(std::time::Instant::now() < deadline, "not within {limit:?}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[test]
    fn the_round_rule_reads_each_running_checks_permit_time() {
        let limit = CHECK_DEADLINE + TICK;
        let t0 = Instant::now();
        let at = |secs| t0 + Duration::from_secs(secs);
        assert!(round_counts(at(0), &[], limit), "no check running");
        assert!(
            round_counts(at(95), &[t0, at(50)], limit),
            "within 90 s plus one tick of their permits"
        );
        assert!(
            !round_counts(at(96), &[t0, at(50)], limit),
            "one held past its deadline plus a tick"
        );
        assert!(round_counts(t0, &[at(1)], limit), "a permit read after now");

        // Through the set of checks in flight: a check that waits for its
        // permit is not running, whatever its age.
        let in_flight = InFlight::default();
        let waiting = in_flight.start("queued").unwrap();
        assert!(round_counts(at(500), &in_flight.permit_times(), limit));
        let stuck = in_flight.start("stuck").unwrap();
        stuck.took_permit(t0);
        assert!(!round_counts(at(96), &in_flight.permit_times(), limit));
        // Its check ended and gave the permit back: it waits only for its
        // result to be applied, and is not running.
        stuck.left_permit();
        assert!(round_counts(at(96), &in_flight.permit_times(), limit));
        drop(stuck);
        assert!(round_counts(at(96), &in_flight.permit_times(), limit));
        drop(waiting);
    }

    /// The round rule through the monitor's own tick. A check that holds
    /// its permit past its deadline plus a tick, as one that blocks its
    /// thread would (no timeout cuts that), stops the rounds, and its
    /// target is not started again. Once it lets go, the rounds go on.
    #[tokio::test(start_paused = true)]
    async fn a_check_held_past_its_deadline_stops_rounds_through_the_tick() {
        let (infra, config) = held(vec![target_every("t1", 3600)]);
        let digest = infra.lock().await.digest();
        // The check of t1 took its permit at 0 s and does not let go.
        let stuck = infra.lock().await.in_flight.start("t1").unwrap();
        stuck.took_permit(Instant::now());
        let starts = Arc::new(AtomicU32::new(0));
        let checker: Checker = {
            let starts = starts.clone();
            Arc::new(move |_: &InfraTarget, _: CheckContext| {
                starts.fetch_add(1, SeqCst);
                async { good() }.boxed()
            })
        };
        let monitor = spawn_monitor_with(infra.clone(), config, Timing::default(), checker);

        tokio::time::sleep(Duration::from_secs(96)).await;
        assert_eq!(
            digest.group().rounds,
            20,
            "the ticks at 0 to 95 s, within 90 s plus a tick of the permit"
        );
        tokio::time::sleep(Duration::from_mins(1)).await;
        assert_eq!(digest.group().rounds, 20, "none from the tick at 100 s on");
        assert_eq!(starts.load(SeqCst), 0, "t1 is in flight: not started again");

        drop(stuck);
        tokio::time::sleep(Duration::from_secs(6)).await;
        assert_eq!(starts.load(SeqCst), 1, "started at the tick at 160 s");
        assert_eq!(digest.group().rounds, 21, "and that tick counts its round");
        monitor.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_tick_that_panics_counts_no_round() {
        let (infra, config) = held(vec![target_every("t1", 60)]);
        let digest = infra.lock().await.digest();
        let calls = Arc::new(AtomicU32::new(0));
        // The tick calls the checker as it starts a check: the second start
        // (at 60 s) is a defect in the tick itself.
        let checker: Checker = {
            let calls = calls.clone();
            Arc::new(move |_: &InfraTarget, _: CheckContext| {
                assert_eq!(calls.fetch_add(1, SeqCst), 0, "a defect in the tick");
                async { good() }.boxed()
            })
        };
        let monitor = spawn_monitor_with(infra.clone(), config, Timing::default(), checker);
        let ended = tokio::time::timeout(Duration::from_mins(10), monitor)
            .await
            .expect("the tick panicked");
        assert!(ended.unwrap_err().is_panic());
        assert_eq!(calls.load(SeqCst), 2);
        assert_eq!(
            digest.group().rounds,
            12,
            "the ticks at 0 to 55 s counted, the one at 60 s did not"
        );
    }

    #[tokio::test]
    async fn rounds_move_every_tick_while_a_custom_script_check_sleeps_12_s() {
        let mut slow = target_every("slow", 1);
        slow.check = CheckSpec::CustomScript {
            command: "sleep 12".into(),
            timeout_ms: None,
        };
        let (infra, config) = held(vec![slow]);
        let digest = infra.lock().await.digest();
        // The monitor's own checker, counting the checks it starts and ends,
        // and reading `rounds` and the starts as the first check ends.
        let starts = Arc::new(AtomicU32::new(0));
        let ends = Arc::new(AtomicU32::new(0));
        let overlaps = Arc::new(AtomicU32::new(0));
        let at_first_end = Arc::new(std::sync::Mutex::new(None));
        let checker: Checker = {
            let (starts, ends, overlaps) = (starts.clone(), ends.clone(), overlaps.clone());
            let (digest, at_first_end) = (digest.clone(), at_first_end.clone());
            let real = run_checks();
            Arc::new(move |target: &InfraTarget, ctx: CheckContext| {
                if starts.fetch_add(1, SeqCst) > ends.load(SeqCst) {
                    overlaps.fetch_add(1, SeqCst);
                }
                let check = real(target, ctx);
                let (starts, ends) = (starts.clone(), ends.clone());
                let (digest, at_first_end) = (digest.clone(), at_first_end.clone());
                async move {
                    let result = check.await;
                    if ends.fetch_add(1, SeqCst) == 0 {
                        *at_first_end.lock().unwrap() =
                            Some((digest.group().rounds, starts.load(SeqCst)));
                    }
                    result
                }
                .boxed()
            })
        };
        let begun = std::time::Instant::now();
        let monitor = spawn_monitor_with(infra.clone(), config, Timing::default(), checker);

        // It ends near 12 s, and the tick at 15 s starts it again.
        wait_until(Duration::from_secs(30), || starts.load(SeqCst) >= 2).await;
        assert!(begun.elapsed() >= Duration::from_secs(12));
        let (rounds, started) = at_first_end.lock().unwrap().expect("the first check ended");
        assert!(
            rounds >= 3,
            "the ticks at 0, 5 and 10 s counted while it slept: {rounds}"
        );
        assert_eq!(started, 1, "due at every tick, but not started again");
        assert_eq!(overlaps.load(SeqCst), 0, "never started before it ended");
        let detail = infra.lock().await.results.targets["slow"].detail.clone();
        assert!(detail.starts_with("SCRIPT OK"), "{detail}");
        monitor.abort();
    }

    /// The set of checks in flight lives in the Infra state, so a second
    /// monitor (one a supervisor rebuilt while a check of the first still
    /// runs) never starts a target the first is checking.
    #[tokio::test(start_paused = true)]
    async fn a_second_monitor_never_starts_a_target_still_in_flight() {
        let (infra, config) = held(vec![target_every("t1", 5)]);
        let starts = Arc::new(AtomicU32::new(0));
        let running = Arc::new(AtomicU32::new(0));
        let most = Arc::new(AtomicU32::new(0));
        let checker: Checker = {
            let (starts, running, most) = (starts.clone(), running.clone(), most.clone());
            Arc::new(move |_: &InfraTarget, _: CheckContext| {
                starts.fetch_add(1, SeqCst);
                let (running, most) = (running.clone(), most.clone());
                async move {
                    let now = running.fetch_add(1, SeqCst) + 1;
                    most.fetch_max(now, SeqCst);
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    running.fetch_sub(1, SeqCst);
                    good()
                }
                .boxed()
            })
        };
        let first = spawn_monitor_with(
            infra.clone(),
            config.clone(),
            Timing::default(),
            checker.clone(),
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(starts.load(SeqCst), 1);

        let second = spawn_monitor_with(infra.clone(), config, Timing::default(), checker);
        tokio::time::sleep(Duration::from_secs(20)).await;
        assert_eq!(
            starts.load(SeqCst),
            1,
            "due at every tick of both, but in flight"
        );

        // The first check ends at 30 s; the next tick of either starts t1.
        tokio::time::sleep(Duration::from_secs(20)).await;
        assert_eq!(starts.load(SeqCst), 2);
        assert_eq!(most.load(SeqCst), 1, "never two checks of t1 at once");
        first.abort();
        second.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn at_most_three_checks_run_at_once_and_permits_go_in_order() {
        let ids = ["a", "b", "c", "d", "e"];
        let (infra, config) = held(ids.iter().map(|id| target_every(id, 3600)).collect());
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));
        let running = Arc::new(AtomicU32::new(0));
        let most = Arc::new(AtomicU32::new(0));
        let checker: Checker = {
            let (order, running, most) = (order.clone(), running.clone(), most.clone());
            Arc::new(move |target: &InfraTarget, _: CheckContext| {
                let id = target.id.clone();
                let (order, running, most) = (order.clone(), running.clone(), most.clone());
                async move {
                    // The check's body runs once it holds its permit.
                    order.lock().unwrap().push(id);
                    let now = running.fetch_add(1, SeqCst) + 1;
                    most.fetch_max(now, SeqCst);
                    tokio::time::sleep(Duration::from_mins(1)).await;
                    running.fetch_sub(1, SeqCst);
                    good()
                }
                .boxed()
            })
        };
        let monitor = spawn_monitor_with(infra.clone(), config, Timing::default(), checker);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(*order.lock().unwrap(), ["a", "b", "c"]);
        assert_eq!(infra.lock().await.in_flight.permit_times().len(), 3);
        tokio::time::sleep(Duration::from_mins(2)).await;
        assert_eq!(*order.lock().unwrap(), ids);
        assert_eq!(most.load(SeqCst), 3);
        monitor.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_panicking_check_reads_unknown_fires_no_recovery_and_rounds_go_on() {
        let mut boom = target_every("boom", 5);
        boom.recovery = Some(RecoveryConfig {
            enabled: true,
            command: "true".into(),
            cooldown_secs: 0,
            max_retries: 5,
            credential_ids: Vec::new(),
        });
        let (infra, config) = held(vec![boom]);
        // Down already: a failed check would fire the recovery.
        {
            let mut guard = infra.lock().await;
            let target = guard.results.targets.get_mut("boom").unwrap();
            target.status = TargetStatus::Down;
            target.consecutive_fail = 4;
            guard.changed();
        }
        let digest = infra.lock().await.digest();
        let seq = digest.group().seq;
        let checker: Checker = Arc::new(|_: &InfraTarget, _: CheckContext| {
            async { panic!("a defect in the check") }.boxed()
        });
        let monitor = spawn_monitor_with(infra.clone(), config, Timing::default(), checker);
        tokio::time::sleep(Duration::from_secs(31)).await;

        let guard = infra.lock().await;
        let target = &guard.results.targets["boom"];
        assert_eq!(target.status, TargetStatus::Unknown);
        assert_eq!(target.detail, CHECK_PANICKED);
        assert_eq!((target.consecutive_ok, target.consecutive_fail), (0, 0));
        assert!(guard.results.recovery_log.is_empty(), "no recovery ran");
        assert!(guard.recovery_tracker.is_empty());
        assert!(!guard.in_flight.contains("boom"), "its place is free again");
        drop(guard);
        assert_eq!(
            digest.group().seq,
            seq + 1,
            "down to unknown once; the panics after it change nothing"
        );
        assert!(digest.group().rounds >= 6, "every tick still counts");
        monitor.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn the_stall_hook_stops_rounds_within_one_tick() {
        let (infra, config) = held(vec![target_every("t1", 60)]);
        let digest = infra.lock().await.digest();
        let timing = Timing {
            stall_after: Some(Duration::from_secs(12)),
            ..Timing::default()
        };
        let monitor = spawn_monitor_with(infra.clone(), config, timing, answers_ok());
        tokio::time::sleep(Duration::from_secs(11)).await;
        assert_eq!(digest.group().rounds, 3, "the ticks at 0, 5 and 10 s");
        tokio::time::sleep(Duration::from_mins(2)).await;
        assert_eq!(digest.group().rounds, 3, "none from the tick at 15 s on");
        assert!(!monitor.is_finished(), "stuck, not gone");
        assert_eq!(digest.group().off, None, "a monitor is expected");
        monitor.abort();
    }

    #[test]
    fn the_stall_hook_reads_whole_seconds() {
        assert_eq!(stall_secs("45"), Some(Duration::from_secs(45)));
        assert_eq!(stall_secs(" 0 "), Some(Duration::ZERO));
        assert_eq!(stall_secs("1.5"), None);
        assert_eq!(stall_secs("soon"), None);
        assert_eq!(stall_secs(""), None);
    }

    #[tokio::test]
    async fn seq_moves_by_one_on_a_status_change_and_on_a_logged_run() {
        let mut t = target_every("t1", 60);
        t.recovery = Some(RecoveryConfig {
            enabled: true,
            command: "true".into(),
            cooldown_secs: 300,
            max_retries: 2,
            credential_ids: Vec::new(),
        });
        let scratch = Scratch::new("seq");
        let mut state = scratch.state();
        assert!(state.hold_config(InfraConfig {
            version: 1,
            check_interval_secs: 60,
            targets: vec![t.clone()],
        }));
        let start = state.stamp();
        let failed = || CheckResult::failed("PING TIMEOUT");

        // Unknown to up: one change.
        apply(&mut state, &t, good(), 1, Instant::now()).await;
        assert_eq!(state.stamp().seq, start.seq + 1);
        // A latency reading alone: none.
        let mut slower = good();
        slower.latency_ms = Some(9);
        apply(&mut state, &t, slower, 1, Instant::now()).await;
        assert_eq!(state.stamp().seq, start.seq + 1);
        // Up to degraded: one.
        apply(&mut state, &t, failed(), 1, Instant::now()).await;
        assert_eq!(state.stamp().seq, start.seq + 2);
        // Still degraded: none.
        apply(&mut state, &t, failed(), 1, Instant::now()).await;
        assert_eq!(state.stamp().seq, start.seq + 2);
        // Degraded to down, and the recovery runs and is logged: one each.
        apply(&mut state, &t, failed(), 1, Instant::now()).await;
        let down = state.stamp();
        assert_eq!(down.seq, start.seq + 4);
        assert_eq!(down.recoveries, 1);
        assert_eq!(state.results.recovery_log.len(), 1);
        assert_eq!(
            down.hash,
            picture_hash(&picture_text(down.boot, 1, [("t1", TargetStatus::Down)]))
        );
        // Still down inside the cooldown: no run, no change.
        apply(&mut state, &t, failed(), 1, Instant::now()).await;
        assert_eq!(state.stamp(), down);
    }

    /// `target` with a recovery that starts no process: it names a
    /// credential this unit does not hold, so each run is logged and
    /// counted, and nothing runs.
    fn with_inert_recovery(mut target: InfraTarget) -> InfraTarget {
        target.recovery = Some(RecoveryConfig {
            enabled: true,
            command: "true".into(),
            cooldown_secs: 60,
            max_retries: 2,
            credential_ids: vec!["not-on-this-unit".into()],
        });
        target
    }

    /// The state of `target` after an outage that used up its recovery:
    /// Down, two runs logged, its count at `max_retries`. `at` turns the
    /// seconds since the first check into the monotonic clock.
    async fn exhausted(
        scratch: &Scratch,
        target: &InfraTarget,
        at: impl Fn(u64) -> Instant,
    ) -> InfraState {
        let mut state = scratch.state();
        assert!(state.hold_config(InfraConfig {
            version: 1,
            check_interval_secs: 60,
            targets: vec![target.clone()],
        }));
        apply(&mut state, target, good(), 1, at(0)).await;
        // Degraded twice, then Down with the first run, the second run once
        // the cooldown has passed, then none.
        for secs in [60, 120, 180, 240, 300] {
            let failed = CheckResult::failed("PING TIMEOUT");
            apply(&mut state, target, failed, 1, at(secs)).await;
        }
        assert_eq!(state.results.targets[&target.id].status, TargetStatus::Down);
        assert_eq!(state.results.recovery_log.len(), 2);
        assert_eq!(
            state.recovery_tracker[&target.id].1, 2,
            "max_retries used up"
        );
        state
    }

    /// A check that panics while its target is Down leaves the target
    /// `unknown` and the recovery count standing. The good answer that
    /// takes it out of `unknown` clears the count (ADR-010 decision 9), so
    /// its next outage runs the recovery again.
    #[tokio::test]
    async fn a_good_answer_out_of_unknown_clears_the_recovery_count() {
        let t = with_inert_recovery(target_every("t1", 60));
        let t0 = Instant::now();
        let at = |secs| t0 + Duration::from_secs(secs);
        let scratch = Scratch::new("clears");
        let mut state = exhausted(&scratch, &t, at).await;

        apply_check_panic(&mut state, &t, 1);
        assert_eq!(state.results.targets["t1"].status, TargetStatus::Unknown);
        assert_eq!(
            state.recovery_tracker["t1"].1, 2,
            "the agent's own defect is no answer"
        );

        apply(&mut state, &t, good(), 1, at(360)).await;
        assert_eq!(state.results.targets["t1"].status, TargetStatus::Up);
        assert!(
            !state.recovery_tracker.contains_key("t1"),
            "it answered: the count is clear"
        );

        // The next outage, an hour later: Down, and the recovery runs.
        for secs in [3600, 3660, 3720] {
            let failed = CheckResult::failed("PING TIMEOUT");
            apply(&mut state, &t, failed, 1, at(secs)).await;
        }
        assert_eq!(state.results.targets["t1"].status, TargetStatus::Down);
        assert_eq!(state.results.recovery_log.len(), 3, "the recovery ran");
        assert_eq!(state.recovery_tracker["t1"].1, 1);
    }

    /// A failure out of `unknown` is no answer: the outage goes on, the
    /// count stands, and `max_retries` still holds.
    #[tokio::test]
    async fn a_failure_out_of_unknown_leaves_the_recovery_count() {
        let t = with_inert_recovery(target_every("t1", 60));
        let t0 = Instant::now();
        let at = |secs| t0 + Duration::from_secs(secs);
        let scratch = Scratch::new("stands");
        let mut state = exhausted(&scratch, &t, at).await;

        apply_check_panic(&mut state, &t, 1);
        for secs in [3600, 3660, 3720] {
            let failed = CheckResult::failed("PING TIMEOUT");
            apply(&mut state, &t, failed, 1, at(secs)).await;
        }
        assert_eq!(state.results.targets["t1"].status, TargetStatus::Down);
        assert_eq!(state.recovery_tracker["t1"].1, 2, "the count stands");
        assert_eq!(
            state.results.recovery_log.len(),
            2,
            "no run past max_retries"
        );
    }

    // ─── The monitor (ADR-010 decisions 8 to 10) ─────────────────────

    use tokio::sync::mpsc;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    /// The next `infra.state` the forwarder sends.
    async fn next_message(rx: &mut mpsc::Receiver<WsMessage>) -> serde_json::Value {
        let message = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("a message")
            .expect("the forwarder is running");
        match message {
            WsMessage::Text(text) => serde_json::from_str(&text).unwrap(),
            other => panic!("not text: {other:?}"),
        }
    }

    /// Nothing sent for longer than the forwarder's one second.
    async fn silent(rx: &mut mpsc::Receiver<WsMessage>) -> bool {
        tokio::time::timeout(Duration::from_millis(1500), rx.recv())
            .await
            .is_err()
    }

    fn answer(ok: bool, latency_ms: u64) -> CheckResult {
        if ok {
            CheckResult {
                ok: true,
                latency_ms: Some(latency_ms),
                detail: format!("OK {latency_ms}ms"),
                ..CheckResult::default()
            }
        } else {
            CheckResult::failed("TIMEOUT")
        }
    }

    /// A state in `scratch` holding `targets` at version 1.
    fn holding(scratch: &Scratch, targets: Vec<InfraTarget>) -> InfraState {
        let mut state = scratch.state();
        assert!(state.hold_config(InfraConfig {
            version: 1,
            check_interval_secs: 60,
            targets,
        }));
        state
    }

    #[test]
    fn the_state_machine_is_the_table() {
        use TargetStatus::{Degraded, Down, Unknown, Up};
        // The streak after the answer, with down_after 3 and up_after 2.
        let streak = |ok, fail, within| Streak { ok, fail, within };
        let next = |from, ok, within, streak| compute_status(from, ok, within, streak, 3, 2);

        // Unknown and Up: within is Up, slow or a failure is Degraded.
        assert_eq!(next(Unknown, true, true, streak(1, 0, 1)), Up);
        assert_eq!(next(Unknown, true, false, streak(1, 0, 0)), Degraded);
        assert_eq!(next(Unknown, false, false, streak(0, 1, 0)), Degraded);
        assert_eq!(next(Up, true, true, streak(7, 0, 7)), Up);
        assert_eq!(next(Up, true, false, streak(8, 0, 0)), Degraded);
        assert_eq!(next(Up, false, false, streak(0, 1, 0)), Degraded);

        // Degraded: Up at up_after answers within in a row, Down at
        // down_after failures in a row, Degraded otherwise.
        assert_eq!(next(Degraded, true, true, streak(1, 0, 1)), Degraded);
        assert_eq!(next(Degraded, true, true, streak(2, 0, 2)), Up);
        assert_eq!(
            next(Degraded, true, true, streak(9, 0, 1)),
            Degraded,
            "good answers before it, but slow"
        );
        assert_eq!(next(Degraded, true, false, streak(50, 0, 0)), Degraded);
        assert_eq!(next(Degraded, false, false, streak(0, 2, 0)), Degraded);
        assert_eq!(next(Degraded, false, false, streak(0, 3, 0)), Down);

        // Down: Down until up_after good answers in a row, then Up when all
        // were within, Degraded otherwise.
        assert_eq!(next(Down, true, true, streak(1, 0, 1)), Down);
        assert_eq!(
            next(Down, true, false, streak(1, 0, 0)),
            Down,
            "one slow answer leaves Down as Down"
        );
        assert_eq!(next(Down, true, true, streak(2, 0, 2)), Up);
        assert_eq!(
            next(Down, true, true, streak(2, 0, 1)),
            Degraded,
            "one slow before"
        );
        assert_eq!(
            next(Down, true, false, streak(2, 0, 0)),
            Degraded,
            "the last slow"
        );
        assert_eq!(next(Down, false, false, streak(0, 9, 0)), Down);
    }

    /// A camera answering steadily at 1.5 times its threshold was one flap
    /// and one push per check. Now it is one Degraded span: one change, one
    /// push, for 50 answers.
    #[tokio::test]
    async fn a_target_steady_at_one_and_a_half_times_its_threshold_stays_degraded() {
        let scratch = Scratch::new("slow");
        let cam = target_every("cam", 60);
        let infra = Arc::new(Mutex::new(holding(&scratch, vec![cam.clone()])));
        let (tx, mut rx) = mpsc::channel(16);
        let forwarder =
            crate::tunnel::infra_state::spawn_forwarder(true, Some(infra.clone()), tx).unwrap();
        assert_eq!(
            next_message(&mut rx).await["targets"]["cam"]["status"],
            "unknown"
        );
        let start = infra.lock().await.stamp();

        for _ in 0..50 {
            let mut state = infra.lock().await;
            apply(&mut state, &cam, answer(true, 300), 1, Instant::now()).await;
            assert_eq!(state.results.targets["cam"].status, TargetStatus::Degraded);
        }
        assert_eq!(infra.lock().await.stamp().seq, start.seq + 1, "one change");
        let pushed = next_message(&mut rx).await;
        assert_eq!(pushed["targets"]["cam"]["status"], "degraded");
        assert!(silent(&mut rx).await, "one push");
        assert!(
            pushed["targets"]["cam"].get("consecutive_within").is_none(),
            "the count stays inside the agent"
        );

        // up_after answers within the threshold bring it Up, not one.
        {
            let mut state = infra.lock().await;
            apply(&mut state, &cam, answer(true, 100), 1, Instant::now()).await;
            assert_eq!(state.results.targets["cam"].status, TargetStatus::Degraded);
            apply(&mut state, &cam, answer(true, 100), 1, Instant::now()).await;
            assert_eq!(state.results.targets["cam"].status, TargetStatus::Up);
        }
        assert_eq!(
            next_message(&mut rx).await["targets"]["cam"]["status"],
            "up"
        );
        forwarder.abort();
    }

    #[tokio::test]
    async fn a_slow_answer_never_makes_a_target_up() {
        let scratch = Scratch::new("never-up");
        let t = target_every("t1", 60);
        let mut state = holding(&scratch, vec![t.clone()]);
        let status = |state: &InfraState| state.results.targets["t1"].status;

        // A slow first answer takes Unknown to Degraded.
        apply(&mut state, &t, answer(true, 300), 1, Instant::now()).await;
        assert_eq!(status(&state), TargetStatus::Degraded);

        // From Down: one slow good answer leaves it Down.
        for _ in 0..3 {
            apply(&mut state, &t, answer(false, 0), 1, Instant::now()).await;
        }
        assert_eq!(status(&state), TargetStatus::Down);
        apply(&mut state, &t, answer(true, 300), 1, Instant::now()).await;
        assert_eq!(status(&state), TargetStatus::Down);
        // up_after good answers with one slow take it to Degraded.
        apply(&mut state, &t, answer(true, 100), 1, Instant::now()).await;
        assert_eq!(status(&state), TargetStatus::Degraded);

        // The slow one last: Degraded too.
        for _ in 0..3 {
            apply(&mut state, &t, answer(false, 0), 1, Instant::now()).await;
        }
        assert_eq!(status(&state), TargetStatus::Down);
        apply(&mut state, &t, answer(true, 100), 1, Instant::now()).await;
        assert_eq!(status(&state), TargetStatus::Down);
        apply(&mut state, &t, answer(true, 300), 1, Instant::now()).await;
        assert_eq!(status(&state), TargetStatus::Degraded);

        // All within: Up.
        for _ in 0..3 {
            apply(&mut state, &t, answer(false, 0), 1, Instant::now()).await;
        }
        apply(&mut state, &t, answer(true, 100), 1, Instant::now()).await;
        apply(&mut state, &t, answer(true, 100), 1, Instant::now()).await;
        assert_eq!(status(&state), TargetStatus::Up);
    }

    /// A target with an inert recovery whose cooldown is `cooldown_secs`.
    fn pdu(cooldown_secs: u64) -> InfraTarget {
        let mut t = with_inert_recovery(target_every("pdu", 60));
        if let Some(recovery) = t.recovery.as_mut() {
            recovery.cooldown_secs = cooldown_secs;
            recovery.max_retries = 5;
        }
        t
    }

    /// A good answer while Down is a device coming back: it fires no
    /// recovery, whatever the cooldown says. A failed check of the monitor
    /// past the cooldown does; Check now's never does.
    #[tokio::test]
    async fn only_a_failed_check_of_the_monitor_fires_a_recovery() {
        let scratch = Scratch::new("fires");
        let t = pdu(60);
        let mut state = holding(&scratch, vec![t.clone()]);
        let t0 = Instant::now();
        let at = |secs| t0 + Duration::from_secs(secs);
        let fold = |state: &mut InfraState, ok: bool, secs: u64, reading: Reading| {
            apply_result(state, &t, answer(ok, 300), 1, at(secs), reading)
        };

        assert!(fold(&mut state, false, 0, Reading::Monitor).is_none());
        assert!(fold(&mut state, false, 10, Reading::Monitor).is_none());
        let first = fold(&mut state, false, 20, Reading::Monitor).expect("the first Down");
        run_inline(&mut state, first).await;
        assert_eq!(state.results.targets["pdu"].status, TargetStatus::Down);

        // Past the cooldown, a good answer that leaves it Down: no run.
        assert!(fold(&mut state, true, 100, Reading::Monitor).is_none());
        assert_eq!(state.results.targets["pdu"].status, TargetStatus::Down);
        assert_eq!(state.recovery_tracker["pdu"].1, 1);
        // Check now's failure past the cooldown: no run either.
        assert!(fold(&mut state, false, 110, Reading::CheckNow).is_none());
        // The monitor's failure past the cooldown: a run.
        let second = fold(&mut state, false, 120, Reading::Monitor).expect("past the cooldown");
        run_inline(&mut state, second).await;
        assert_eq!(state.recovery_tracker["pdu"].1, 2);
        assert_eq!(state.results.recovery_log.len(), 2);
    }

    /// One run per target at a time: a Down check while its run is in
    /// progress starts no second run, even with no cooldown.
    #[tokio::test]
    async fn a_down_check_while_a_run_is_in_progress_starts_no_second_run() {
        let scratch = Scratch::new("one-run");
        let t = pdu(0);
        let mut state = holding(&scratch, vec![t.clone()]);
        let failed = |state: &mut InfraState| {
            apply_result(
                state,
                &t,
                answer(false, 0),
                1,
                Instant::now(),
                Reading::Monitor,
            )
        };
        for _ in 0..2 {
            assert!(failed(&mut state).is_none());
        }
        let run = failed(&mut state).expect("Down: the first run");
        assert!(state.runs().contains("pdu"));
        assert_eq!(state.runs_not_ended["pdu"].command, "true");
        assert!(failed(&mut state).is_none(), "its run is in progress");
        assert_eq!(state.recovery_tracker["pdu"].1, 1);

        run_inline(&mut state, run).await;
        assert!(!state.runs().contains("pdu"));
        assert!(state.runs_not_ended.is_empty(), "logged: ended");
        assert!(failed(&mut state).is_some(), "the next one may run");
    }

    /// Check now during a recovery run stays a reading: its answers move
    /// the target's record, its status included, while the count and the
    /// cooldown stand until the run ends. The run's outcome then applies
    /// them: a target that reads Up came back during the run and starts
    /// afresh; one that reads Down keeps the count and the cooldown the run
    /// began.
    #[tokio::test]
    async fn during_a_run_check_now_reads_and_the_run_applies_the_count_at_its_end() {
        let scratch = Scratch::new("during-run");
        let t = pdu(60);
        let mut state = holding(&scratch, vec![t.clone()]);
        let t0 = Instant::now();
        let at = |secs| t0 + Duration::from_secs(secs);
        let fold = |state: &mut InfraState, ok: bool, secs: u64, reading: Reading| {
            apply_result(state, &t, answer(ok, 3), 1, at(secs), reading)
        };

        // Down, and a run starts.
        assert!(fold(&mut state, false, 0, Reading::Monitor).is_none());
        assert!(fold(&mut state, false, 10, Reading::Monitor).is_none());
        let run = fold(&mut state, false, 20, Reading::Monitor).expect("Down: a run");
        let begun = state.recovery_tracker["pdu"];
        assert_eq!(begun, (at(20), 1));

        // Two good readings during the run bring it Up, a change pushed,
        // and leave the count and the cooldown as the run began them.
        let seq = state.stamp().seq;
        assert!(fold(&mut state, true, 25, Reading::CheckNow).is_none());
        assert!(fold(&mut state, true, 26, Reading::CheckNow).is_none());
        let shown = &state.results.targets["pdu"];
        assert_eq!((shown.status, shown.consecutive_ok), (TargetStatus::Up, 2));
        assert_eq!(state.stamp().seq, seq + 1, "Down to Up, pushed");
        assert_eq!(state.recovery_tracker["pdu"], begun, "they stand");

        // The run ends: the target came back, so it starts afresh.
        run_inline(&mut state, run).await;
        assert!(!state.recovery_tracker.contains_key("pdu"));
        assert_eq!(state.results.recovery_log.len(), 1);
        assert_eq!(
            scratch.saved(),
            serde_json::Value::Null,
            "no count left to save"
        );

        // The next outage: Down, a run, a good reading and a failed one
        // during it. It ends Down: the count and the cooldown stand.
        for secs in [100, 110] {
            assert!(fold(&mut state, false, secs, Reading::Monitor).is_none());
        }
        let run = fold(&mut state, false, 120, Reading::Monitor).expect("Down again: a run");
        assert!(fold(&mut state, true, 125, Reading::CheckNow).is_none());
        assert!(fold(&mut state, false, 126, Reading::CheckNow).is_none());
        run_inline(&mut state, run).await;
        assert_eq!(state.results.targets["pdu"].status, TargetStatus::Down);
        assert_eq!(state.recovery_tracker["pdu"], (at(120), 1));
        assert!(
            fold(&mut state, false, 150, Reading::Monitor).is_none(),
            "within the cooldown the run began"
        );
        assert!(fold(&mut state, false, 180, Reading::Monitor).is_some());
    }

    /// What a recovery wrote, as its log entry keeps it.
    async fn logged_output(scratch: &Scratch, command: String) -> RecoveryLogEntry {
        let mut t = pdu(0);
        if let Some(recovery) = t.recovery.as_mut() {
            recovery.command = command;
            recovery.credential_ids = vec!["pdu-login".into()];
        }
        let mut state = holding(scratch, vec![t.clone()]);
        state.credentials.insert(
            "pdu-login".into(),
            super::super::Credential {
                username: "admin".into(),
                password: "S3cret-pw".into(),
            },
        );
        let run = (0..5)
            .find_map(|_| {
                apply_result(
                    &mut state,
                    &t,
                    answer(false, 0),
                    1,
                    Instant::now(),
                    Reading::Monitor,
                )
            })
            .expect("Down within five failures");
        run_inline(&mut state, run).await;
        state.results.recovery_log.pop().expect("logged")
    }

    /// 600 bytes of three-byte characters: byte 512 falls inside one, and a
    /// slice there panicked before the run was logged or counted, so the
    /// recovery ran again at every Down check.
    #[tokio::test]
    async fn a_recoverys_output_is_cut_on_a_character_boundary() {
        let scratch = Scratch::new("cut");
        let euros = "€".repeat(200);
        assert_eq!(euros.len(), 600);
        // printf only, with fixed text.
        let entry = logged_output(
            &scratch,
            format!("printf '%s' '{euros}'; printf '%s' '{euros}' >&2; exit 3"),
        )
        .await;
        assert_eq!(entry.exit_code, 3);
        for (stream, text) in [("stdout", &entry.stdout), ("stderr", &entry.stderr)] {
            assert_eq!(text.len(), 510 + 3, "{stream}: 170 characters and `...`");
            assert_eq!(
                text.strip_suffix("...").unwrap(),
                "€".repeat(170),
                "{stream}"
            );
        }
    }

    /// stdout and stderr are masked before they are cut, so a cut never
    /// leaves part of a password. stderr gets stdout's mask: a shell trace
    /// writes expanded commands there.
    #[tokio::test]
    async fn a_recoverys_stdout_and_stderr_are_masked_before_the_cut() {
        let scratch = Scratch::new("mask");
        let pad = "x".repeat(508);
        // The password's bytes would straddle byte 512 if it were cut first.
        let entry = logged_output(
            &scratch,
            format!(
                "printf '%s%s' '{pad}' \"$SCTL_CRED_PASS_1\"; \
                 printf '%s%s' '{pad}' \"$SCTL_CRED_PASS_1\" >&2; printf '%s' '{pad}' >&2"
            ),
        )
        .await;
        assert_eq!(entry.stdout, format!("{pad}***"));
        assert_eq!(entry.stderr, format!("{pad}***x..."));
        for text in [&entry.stdout, &entry.stderr] {
            assert!(!text.contains("S3c"), "{text}");
        }
    }

    /// A verbose HTTP client (`curl -v -u "$SCTL_CRED_USER_1:$SCTL_CRED_PASS_1"`)
    /// writes its request's headers to stderr, the Basic credential among
    /// them: the base64 of `admin:S3cret-pw`, which holds no password as
    /// written. It is masked too. The command only prints fixed text.
    #[tokio::test]
    async fn a_recoverys_output_is_masked_of_the_http_basic_form_too() {
        let scratch = Scratch::new("basic");
        let entry = logged_output(
            &scratch,
            "printf '%s\\n' '> GET /outlet?1=off HTTP/1.1' \
             '> Authorization: Basic YWRtaW46UzNjcmV0LXB3' >&2"
                .to_string(),
        )
        .await;
        assert_eq!(
            entry.stderr,
            "> GET /outlet?1=off HTTP/1.1\n> Authorization: Basic ***\n"
        );
    }

    #[test]
    fn the_masks_are_each_password_and_its_basic_form_longest_first() {
        let credential = |username: &str, password: &str| super::super::Credential {
            username: username.into(),
            password: password.into(),
        };
        let pdu = [credential("admin", "S3cret-pw")];
        assert_eq!(masks(&pdu), ["YWRtaW46UzNjcmV0LXB3", "S3cret-pw"]);

        // A password that holds a shorter one is masked whole, whatever
        // the order of the credentials.
        let nested = [credential("a", "pass"), credential("b", "password1")];
        assert_eq!(
            mask("password1 pass YjpwYXNzd29yZDE= YTpwYXNz", &masks(&nested)),
            "*** *** *** ***"
        );

        // A password under 4 bytes and a username stay as they are, in
        // either form.
        assert!(masks(&[credential("admin", "abc")]).is_empty());
        assert_eq!(
            mask("admin S3cret-pw", &masks(&pdu)),
            "admin ***",
            "the username stays"
        );
        // The same credential twice is one mask each.
        assert_eq!(masks(&[pdu[0].clone(), pdu[0].clone()]).len(), 2);
    }

    /// A unit whose recovery cuts its own power restarts the agent. The
    /// saved count stands, a full cooldown runs from the start, and a run
    /// the agent did not see end is logged with its outcome unknown.
    #[tokio::test]
    async fn at_start_a_run_not_ended_is_logged_and_the_count_stands() {
        let scratch = Scratch::new("restart");
        let config = InfraConfig {
            version: 7,
            check_interval_secs: 60,
            targets: vec![pdu(300), target_every("cam", 60)],
        };
        std::fs::write(
            scratch.0.join("infra-monitor.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        let saved = json!({
            "counts": {"pdu": 1, "gone": 2},
            "running": {"pdu": {"started_ms": 1_790_000_000_000_u64, "command": "true"}},
        });
        std::fs::write(scratch.0.join("infra-recovery.json"), saved.to_string()).unwrap();

        let mut state = scratch.state();
        state.load_config();
        let loaded = Instant::now();
        state.load_recoveries();

        let entry = &state.results.recovery_log[0];
        assert_eq!(state.results.recovery_log.len(), 1);
        assert_eq!((entry.target_id.as_str(), entry.exit_code), ("pdu", -1));
        assert_eq!(
            entry.stdout,
            "OUTCOME UNKNOWN: the agent stopped before this run ended (it started at 2026-09-21T14:13:20Z)"
        );
        let stamp = state.stamp();
        assert_eq!(
            (stamp.seq, stamp.recoveries),
            (2, 1),
            "a logged run moves both"
        );
        assert_eq!(
            scratch.saved(),
            json!({"counts": {"pdu": 1}, "running": {}}),
            "logged now, and a target the config lacks loses its count"
        );

        // The count stands, and the cooldown runs from the start.
        let t = &config.targets[0];
        assert!(state.recovery_tracker["pdu"].0 >= loaded);
        let at = |secs| loaded + Duration::from_secs(secs);
        for secs in [10, 20, 30, 299] {
            let run = apply_result(
                &mut state,
                t,
                answer(false, 0),
                7,
                at(secs),
                Reading::Monitor,
            );
            assert!(run.is_none(), "cooling down at {secs}s");
        }
        assert_eq!(state.results.targets["pdu"].status, TargetStatus::Down);
        let run = apply_result(
            &mut state,
            t,
            answer(false, 0),
            7,
            at(301),
            Reading::Monitor,
        );
        assert!(run.is_some(), "a full cooldown after the start");
        assert_eq!(state.recovery_tracker["pdu"].1, 2, "the count stood");
    }

    #[tokio::test]
    async fn the_first_good_answer_after_a_start_clears_the_saved_count() {
        let scratch = Scratch::new("answered");
        let t = pdu(300);
        let mut state = holding(&scratch, vec![t.clone()]);
        std::fs::write(
            scratch.0.join("infra-recovery.json"),
            json!({"counts": {"pdu": 2}}).to_string(),
        )
        .unwrap();
        state.load_recoveries();
        assert_eq!(state.recovery_tracker["pdu"].1, 2);
        assert_eq!(state.results.targets["pdu"].status, TargetStatus::Unknown);

        apply(&mut state, &t, answer(true, 3), 1, Instant::now()).await;
        assert!(state.recovery_tracker.is_empty(), "out of unknown: afresh");
        // The clear hands its save to the blocking pool: nothing is left to
        // save, so the file goes.
        wait_until(Duration::from_secs(5), || {
            scratch.saved() == serde_json::Value::Null
        })
        .await;
    }

    /// A synced write to a unit's flash can take hundreds of milliseconds,
    /// and a one-core unit has one async worker. A run's record is written
    /// on the blocking pool, outside the Infra lock: while its write is
    /// held, here inside the write as a slow disk would hold it, the lock
    /// is free and the worker goes on, and the command waits for the
    /// record. A FIFO in place of the file cannot hold it: a FIFO takes no
    /// sync, so the write fails, and a run whose record did not reach the
    /// disk starts no command.
    #[tokio::test]
    async fn a_runs_record_is_written_outside_the_lock_and_off_the_async_worker() {
        let scratch = Scratch::new("slow-disk");
        let mut t = target_every("pdu", 60);
        t.recovery = Some(RecoveryConfig {
            enabled: true,
            command: "true".into(),
            cooldown_secs: 0,
            max_retries: 5,
            credential_ids: Vec::new(),
        });
        let mut state = holding(&scratch, vec![t.clone()]);
        let run = (0..5)
            .find_map(|_| {
                apply_result(
                    &mut state,
                    &t,
                    answer(false, 0),
                    1,
                    Instant::now(),
                    Reading::Monitor,
                )
            })
            .expect("Down within five failures");
        // The write waits inside itself until the test lets it through, or
        // 10 s whatever happens, so a write that holds the worker fails the
        // test instead of hanging it.
        let release = state.recovery_file.hold_next_write();
        let infra = Arc::new(Mutex::new(state));

        let asked = std::time::Instant::now();
        let task = tokio::spawn(run_recovery(infra.clone(), run));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            asked.elapsed() < Duration::from_secs(1),
            "the async worker waited for the disk: {:?}",
            asked.elapsed()
        );
        assert!(infra.try_lock().is_ok(), "the Infra lock is free");
        assert!(
            infra.lock().await.results.recovery_log.is_empty(),
            "the command waits for its record"
        );
        assert!(!task.is_finished());

        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("the run ends once its write does")
            .unwrap();
        let log = infra.lock().await.results.recovery_log.clone();
        assert_eq!((log.len(), log[0].exit_code), (1, 0), "it ran, once");
        assert_eq!(
            scratch.saved(),
            json!({"counts": {"pdu": 1}, "running": {}}),
            "saved as ended"
        );
    }

    /// A run whose record did not reach the disk does not run: that record
    /// is what bounds a recovery that power-cycles the unit's own supply
    /// across the restarts it causes. No command starts, the run is logged
    /// with exit -1 saying so, and it counts as an attempt: no run starts
    /// within the cooldown, and none past `max_retries`.
    #[tokio::test]
    async fn a_run_whose_record_did_not_reach_the_disk_does_not_run() {
        let scratch = Scratch::new("unrecorded");
        // A directory where the record goes: no write of it reaches the
        // disk.
        let record = scratch.0.join("infra-recovery.json");
        std::fs::create_dir(&record).unwrap();
        let mut t = target_every("pdu", 60);
        t.down_after_consecutive = 1;
        t.recovery = Some(RecoveryConfig {
            enabled: true,
            // Its output would be in the log, were it run.
            command: "echo ran".into(),
            cooldown_secs: 60,
            max_retries: 2,
            credential_ids: Vec::new(),
        });
        let infra = Arc::new(Mutex::new(holding(&scratch, vec![t.clone()])));
        let digest = infra.lock().await.digest();
        let t0 = Instant::now();

        // Degraded, then Down with a run; one within the cooldown; one past
        // it; then the count is spent.
        let mut started_at = Vec::new();
        for secs in [0, 10, 20, 70, 80, 140] {
            let started = {
                let mut state = infra.lock().await;
                let now = t0 + Duration::from_secs(secs);
                apply_result(&mut state, &t, answer(false, 0), 1, now, Reading::Monitor)
            };
            if let Some(run) = started {
                started_at.push(secs);
                run_recovery(infra.clone(), run).await;
            }
        }
        assert_eq!(
            started_at,
            [10, 70],
            "attempts, within the cooldown and max_retries"
        );

        let state = infra.lock().await;
        let log = &state.results.recovery_log;
        assert_eq!(log.len(), 2);
        for entry in log {
            assert_eq!(
                (
                    entry.command.as_str(),
                    entry.exit_code,
                    entry.stdout.as_str()
                ),
                ("echo ran", -1, NOT_RECORDED),
                "no command started"
            );
            assert!(entry.stderr.is_empty());
        }
        assert_eq!(
            state.recovery_tracker["pdu"],
            (t0 + Duration::from_secs(70), 2),
            "each counted, its cooldown begun"
        );
        assert!(state.runs_not_ended.is_empty(), "logged: ended");
        assert_eq!(state.runs().in_progress(), 0);
        assert_eq!(digest.stamp().recoveries, 2, "each logged run moves it");
        assert!(record.is_dir(), "nothing reached the disk");
    }

    #[tokio::test]
    async fn a_target_a_push_drops_loses_its_saved_count() {
        let scratch = Scratch::new("dropped");
        let mut state = holding(&scratch, vec![pdu(300), target_every("cam", 60)]);
        std::fs::write(
            scratch.0.join("infra-recovery.json"),
            json!({"counts": {"pdu": 2, "cam": 1}}).to_string(),
        )
        .unwrap();
        state.load_recoveries();
        assert_eq!(state.recovery_tracker.len(), 2);

        assert!(state.hold_config(InfraConfig {
            version: 2,
            check_interval_secs: 60,
            targets: vec![target_every("cam", 60)],
        }));
        assert!(!state.recovery_tracker.contains_key("pdu"));
        assert_eq!(
            scratch.saved(),
            json!({"counts": {"cam": 1}, "running": {}})
        );

        // A removed config clears every count.
        assert!(state.drop_config());
        assert!(state.recovery_tracker.is_empty());
        assert_eq!(scratch.saved(), serde_json::Value::Null);
    }

    /// A result is applied only while the unit holds the config the check
    /// ran under, the target in it: a monitor replaced during its check, or
    /// a Check now whose config changed, is answered and applies nothing.
    #[test]
    fn a_result_for_a_config_no_longer_held_is_answered_and_not_applied() {
        let scratch = Scratch::new("stale");
        let t = pdu(0);
        let mut state = holding(&scratch, vec![t.clone()]);
        let job = Job {
            target: t.clone(),
            config_version: 1,
            generation: state.config_generation,
            deadline: CHECK_DEADLINE,
        };
        assert!(state.hold_config(InfraConfig {
            version: 2,
            check_interval_secs: 60,
            targets: vec![t.clone()],
        }));
        let seq = state.stamp().seq;
        for reading in [Reading::Monitor, Reading::CheckNow] {
            let (result, run) = settle(&mut state, &job, Outcome::Answered(good()), reading);
            assert!(result.ok, "answered");
            assert!(run.is_none());
            let (result, _) = settle(&mut state, &job, Outcome::Panicked("x".into()), reading);
            assert_eq!(result.detail, CHECK_PANICKED);
        }
        let target = &state.results.targets["pdu"];
        assert_eq!(target.status, TargetStatus::Unknown);
        assert!(target.last_check.is_empty(), "nothing applied");
        assert_eq!(state.results.config_version, 2);
        assert_eq!(state.stamp().seq, seq);

        // The config it ran under: applied.
        let held = Job {
            generation: state.config_generation,
            config_version: 2,
            ..job
        };
        let (_, run) = settle(&mut state, &held, Outcome::TimedOut, Reading::Monitor);
        assert!(run.is_none());
        let target = &state.results.targets["pdu"];
        assert_eq!(target.status, TargetStatus::Degraded);
        assert_eq!(target.detail, "CHECK DEADLINE: no answer within 90s");
    }
}
