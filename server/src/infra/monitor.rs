//! Monitoring loop: a long-lived tokio task per config.
//!
//! Spawned when a config is pushed or loaded from disk at startup, aborted
//! and spawned again on a config change.
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
//! Due times and cooldowns use the monotonic clock, so a step of the wall
//! clock cannot stop the checks. A check that panics leaves its target
//! `unknown`, saying the agent failed to check it, and never fires a
//! recovery.
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
use tokio::task::JoinSet;
use tokio::time::{Instant, MissedTickBehavior};
use tracing::{debug, error, info, warn};

use super::checks::{self, CheckContext, CheckResult};
use super::{
    now_iso, CheckSpec, DataSample, Flight, InfraConfig, InfraState, InfraTarget, RecoveryLogEntry,
    TargetState, TargetStatus,
};

/// How many checks may run at once. Small: these devices have one core and
/// the checks fork processes.
const CHECK_CONCURRENCY: usize = 3;
/// How often the monitor looks for due targets and counts a round.
const TICK: Duration = Duration::from_secs(5);
/// Hard ceiling on one check, whatever its own timeouts say.
const CHECK_DEADLINE: Duration = Duration::from_secs(90);
/// The bench's stall hook: the loop stops ticking this many seconds after
/// it starts.
const STALL_ENV: &str = "SCTL_INFRA_TEST_STALL_SECS";
/// The detail of a target whose check panicked.
const CHECK_PANICKED: &str =
    "AGENT ERROR: the agent failed to check this target (its check panicked; see the agent's log)";

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

/// Spawn the infra monitoring loop. Returns a `JoinHandle` that the caller
/// should store so it can be aborted on config change or shutdown.
pub fn spawn_monitor(
    infra_state: Arc<Mutex<InfraState>>,
    config: InfraConfig,
) -> tokio::task::JoinHandle<()> {
    spawn_monitor_with(infra_state, config, Timing::from_env(), run_checks())
}

/// [`spawn_monitor`] with its clock and its checker given.
pub(crate) fn spawn_monitor_with(
    infra: Arc<Mutex<InfraState>>,
    config: InfraConfig,
    timing: Timing,
    checker: Checker,
) -> tokio::task::JoinHandle<()> {
    let monitor = Monitor {
        infra,
        config: Arc::new(config),
        timing,
        checker,
        limiter: Arc::new(Semaphore::new(CHECK_CONCURRENCY)),
        last_start: HashMap::new(),
        checks: JoinSet::new(),
    };
    tokio::spawn(monitor.run())
}

/// One monitor: its config, its clock, and the checks it started. Its
/// checks end with it: aborting the monitor aborts them, and each frees its
/// target as it goes.
struct Monitor {
    infra: Arc<Mutex<InfraState>>,
    config: Arc<InfraConfig>,
    timing: Timing,
    checker: Checker,
    limiter: Arc<Semaphore>,
    /// When each target's last check was started, on the tick's schedule.
    last_start: HashMap<String, Instant>,
    checks: JoinSet<()>,
}

impl Monitor {
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
            let Some(flight) = guard.in_flight.start(&target.id) else {
                // Still being checked: due again at the next tick.
                debug!(
                    "Infra: {} is still being checked; not started again",
                    target.id
                );
                continue;
            };
            self.last_start.insert(target.id.clone(), at);
            let check = (self.checker)(target, context_for(&guard, target));
            self.checks.spawn(run_check(
                self.infra.clone(),
                target.clone(),
                self.config.version,
                check,
                flight,
                self.limiter.clone(),
                self.timing.deadline,
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

/// One check, from its permit to its result applied. It holds its target's
/// place in flight until the result is in, and its permit only while the
/// check runs.
async fn run_check(
    infra: Arc<Mutex<InfraState>>,
    target: InfraTarget,
    config_version: u32,
    check: BoxFuture<'static, CheckResult>,
    flight: Flight,
    limiter: Arc<Semaphore>,
    deadline: Duration,
) {
    let Ok(permit) = limiter.acquire_owned().await else {
        return;
    };
    flight.took_permit(Instant::now());
    debug!("Checking target {}: {}", target.id, target.name);
    let outcome = tokio::time::timeout(deadline, AssertUnwindSafe(check).catch_unwind()).await;
    drop(permit);
    // No longer running: the round rule stops reading its permit time.
    flight.left_permit();
    let mut guard = infra.lock().await;
    match outcome {
        Ok(Ok(result)) => {
            apply_result(&mut guard, &target, result, config_version, Instant::now()).await;
        }
        Ok(Err(panic)) => {
            error!(
                "Infra: the check of {} ({}) panicked: {}",
                target.id,
                target.name,
                panic_text(panic.as_ref())
            );
            apply_check_panic(&mut guard, &target, config_version);
        }
        Err(_) => {
            let result = CheckResult::failed(format!(
                "CHECK DEADLINE: no answer within {}s",
                deadline.as_secs()
            ));
            apply_result(&mut guard, &target, result, config_version, Instant::now()).await;
        }
    }
    // Out of flight only once its result is in.
    drop(flight);
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

/// Fold one check result into the target's state, run the state machine,
/// fire recovery, keep the session and the data history. `now` is the
/// monotonic clock, which the recovery's cooldown reads.
pub async fn apply_result(
    state: &mut InfraState,
    target: &InfraTarget,
    result: CheckResult,
    config_version: u32,
    now: Instant,
) {
    let current = state.results.targets.get(&target.id);
    let shown = current.is_some();
    let prev_status = current.map_or(TargetStatus::Unknown, |t| t.status);
    let version_before = state.results.config_version;
    let new_status = compute_status(
        prev_status,
        &result,
        target.degraded_threshold_ms,
        target.down_after_consecutive,
        target.up_after_consecutive,
        current,
    );

    let since = if new_status == prev_status {
        current.map_or_else(now_iso, |t| t.since.clone())
    } else {
        now_iso()
    };

    let (consecutive_ok, consecutive_fail) = if result.ok {
        let prev_ok = current.map_or(0, |t| t.consecutive_ok);
        (prev_ok.saturating_add(1), 0)
    } else {
        let prev_fail = current.map_or(0, |t| t.consecutive_fail);
        (0, prev_fail.saturating_add(1))
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
            consecutive_ok,
            consecutive_fail,
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
    // recovery: a status, or the version and the target set when the result
    // is for a config the results no longer show. A check that leaves them
    // alone (a latency reading, a counter) is not one.
    if new_status != prev_status || !shown || version_before != config_version {
        state.changed();
    }

    // Recovery action: fire on any DOWN state (new transition or sustained)
    if new_status == TargetStatus::Down {
        if let Some(ref recovery) = target.recovery {
            if recovery.enabled {
                try_recovery(state, &target.id, recovery, now).await;
            }
        }
    }

    // Reset recovery tracker on recovery
    if new_status == TargetStatus::Up && prev_status == TargetStatus::Down {
        state.recovery_tracker.remove(&target.id);
        info!("Target {} ({}) recovered to UP", target.id, target.name);
    }
}

/// Compute the new status based on the state machine rules.
fn compute_status(
    prev: TargetStatus,
    result: &CheckResult,
    degraded_threshold_ms: u64,
    down_after: u32,
    up_after: u32,
    current_state: Option<&TargetState>,
) -> TargetStatus {
    let consecutive_fail = current_state.map_or(0, |s| s.consecutive_fail);
    let consecutive_ok = current_state.map_or(0, |s| s.consecutive_ok);

    match prev {
        TargetStatus::Unknown => {
            if result.ok {
                TargetStatus::Up
            } else {
                TargetStatus::Degraded
            }
        }
        TargetStatus::Up => {
            if !result.ok {
                return TargetStatus::Degraded;
            }
            if result
                .latency_ms
                .is_some_and(|ms| ms > degraded_threshold_ms)
            {
                TargetStatus::Degraded
            } else {
                TargetStatus::Up
            }
        }
        TargetStatus::Degraded => {
            if result.ok && consecutive_ok + 1 >= up_after {
                TargetStatus::Up
            } else if !result.ok && consecutive_fail + 1 >= down_after {
                TargetStatus::Down
            } else {
                TargetStatus::Degraded
            }
        }
        TargetStatus::Down => {
            if result.ok && consecutive_ok + 1 >= up_after {
                TargetStatus::Up
            } else {
                TargetStatus::Down
            }
        }
    }
}

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

/// A recovery's output with its passwords masked, before it is logged or kept.
fn mask(text: &str, secrets: &[String]) -> String {
    secrets
        .iter()
        .filter(|s| s.len() >= 4)
        .fold(text.to_string(), |acc, s| acc.replace(s.as_str(), "***"))
}

/// Attempt to execute a recovery action (respecting cooldown and max
/// retries). `now` is the monotonic clock.
async fn try_recovery(
    state: &mut InfraState,
    target_id: &str,
    recovery: &super::RecoveryConfig,
    now: Instant,
) {
    let (last_run, count) = state
        .recovery_tracker
        .get(target_id)
        .map_or((None, 0), |&(at, count)| (Some(at), count));

    // Check max retries
    if count >= recovery.max_retries {
        debug!(
            "Recovery for {target_id}: exhausted ({count}/{} retries)",
            recovery.max_retries
        );
        return;
    }

    // Check cooldown
    if let Some(last) = last_run {
        let since = now.saturating_duration_since(last);
        if since < Duration::from_secs(recovery.cooldown_secs) {
            debug!(
                "Recovery for {target_id}: cooling down ({}s of {}s)",
                since.as_secs(),
                recovery.cooldown_secs
            );
            return;
        }
    }

    info!("Executing recovery for {target_id}: {}", recovery.command);

    // Run the recovery command (5-minute hard timeout), with the credentials
    // it names as environment; a credential this unit does not hold is a
    // failed run, never a command run without it.
    let (exit_code, stdout) = match recovery_env(&state.credentials, &recovery.credential_ids) {
        Err(missing) => (
            -1,
            format!("ERROR: credential {missing} is not on this unit"),
        ),
        Ok(env) => {
            let secrets: Vec<String> = env
                .iter()
                .filter(|(k, _)| k.starts_with("SCTL_CRED_PASS_"))
                .map(|(_, v)| v.clone())
                .collect();
            match super::checks::exec_with_env(&recovery.command, &env, 300_000).await {
                Ok((exit, out, _err)) => (exit, mask(&out, &secrets)),
                Err(e) => (-1, format!("ERROR: {e}")),
            }
        }
    };

    // Truncate stdout for the log
    let stdout_trunc = if stdout.len() > 512 {
        format!("{}...", &stdout[..512])
    } else {
        stdout
    };

    info!("Recovery for {target_id}: exit={exit_code}, output={stdout_trunc}");

    state.push_recovery_log(RecoveryLogEntry {
        ts: now_iso(),
        target_id: target_id.to_string(),
        command: recovery.command.clone(),
        exit_code,
        stdout: stdout_trunc,
    });

    state
        .recovery_tracker
        .insert(target_id.to_string(), (now, count + 1));
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
        apply_result(&mut state, &t, result, 7, Instant::now()).await;
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
        apply_result(&mut state, &t, good, 7, Instant::now()).await;
        apply_result(
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
            apply_result(&mut state, &t, r, 1, Instant::now()).await;
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
        apply_result(&mut state, &t, ok(3), 1, Instant::now()).await;
        assert!(changes.has_changed().unwrap());
        changes.borrow_and_update();

        // up stays up: a new latency, a new counter, a new last_check
        apply_result(&mut state, &t, ok(9), 1, Instant::now()).await;
        assert!(!changes.has_changed().unwrap(), "not a status change");

        // up -> degraded
        apply_result(
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
            apply_result(&mut state, &t, r, 1, Instant::now()).await;
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
        let mut state = InfraState::new("/tmp/sctl-infra-proof-test");
        assert!(state.hold_config(InfraConfig {
            version: 1,
            check_interval_secs: 60,
            targets: vec![t.clone()],
        }));
        let start = state.stamp();
        let failed = || CheckResult::failed("PING TIMEOUT");

        // Unknown to up: one change.
        apply_result(&mut state, &t, good(), 1, Instant::now()).await;
        assert_eq!(state.stamp().seq, start.seq + 1);
        // A latency reading alone: none.
        let mut slower = good();
        slower.latency_ms = Some(9);
        apply_result(&mut state, &t, slower, 1, Instant::now()).await;
        assert_eq!(state.stamp().seq, start.seq + 1);
        // Up to degraded: one.
        apply_result(&mut state, &t, failed(), 1, Instant::now()).await;
        assert_eq!(state.stamp().seq, start.seq + 2);
        // Still degraded: none.
        apply_result(&mut state, &t, failed(), 1, Instant::now()).await;
        assert_eq!(state.stamp().seq, start.seq + 2);
        // Degraded to down, and the recovery runs and is logged: one each.
        apply_result(&mut state, &t, failed(), 1, Instant::now()).await;
        let down = state.stamp();
        assert_eq!(down.seq, start.seq + 4);
        assert_eq!(down.recoveries, 1);
        assert_eq!(state.results.recovery_log.len(), 1);
        assert_eq!(
            down.hash,
            picture_hash(&picture_text(down.boot, 1, [("t1", TargetStatus::Down)]))
        );
        // Still down inside the cooldown: no run, no change.
        apply_result(&mut state, &t, failed(), 1, Instant::now()).await;
        assert_eq!(state.stamp(), down);
    }
}
