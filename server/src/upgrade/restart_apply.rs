//! The helper's half of a restart on request (ADR-014): `sctl restart-apply
//! --dir <dir>`, run detached from the service as the upgrade's helper is.
//! It runs the install's restart command, reads health until a new process
//! answers, writes the record's outcome and exits. It replaces nothing, so
//! it restores nothing, and it never reboots the unit.
//!
//! Shared with the upgrade's helper (`apply.rs`): the HTTP reader, the log,
//! the lock, and the settle time, the interval, the passes and the window.
//! Its wait is its own: it has to remember whether the old process still
//! answers, which the upgrade's wait has no use for.
//!
//! Every step is bounded, because this helper holds the lock that keeps
//! every later restart and upgrade out: the restart command runs under a
//! deadline and the helper's own child is killed at it, each health read
//! ends by a total deadline, and the helper as a whole ends by a hard
//! limit derived from its windows, writing its outcome first.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use super::apply::{self, Log, Runner, HEALTH_INTERVAL, HEALTH_PASSES, HEALTH_WINDOW};
use super::install::Layout;
use super::restart::{self, Outcome, RestartState};

/// The plan's file name in the helper's directory.
pub const PLAN_FILE: &str = "plan.json";
/// The log's file name in the helper's directory.
pub const LOG_FILE: &str = "log";

/// What the agent hands its helper: everything the helper needs, so that it
/// reads no configuration of its own.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub v: u32,
    /// The install's restart command, run through `sh -c`.
    pub restart: String,
    /// Where health is read, on loopback.
    pub health_url: String,
    pub layout: Layout,
    /// The record the helper ends.
    pub record_path: PathBuf,
    /// The `instance` of the process that accepted the request: an answer
    /// that carries it comes from the process the restart was to stop.
    pub instance_before: String,
}

/// How the helper reads which process answers: the real thing, or a
/// stand-in for tests.
pub trait Probe {
    /// `Ok(instance)` when the service answered.
    fn instance(&mut self) -> Result<String, String>;
}

/// `GET health_url` on loopback: the answer's `instance`. The whole read
/// ends by `total`: a process that takes the connection and never answers
/// does not hold the helper.
pub struct HttpProbe {
    pub url: String,
    pub total: Duration,
}

impl Probe for HttpProbe {
    fn instance(&mut self) -> Result<String, String> {
        let body = apply::http_get_within(&self.url, self.total, Some(self.total))?;
        let json: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| format!("health is not JSON: {e}"))?;
        json["instance"]
            .as_str()
            .map(ToString::to_string)
            .ok_or_else(|| "health has no instance".to_string())
    }
}

/// How one window ended.
#[derive(Debug, PartialEq, Eq)]
enum Waited {
    /// A new process answered [`HEALTH_PASSES`] times in a row.
    New(String),
    /// The last read that was answered in the window was answered by the
    /// process the restart was to stop, and it is not established that
    /// nothing answers.
    OldStillAnswers,
    /// Nothing answers: every read in the window's last
    /// [`Limits::silence`] failed, at least [`SILENT_READS`] of them.
    /// `new_seen` when a new process did answer at some point: something
    /// is starting it.
    Silence { new_seen: bool },
    /// Neither: a new process answered, though never twice in a row, or
    /// too few reads were made to say that nothing answers.
    Unproven { new_seen: bool },
}

/// How many failed reads, at least, say that nothing answers.
pub const SILENT_READS: usize = 3;
/// The end of the window in which every read must have failed for the
/// helper to say that nothing answers.
pub const SILENCE_SPAN: Duration = Duration::from_secs(30);

/// What one health read found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Read {
    Old,
    New,
    Failed,
}

/// Judge a window that ended with no new process proven, from its reads
/// and when each was made. One failed read at the end does not turn a
/// process that still answers into silence: the judgment is the last read
/// that was answered, unless every read of the window's last `silence`
/// failed, [`SILENT_READS`] of them at least.
fn judge_window(reads: &[(Duration, Read)], window: Duration, silence: Duration) -> Waited {
    let new_seen = reads.iter().any(|(_, read)| *read == Read::New);
    let tail_from = window.saturating_sub(silence);
    let tail: Vec<Read> = reads
        .iter()
        .filter(|(at, _)| *at >= tail_from)
        .map(|(_, read)| *read)
        .collect();
    if tail.len() >= SILENT_READS && tail.iter().all(|read| *read == Read::Failed) {
        return Waited::Silence { new_seen };
    }
    let last_answered = reads
        .iter()
        .rev()
        .map(|(_, read)| *read)
        .find(|read| *read != Read::Failed);
    match last_answered {
        Some(Read::Old) => Waited::OldStillAnswers,
        _ => Waited::Unproven { new_seen },
    }
}

/// Read health until a process other than `before` has answered
/// [`HEALTH_PASSES`] times in a row, at most `window`.
fn wait_new(probe: &mut dyn Probe, before: &str, limits: &Limits, log: &mut Log) -> Waited {
    let start = Instant::now();
    let mut passes = 0;
    let mut candidate: Option<String> = None;
    let mut reads: Vec<(Duration, Read)> = Vec::new();
    while start.elapsed() < limits.window {
        let at = start.elapsed();
        match probe.instance() {
            Ok(instance) if instance == before => {
                passes = 0;
                candidate = None;
                reads.push((at, Read::Old));
                log.line("health: the process that accepted the request still answers");
            }
            Ok(instance) => {
                reads.push((at, Read::New));
                // Two answers count only when one process gave both.
                if candidate.as_deref() == Some(instance.as_str()) {
                    passes += 1;
                } else {
                    passes = 1;
                    candidate = Some(instance.clone());
                }
                log.line(&format!(
                    "health ok ({passes}/{HEALTH_PASSES}): a new process answers"
                ));
                if passes >= HEALTH_PASSES {
                    return Waited::New(instance);
                }
            }
            Err(e) => {
                passes = 0;
                candidate = None;
                reads.push((at, Read::Failed));
                log.line(&format!("health: {e}"));
            }
        }
        std::thread::sleep(limits.interval);
    }
    judge_window(&reads, limits.window, limits.silence)
}

/// Whether a second run of the restart command is tried: on `gz-tmp` only,
/// where the init can return without registering an instance and nothing
/// else starts the agent before the next boot (ADR-014 decision 5).
fn second_run_allowed(layout: Layout) -> bool {
    layout == Layout::GzTmp
}

/// How long the restart command may run before its child is killed.
pub const COMMAND_LIMIT: Duration = Duration::from_mins(1);
/// How long one health read may take, connect to last byte.
pub const READ_LIMIT: Duration = Duration::from_secs(5);
/// What the helper is allowed beyond its bounded steps before it ends
/// itself.
pub const HARD_MARGIN: Duration = Duration::from_secs(30);

/// The bounds of the helper's steps. Each step ends by its own; `hard`
/// ends the helper whatever a step does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Before the command runs: the answer and the state push leave first.
    pub settle: Duration,
    /// The restart command's deadline.
    pub command: Duration,
    /// How long a new process is waited for after the command.
    pub window: Duration,
    /// Between two health reads.
    pub interval: Duration,
    /// One health read's total deadline.
    pub read: Duration,
    /// The end of the window in which every read must have failed for the
    /// helper to say that nothing answers.
    pub silence: Duration,
    /// The helper's hard limit.
    pub hard: Duration,
}

impl Limits {
    /// The helper's real bounds on `layout`. The hard limit is derived
    /// from the bounded steps: the settle time, then for each run of the
    /// command (two on `gz-tmp`) its deadline, the window and one read
    /// that began as the window ended, and a margin.
    pub fn production(layout: Layout) -> Self {
        Self::with(
            layout,
            apply::SETTLE,
            COMMAND_LIMIT,
            HEALTH_WINDOW,
            HEALTH_INTERVAL,
            READ_LIMIT,
            HARD_MARGIN,
        )
    }

    /// Bounds built from the given steps, the hard limit derived as
    /// [`Limits::production`] derives it.
    pub fn with(
        layout: Layout,
        settle: Duration,
        command: Duration,
        window: Duration,
        interval: Duration,
        read: Duration,
        margin: Duration,
    ) -> Self {
        let runs = if second_run_allowed(layout) { 2 } else { 1 };
        Self {
            settle,
            command,
            window,
            interval,
            read,
            silence: SILENCE_SPAN,
            hard: settle + (command + window + read) * runs + margin,
        }
    }
}

/// `sh -c <command>` under a deadline. A command still running at it is
/// killed: the child this helper spawned, and nothing else.
pub struct BoundedShell {
    pub limit: Duration,
}

impl Runner for BoundedShell {
    fn run(&mut self, command: &str) -> Result<i32, String> {
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .stdin(std::process::Stdio::null())
            .spawn()
            .map_err(|e| e.to_string())?;
        let deadline = Instant::now() + self.limit;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return Ok(status.code().unwrap_or(-1)),
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "still running after {}s: its shell was killed",
                        self.limit.as_secs()
                    ));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(e) => return Err(e.to_string()),
            }
        }
    }
}

/// The one write that ends the record. The helper's run and its hard
/// limit both reach for it, and the first to do so writes: an outcome is
/// never written over by a later one.
pub struct Ending {
    record_path: PathBuf,
    log_path: PathBuf,
    done: Mutex<bool>,
    /// How many times the restart command has been run so far.
    attempts: AtomicU32,
}

impl Ending {
    pub fn new(record_path: &Path, log_path: &Path) -> Self {
        Self {
            record_path: record_path.to_path_buf(),
            log_path: log_path.to_path_buf(),
            done: Mutex::new(false),
            attempts: AtomicU32::new(0),
        }
    }

    /// End the record with `state`, unless it is ended already. Returns
    /// the record as it stands on disk either way.
    fn end(&self, state: RestartState, log: &mut Log) -> RestartState {
        let mut done = self
            .done
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *done {
            return restart::load(&self.record_path).unwrap_or(state);
        }
        *done = true;
        let state = state.with_log_tail(super::state::log_tail(&self.log_path));
        if let Err(e) = restart::save(&self.record_path, &state) {
            log.line(&format!(
                "could not write {}: {e}",
                self.record_path.display()
            ));
        }
        state
    }

    /// The hard limit was reached: a step did not return. End the record
    /// as it stands, so that the helper can exit with an outcome written.
    /// `None` when the run ended it first.
    pub fn expire(&self, hard: Duration) -> Option<RestartState> {
        if *self
            .done
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            return None;
        }
        let mut log = Log::open(&self.log_path);
        log.line(&format!(
            "WARNING: the helper reached its hard limit of {}s: a step did not return. Ending with what is known.",
            hard.as_secs()
        ));
        let mut state = restart::load(&self.record_path)
            .unwrap_or_else(RestartState::idle)
            .restarting();
        state.attempts = self.attempts.load(Ordering::SeqCst);
        let state = state.finished(
            Outcome::NotHealthy,
            Some(format!(
                "the helper reached its hard limit of {}s before it could judge: a step did not return",
                hard.as_secs()
            )),
        );
        Some(self.end(state, &mut log))
    }
}

/// The helper's whole run, with its collaborators injected. Returns the
/// record as it ended.
pub fn apply(
    plan: &Plan,
    log_path: &Path,
    runner: &mut dyn Runner,
    probe: &mut dyn Probe,
    limits: &Limits,
    ending: &Ending,
) -> RestartState {
    let mut log = Log::open(log_path);
    let mut state = restart::load(&plan.record_path).unwrap_or_else(|| {
        let mut state = RestartState::idle();
        state.instance_before = Some(plan.instance_before.clone());
        state
    });
    state = state.restarting();
    log.line(&format!(
        "restart-apply: layout {}, restart: {}",
        plan.layout.as_str(),
        plan.restart
    ));
    // The answer and the state push leave first.
    std::thread::sleep(limits.settle);
    let window = limits.window;

    let mut run = |attempt: u32, log: &mut Log| {
        log.line(&format!("restart (run {attempt}): {}", plan.restart));
        ending.attempts.store(attempt, Ordering::SeqCst);
        match runner.run(&plan.restart) {
            Ok(code) => log.line(&format!("restart exited {code}")),
            Err(e) => log.line(&format!("restart: {e}")),
        }
    };
    run(1, &mut log);
    state.attempts = 1;
    let mut waited = wait_new(probe, &plan.instance_before, limits, &mut log);
    let mut second_run = false;
    if waited == (Waited::Silence { new_seen: false }) && second_run_allowed(plan.layout) {
        // Never quiet: the first run started nothing, which is a defect.
        log.line(&format!(
            "WARNING: nothing answered within {}s of the first run: the init started no agent. Running the restart command a second time.",
            window.as_secs()
        ));
        second_run = true;
        run(2, &mut log);
        state.attempts = 2;
        waited = wait_new(probe, &plan.instance_before, limits, &mut log);
    }
    let first_run_started_nothing = format!(
        "the first run of the restart command started nothing within {}s; it was run a second time",
        window.as_secs()
    );
    let silent = matches!(waited, Waited::Silence { .. });
    let (outcome, detail) = match waited {
        Waited::New(instance) => {
            state.instance_after = Some(instance);
            (Outcome::Ok, second_run.then_some(first_run_started_nothing))
        }
        Waited::OldStillAnswers => (
            Outcome::NotRestarted,
            Some(
                "the restart command did not stop the process that accepted the request"
                    .to_string(),
            ),
        ),
        Waited::Silence { new_seen } | Waited::Unproven { new_seen } => {
            let why = match (silent, new_seen) {
                (true, true) => "a new process answered and then stopped answering",
                (true, false) => "no process answered",
                (false, true) => "a new process answered, but never twice in a row,",
                (false, false) => "too few health reads were made to say that nothing answers",
            };
            let detail = if second_run {
                format!(
                    "{first_run_started_nothing}, and {why} within {}s of that either",
                    window.as_secs()
                )
            } else {
                format!("{why} within {}s of the restart command", window.as_secs())
            };
            (Outcome::NotHealthy, Some(detail))
        }
    };
    log.line(&format!(
        "outcome: {} (run {} of the restart command)",
        serde_json::to_string(&outcome).unwrap_or_default(),
        state.attempts
    ));
    ending.end(state.finished(outcome, detail), &mut log)
}

/// The restart's words for its helper: `restart-apply --dir <dir>`.
pub fn helper_args(dir: &Path) -> Vec<std::ffi::OsString> {
    vec!["restart-apply".into(), "--dir".into(), dir.into()]
}

/// The transient unit a restart's helper runs as on the systemd layout,
/// named by when the request was accepted, in unix seconds.
pub fn helper_unit(accepted_unix_secs: u64) -> String {
    format!("sctl-restart-{accepted_unix_secs}")
}

/// Read the plan in `dir`.
pub fn load_plan(dir: &Path) -> Result<Plan, String> {
    let path = dir.join(PLAN_FILE);
    std::fs::read(&path)
        .map_err(|e| e.to_string())
        .and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string()))
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Write the plan into `dir`, which is made if it is not there.
pub fn save_plan(dir: &Path, plan: &Plan) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join(PLAN_FILE);
    let bytes = serde_json::to_vec_pretty(plan).map_err(|e| e.to_string())?;
    std::fs::write(&path, bytes).map_err(|e| format!("{}: {e}", path.display()))
}

/// `sctl restart-apply --dir <dir>`: the process entry. Exit 0 on `ok`, 1
/// on `not_restarted`, 2 on `not_healthy`, 3 when the hard limit ended
/// it, 4 when the plan cannot be read or the directory is locked.
pub fn main(dir: &str) -> i32 {
    let dir = PathBuf::from(dir);
    let plan = match load_plan(&dir) {
        Ok(plan) => plan,
        Err(e) => {
            eprintln!("restart-apply: {e}");
            return 4;
        }
    };
    let Some(_lock) = apply::take_lock(&dir) else {
        eprintln!(
            "restart-apply: {} is locked: another helper is running",
            dir.display()
        );
        return 4;
    };
    let limits = Limits::production(plan.layout);
    let log_path = dir.join(LOG_FILE);
    let ending = std::sync::Arc::new(Ending::new(&plan.record_path, &log_path));
    // The hard limit: whatever a step does, the helper ends, its outcome
    // written first. Its lock goes with the process.
    {
        let ending = ending.clone();
        std::thread::spawn(move || {
            std::thread::sleep(limits.hard);
            if ending.expire(limits.hard).is_some() {
                std::process::exit(3);
            }
        });
    }
    let mut runner = BoundedShell {
        limit: limits.command,
    };
    let mut probe = HttpProbe {
        url: plan.health_url.clone(),
        total: limits.read,
    };
    let state = apply(&plan, &log_path, &mut runner, &mut probe, &limits, &ending);
    match state.outcome {
        Some(Outcome::Ok) => 0,
        Some(Outcome::NotRestarted) => 1,
        // `not_healthy`; the helper writes neither `not_started` nor
        // `interrupted`.
        _ => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upgrade::restart::{Phase, Process};

    const OLD: &str = "old-instance";

    /// A scripted service: what health answers at each check after a run of
    /// the restart command, by run. A script that ends is silence.
    struct Fake {
        /// Per run of the command, the answers that follow it, in order.
        after_run: Vec<Vec<Option<&'static str>>>,
        runs: usize,
        at: usize,
        commands: Vec<String>,
    }

    impl Fake {
        fn new(after_run: Vec<Vec<Option<&'static str>>>) -> Self {
            Self {
                after_run,
                runs: 0,
                at: 0,
                commands: Vec::new(),
            }
        }
    }

    impl Runner for Fake {
        fn run(&mut self, command: &str) -> Result<i32, String> {
            self.commands.push(command.to_string());
            self.runs += 1;
            self.at = 0;
            Ok(0)
        }
    }

    impl Probe for Fake {
        fn instance(&mut self) -> Result<String, String> {
            assert!(self.runs > 0, "health is not read before the command ran");
            let script = self
                .after_run
                .get(self.runs - 1)
                .cloned()
                .unwrap_or_default();
            let answer = script.get(self.at).copied().flatten();
            self.at += 1;
            answer
                .map(ToString::to_string)
                .ok_or_else(|| "connection refused".to_string())
        }
    }

    /// Both traits on one struct: a helper to hand the same fake twice.
    struct Split<'a>(&'a std::cell::RefCell<Fake>);
    impl Runner for Split<'_> {
        fn run(&mut self, c: &str) -> Result<i32, String> {
            self.0.borrow_mut().run(c)
        }
    }
    impl Probe for Split<'_> {
        fn instance(&mut self) -> Result<String, String> {
            self.0.borrow_mut().instance()
        }
    }

    struct Bench {
        dir: PathBuf,
        plan: Plan,
    }

    impl Drop for Bench {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn bench(tag: &str, layout: Layout) -> Bench {
        let dir =
            std::env::temp_dir().join(format!("sctl-restart-apply-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let helper = dir.join("upgrade").join("restart");
        std::fs::create_dir_all(&helper).unwrap();
        let plan = Plan {
            v: 1,
            restart: "restart-it".into(),
            health_url: "http://127.0.0.1:1/api/health".into(),
            layout,
            record_path: dir.join("upgrade").join(restart::FILE_NAME),
            instance_before: OLD.into(),
        };
        // The record as the agent leaves it at the hand-off.
        let me = Process {
            instance: OLD,
            boot: 1_790_000_000_000,
        };
        restart::save(
            &plan.record_path,
            &RestartState::accepted(Some("r-41".into()), &me).restarting(),
        )
        .unwrap();
        Bench { dir, plan }
    }

    /// Bounds small enough for a test: a window of `checks` health reads
    /// 20 ms apart, no settle time.
    fn quick(layout: Layout, checks: u64) -> Limits {
        Limits {
            // The last 150 ms of the window: fifteen reads or so, three
            // needed.
            silence: Duration::from_millis(150),
            ..Limits::with(
                layout,
                Duration::ZERO,
                Duration::from_millis(200),
                Duration::from_millis(20 * checks),
                Duration::from_millis(10),
                Duration::from_millis(200),
                Duration::from_millis(200),
            )
        }
    }

    /// Run the helper over `fake` with a window of `checks` health reads.
    fn run(bench: &Bench, fake: Fake, checks: u64) -> (RestartState, Fake) {
        let fake = std::cell::RefCell::new(fake);
        let log = bench.dir.join("upgrade").join("restart").join(LOG_FILE);
        let state = apply(
            &bench.plan,
            &log,
            &mut Split(&fake),
            &mut Split(&fake),
            &quick(bench.plan.layout, checks),
            &Ending::new(&bench.plan.record_path, &log),
        );
        // What the helper answered is what it left on disk.
        assert_eq!(
            restart::load(&bench.plan.record_path).as_ref(),
            Some(&state)
        );
        (state, fake.into_inner())
    }

    #[test]
    fn a_new_process_that_answers_twice_is_ok() {
        for layout in [
            Layout::UsrBin,
            Layout::GzTmp,
            Layout::Ramboot,
            Layout::Systemd,
        ] {
            let bench = bench(&format!("ok-{}", layout.as_str()), layout);
            // The old process answers once more, then nothing, then the new one.
            let fake = Fake::new(vec![vec![Some(OLD), None, Some("new"), Some("new")]]);
            let (state, fake) = run(&bench, fake, 40);
            assert_eq!(state.phase, Phase::Done);
            assert_eq!(state.outcome, Some(Outcome::Ok), "{}", layout.as_str());
            assert_eq!(state.instance_after.as_deref(), Some("new"));
            assert_eq!(state.attempts, 1);
            assert_eq!(state.detail, None, "one run: nothing to chase");
            assert_eq!(state.request_id.as_deref(), Some("r-41"));
            assert_eq!(state.boot_before, Some(1_790_000_000_000));
            assert_eq!(
                state.boot_after, None,
                "the new process writes its own boot"
            );
            assert_eq!(fake.commands, ["restart-it"]);
            assert!(state.log_tail.unwrap().contains("outcome: \"ok\""));
        }
    }

    /// Two answers count only when one process gave both: a process that
    /// answers once and is replaced by another has not proved itself.
    #[test]
    fn two_answers_from_two_processes_are_not_two_passes() {
        let bench = bench("two", Layout::UsrBin);
        let fake = Fake::new(vec![vec![Some("a"), Some("b"), Some("b")]]);
        let (state, _) = run(&bench, fake, 40);
        assert_eq!(state.outcome, Some(Outcome::Ok));
        assert_eq!(state.instance_after.as_deref(), Some("b"));
    }

    #[test]
    fn the_old_process_still_answering_is_not_restarted_on_every_layout_with_one_run() {
        for layout in [
            Layout::UsrBin,
            Layout::GzTmp,
            Layout::Ramboot,
            Layout::Systemd,
        ] {
            let bench = bench(&format!("old-{}", layout.as_str()), layout);
            let fake = Fake::new(vec![vec![Some(OLD); 64]]);
            let (state, fake) = run(&bench, fake, 6);
            assert_eq!(
                state.outcome,
                Some(Outcome::NotRestarted),
                "{}",
                layout.as_str()
            );
            assert_eq!(state.attempts, 1);
            assert_eq!(state.instance_after, None);
            assert_eq!(
                fake.commands.len(),
                1,
                "never a second run while it answers"
            );
            assert!(state.detail.unwrap().contains("did not stop"));
        }
    }

    #[test]
    fn silence_is_not_healthy_after_one_run_where_a_supervisor_is_trying() {
        for layout in [Layout::UsrBin, Layout::Ramboot, Layout::Systemd] {
            let bench = bench(&format!("silence-{}", layout.as_str()), layout);
            let (state, fake) = run(&bench, Fake::new(vec![vec![]]), 6);
            assert_eq!(
                state.outcome,
                Some(Outcome::NotHealthy),
                "{}",
                layout.as_str()
            );
            assert_eq!(state.attempts, 1);
            assert_eq!(
                fake.commands.len(),
                1,
                "no second run on {}",
                layout.as_str()
            );
            assert!(state.detail.unwrap().starts_with("no process answered"));
        }
    }

    /// On `gz-tmp` the init can return without registering an instance. A
    /// second run brings the agent back, and the record never hides it.
    #[test]
    fn on_gz_tmp_silence_brings_one_second_run_that_the_record_names() {
        let bench = bench("second", Layout::GzTmp);
        // The old process answers once as it stops; then nothing; the
        // second run starts the agent.
        let fake = Fake::new(vec![vec![Some(OLD)], vec![None, Some("new"), Some("new")]]);
        let (state, fake) = run(&bench, fake, 8);
        assert_eq!(state.outcome, Some(Outcome::Ok));
        assert_eq!(state.attempts, 2);
        assert_eq!(state.instance_after.as_deref(), Some("new"));
        assert_eq!(fake.commands, ["restart-it", "restart-it"]);
        let detail = state.detail.unwrap();
        assert!(
            detail.contains("started nothing") && detail.contains("second time"),
            "{detail}"
        );
        let log = state.log_tail.unwrap();
        assert!(log.contains("WARNING: nothing answered"), "{log}");
        assert!(log.contains("run 2 of the restart command"));

        // Silence again: not healthy, and still said.
        let bench = self::bench("second-silence", Layout::GzTmp);
        let (state, fake) = run(&bench, Fake::new(vec![vec![], vec![]]), 6);
        assert_eq!(state.outcome, Some(Outcome::NotHealthy));
        assert_eq!(state.attempts, 2);
        assert_eq!(fake.commands.len(), 2, "one second run, never a third");
        assert!(state.detail.unwrap().contains("second time"));
    }

    /// A new process that answered once and went quiet is being started by
    /// something: a second run would only cut it.
    #[test]
    fn on_gz_tmp_a_new_process_that_went_quiet_brings_no_second_run() {
        let bench = bench("flap", Layout::GzTmp);
        let (state, fake) = run(&bench, Fake::new(vec![vec![Some("new")]]), 20);
        assert_eq!(state.outcome, Some(Outcome::NotHealthy));
        assert_eq!(state.attempts, 1);
        assert_eq!(fake.commands.len(), 1);
        assert!(state.detail.unwrap().contains("then stopped answering"));
    }

    #[test]
    fn the_plan_round_trips_and_a_held_lock_ends_a_second_helper_at_once() {
        let bench = bench("lock", Layout::GzTmp);
        let dir = bench.dir.join("upgrade").join("restart");
        save_plan(&dir, &bench.plan).unwrap();
        assert_eq!(load_plan(&dir).unwrap(), bench.plan);
        let before = restart::load(&bench.plan.record_path).unwrap();
        let _lock = apply::take_lock(&dir).unwrap();
        assert_eq!(main(dir.to_str().unwrap()), 4);
        assert_eq!(restart::load(&bench.plan.record_path), Some(before));
        // No plan: the same exit, nothing run.
        assert_eq!(main(bench.dir.join("nowhere").to_str().unwrap()), 4);
    }

    #[test]
    fn the_probe_reads_the_instance_health_carries() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            for body in [r#"{"status":"ok","instance":"abc"}"#, r#"{"status":"ok"}"#] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 512];
                let _ = stream.read(&mut request);
                let _ = write!(
                    stream,
                    "HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        let mut probe = HttpProbe {
            url: format!("http://127.0.0.1:{port}/api/health"),
            total: READ_LIMIT,
        };
        assert_eq!(probe.instance().as_deref(), Ok("abc"));
        // An agent from before the route carries none: not an answer.
        assert_eq!(probe.instance().unwrap_err(), "health has no instance");
        server.join().unwrap();
    }

    /// The hard limit is derived from the bounded steps, and the agent
    /// that waits on the helper gives it longer than that: it never calls
    /// a helper lost that is still inside its own time.
    #[test]
    fn the_hard_limit_is_derived_from_the_steps_and_the_agent_waits_longer() {
        // 2 s, then a minute for the command, the 180 s window and one 5 s
        // read, and 30 s.
        assert_eq!(
            Limits::production(Layout::UsrBin).hard.as_secs(),
            2 + 245 + 30
        );
        assert_eq!(
            Limits::production(Layout::Ramboot),
            Limits::production(Layout::Systemd)
        );
        // Two runs of the command on gz-tmp.
        assert_eq!(
            Limits::production(Layout::GzTmp).hard.as_secs(),
            2 + 490 + 30
        );
        let agent = restart::Timing::production();
        for layout in [
            Layout::UsrBin,
            Layout::GzTmp,
            Layout::Ramboot,
            Layout::Systemd,
        ] {
            assert!(
                agent.bound(layout) > Limits::production(layout).hard,
                "{}",
                layout.as_str()
            );
        }
    }

    /// The restart command runs under a deadline. One still running at it
    /// is killed: the helper's own child. The test's commands are harmless
    /// stand-ins, and the child killed is the one this test started.
    #[test]
    fn a_restart_command_that_does_not_return_is_killed_at_its_deadline() {
        let mut shell = BoundedShell {
            limit: Duration::from_millis(300),
        };
        let at = Instant::now();
        let hung = shell.run("sleep 30").unwrap_err();
        assert!(at.elapsed() < Duration::from_secs(5), "{:?}", at.elapsed());
        assert!(hung.contains("killed"), "{hung}");
        // One that ends in time answers its exit code.
        assert_eq!(shell.run("true"), Ok(0));
        assert_eq!(shell.run("exit 3"), Ok(3));
    }

    /// A command that hit its deadline is logged, and the helper still
    /// reads health: the restart may have happened before the command
    /// hung.
    #[test]
    fn a_killed_command_does_not_end_the_helper_which_still_judges_by_health() {
        struct Hung;
        impl Runner for Hung {
            fn run(&mut self, _: &str) -> Result<i32, String> {
                Err("still running after 60s: its shell was killed".into())
            }
        }
        struct NewAtOnce;
        impl Probe for NewAtOnce {
            fn instance(&mut self) -> Result<String, String> {
                Ok("new".into())
            }
        }
        let bench = bench("killed", Layout::UsrBin);
        let log = bench.dir.join("upgrade").join("restart").join(LOG_FILE);
        let state = apply(
            &bench.plan,
            &log,
            &mut Hung,
            &mut NewAtOnce,
            &quick(Layout::UsrBin, 8),
            &Ending::new(&bench.plan.record_path, &log),
        );
        assert_eq!(state.outcome, Some(Outcome::Ok));
        assert!(state.log_tail.unwrap().contains("its shell was killed"));
    }

    /// A health read ends by its total deadline, whatever the peer does: a
    /// process that takes the connection and sends a byte now and then
    /// does not hold the helper.
    #[test]
    fn a_health_read_ends_by_its_total_deadline() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 512];
            let _ = stream.read(&mut request);
            // A byte every 100 ms for 3 s: each read succeeds in time, and
            // the answer never ends.
            for byte in b"HTTP/1.0 200 OK\r\n\r\n{\"sta".iter().cycle().take(30) {
                if stream.write_all(&[*byte]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        let mut probe = HttpProbe {
            url: format!("http://127.0.0.1:{port}/api/health"),
            total: Duration::from_millis(400),
        };
        let at = Instant::now();
        let error = probe.instance().unwrap_err();
        assert!(
            at.elapsed() < Duration::from_millis(1500),
            "ended by the total, not by the peer: {:?}",
            at.elapsed()
        );
        assert!(error.contains("no answer within 400 ms"), "{error}");
        server.join().unwrap();
    }

    /// The helper as a whole ends by its hard limit, with an outcome
    /// written first, and that outcome is not written over by a step that
    /// returns later.
    #[test]
    fn the_hard_limit_ends_the_record_and_a_late_step_does_not_write_over_it() {
        /// A step that does not return in time, whatever its own bound.
        struct Stuck;
        impl Runner for Stuck {
            fn run(&mut self, _: &str) -> Result<i32, String> {
                std::thread::sleep(Duration::from_millis(500));
                Ok(0)
            }
        }
        struct NewAtOnce;
        impl Probe for NewAtOnce {
            fn instance(&mut self) -> Result<String, String> {
                Ok("new".into())
            }
        }
        let bench = bench("hard", Layout::GzTmp);
        let log = bench.dir.join("upgrade").join("restart").join(LOG_FILE);
        let ending = Ending::new(&bench.plan.record_path, &log);
        let limits = quick(Layout::GzTmp, 4);
        let state = std::thread::scope(|scope| {
            let run = scope.spawn(|| {
                apply(
                    &bench.plan,
                    &log,
                    &mut Stuck,
                    &mut NewAtOnce,
                    &limits,
                    &ending,
                )
            });
            // The hard limit passes while the command is still stuck.
            std::thread::sleep(Duration::from_millis(100));
            let expired = ending
                .expire(limits.hard)
                .expect("the limit ends the record");
            assert_eq!(expired.phase, crate::upgrade::restart::Phase::Done);
            assert_eq!(expired.outcome, Some(Outcome::NotHealthy));
            assert_eq!(expired.attempts, 1, "the command was run once so far");
            assert!(expired.detail.as_deref().unwrap().contains("hard limit"));
            assert!(expired
                .log_tail
                .as_deref()
                .unwrap()
                .contains("WARNING: the helper reached its hard limit"));
            assert_eq!(
                restart::load(&bench.plan.record_path),
                Some(expired.clone())
            );
            // In the helper the process exits here. The run that returns
            // late finds the record ended and leaves it.
            let late = run.join().unwrap();
            assert_eq!(late, expired);
            late
        });
        assert_eq!(restart::load(&bench.plan.record_path), Some(state));
        // Once the run ended the record, the limit has nothing to end.
        let bench = self::bench("hard-late", Layout::UsrBin);
        let log = bench.dir.join("upgrade").join("restart").join(LOG_FILE);
        let ending = Ending::new(&bench.plan.record_path, &log);
        let ended = apply(
            &bench.plan,
            &log,
            &mut Stuck,
            &mut NewAtOnce,
            &quick(Layout::UsrBin, 4),
            &ending,
        );
        assert_eq!(ended.outcome, Some(Outcome::Ok));
        assert_eq!(ending.expire(Duration::from_secs(1)), None);
        assert_eq!(restart::load(&bench.plan.record_path), Some(ended));
    }

    /// A health that answers by the clock: `old` until `quiet_from`, then
    /// nothing.
    struct OldUntil {
        started: Option<Instant>,
        quiet_from: Duration,
    }
    impl Runner for OldUntil {
        fn run(&mut self, _: &str) -> Result<i32, String> {
            self.started.get_or_insert_with(Instant::now);
            Ok(0)
        }
    }
    impl Probe for OldUntil {
        fn instance(&mut self) -> Result<String, String> {
            if self.started.expect("the command ran").elapsed() < self.quiet_from {
                Ok(OLD.to_string())
            } else {
                Err("connection refused".into())
            }
        }
    }
    struct SplitClock<'a>(&'a std::cell::RefCell<OldUntil>);
    impl Runner for SplitClock<'_> {
        fn run(&mut self, c: &str) -> Result<i32, String> {
            self.0.borrow_mut().run(c)
        }
    }
    impl Probe for SplitClock<'_> {
        fn instance(&mut self) -> Result<String, String> {
            self.0.borrow_mut().instance()
        }
    }

    /// One failed read at the end of the window does not turn a process
    /// that still answers into silence, and on `gz-tmp` it brings no
    /// second run while the old process is alive.
    #[test]
    fn a_failed_read_at_the_end_does_not_make_the_old_process_silent() {
        let bench = bench("lastread", Layout::GzTmp);
        let log = bench.dir.join("upgrade").join("restart").join(LOG_FILE);
        // A 600 ms window; the old process answers until its last 40 ms.
        let limits = quick(Layout::GzTmp, 30);
        let fake = std::cell::RefCell::new(OldUntil {
            started: None,
            quiet_from: limits.window.saturating_sub(Duration::from_millis(40)),
        });
        let state = apply(
            &bench.plan,
            &log,
            &mut SplitClock(&fake),
            &mut SplitClock(&fake),
            &limits,
            &Ending::new(&bench.plan.record_path, &log),
        );
        assert_eq!(state.outcome, Some(Outcome::NotRestarted), "{state:?}");
        assert_eq!(
            state.attempts, 1,
            "no second run while the old process answers"
        );
    }

    /// The judgment, from the reads and when each was made.
    #[test]
    fn a_window_is_judged_by_its_last_answered_read_and_silence_by_its_tail() {
        use Read::{Failed, New, Old};
        let window = Duration::from_mins(3);
        let silence = Duration::from_secs(30);
        let at = |secs: u64, read| (Duration::from_secs(secs), read);
        let judge = |reads: &[(Duration, Read)]| judge_window(reads, window, silence);

        // Old throughout, one failure at the very end: it still answers.
        let mut reads: Vec<_> = (0..35).map(|i| at(i * 5, Old)).collect();
        reads.push(at(178, Failed));
        assert_eq!(judge(&reads), Waited::OldStillAnswers);
        // Two failures at the end are not three.
        reads.push(at(179, Failed));
        assert_eq!(judge(&reads), Waited::OldStillAnswers);

        // Old once, then every read failed, the last 30 s included: nothing
        // answers, and nothing new was seen.
        let mut reads = vec![at(0, Old)];
        reads.extend((1..36).map(|i| at(i * 5, Failed)));
        assert_eq!(judge(&reads), Waited::Silence { new_seen: false });

        // Every read failed: the same.
        let reads: Vec<_> = (0..36).map(|i| at(i * 5, Failed)).collect();
        assert_eq!(judge(&reads), Waited::Silence { new_seen: false });

        // A new process answered once, then silence to the end: something
        // is starting it.
        let mut reads = vec![at(0, Failed), at(5, New)];
        reads.extend((2..36).map(|i| at(i * 5, Failed)));
        assert_eq!(judge(&reads), Waited::Silence { new_seen: true });

        // The last 30 s hold an answered read: not silence. The last read
        // answered decides.
        let mut reads: Vec<_> = (0..30).map(|i| at(i * 5, Failed)).collect();
        reads.extend([
            at(155, Old),
            at(160, Failed),
            at(165, Failed),
            at(170, Failed),
        ]);
        assert_eq!(judge(&reads), Waited::OldStillAnswers);
        let mut reads: Vec<_> = (0..30).map(|i| at(i * 5, Failed)).collect();
        reads.extend([
            at(155, New),
            at(160, Failed),
            at(165, Failed),
            at(170, Failed),
        ]);
        assert_eq!(judge(&reads), Waited::Unproven { new_seen: true });

        // Too few reads in the last 30 s to say that nothing answers: two
        // slow failed reads are not silence, and bring no second run.
        let reads = [
            at(0, Failed),
            at(100, Failed),
            at(160, Failed),
            at(175, Failed),
        ];
        assert_eq!(judge(&reads), Waited::Unproven { new_seen: false });
        assert_eq!(judge(&[]), Waited::Unproven { new_seen: false });
        // Three are.
        let reads = [at(152, Failed), at(160, Failed), at(175, Failed)];
        assert_eq!(judge(&reads), Waited::Silence { new_seen: false });
        assert_eq!(SILENCE_SPAN, Duration::from_secs(30));
        assert_eq!(Limits::production(Layout::GzTmp).silence, SILENCE_SPAN);
    }

    /// Too little evidence is not silence: on `gz-tmp` it brings no second
    /// run, and the record says why.
    #[test]
    fn too_few_reads_bring_no_second_run() {
        let bench = bench("thin", Layout::GzTmp);
        let log = bench.dir.join("upgrade").join("restart").join(LOG_FILE);
        // Two reads in the whole window.
        let limits = Limits {
            window: Duration::from_millis(70),
            interval: Duration::from_millis(50),
            ..quick(Layout::GzTmp, 1)
        };
        let fake = std::cell::RefCell::new(Fake::new(vec![vec![], vec![]]));
        let state = apply(
            &bench.plan,
            &log,
            &mut Split(&fake),
            &mut Split(&fake),
            &limits,
            &Ending::new(&bench.plan.record_path, &log),
        );
        assert_eq!(state.outcome, Some(Outcome::NotHealthy));
        assert_eq!(state.attempts, 1);
        assert_eq!(fake.into_inner().commands.len(), 1);
        assert!(state.detail.unwrap().starts_with("too few health reads"));
    }
}
