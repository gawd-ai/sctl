//! The helper's half of a restart on request (ADR-014): `sctl restart-apply
//! --dir <dir>`, run detached from the service as the upgrade's helper is.
//! It runs the install's restart command, reads health until a new process
//! answers, writes the record's outcome and exits. It replaces nothing, so
//! it restores nothing, and it never reboots the unit.
//!
//! Shared with the upgrade's helper (`apply.rs`): the shell that runs the
//! command, the HTTP reader, the log, the lock, and the settle time, the
//! interval, the passes and the window. Its wait is its own: it has to
//! remember whether the old process still answers, which the upgrade's
//! wait has no use for.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::apply::{self, Log, Runner, Shell, HEALTH_INTERVAL, HEALTH_PASSES, HEALTH_WINDOW};
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

/// `GET health_url` on loopback, 5 s budget: the answer's `instance`.
pub struct HttpProbe {
    pub url: String,
}

impl Probe for HttpProbe {
    fn instance(&mut self) -> Result<String, String> {
        let body = apply::http_get(&self.url, Duration::from_secs(5))?;
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
    /// The window's last answer came from the process the restart was to
    /// stop.
    OldStillAnswers,
    /// The window ended on no answer. `new_seen` when a new process did
    /// answer at some point: something is starting it.
    Silence { new_seen: bool },
}

/// Read health until a process other than `before` has answered
/// [`HEALTH_PASSES`] times in a row, at most `window`.
fn wait_new(
    probe: &mut dyn Probe,
    before: &str,
    window: Duration,
    interval: Duration,
    log: &mut Log,
) -> Waited {
    let start = Instant::now();
    let mut passes = 0;
    let mut candidate: Option<String> = None;
    let mut new_seen = false;
    let mut last_was_old = false;
    while start.elapsed() < window {
        match probe.instance() {
            Ok(instance) if instance == before => {
                passes = 0;
                candidate = None;
                last_was_old = true;
                log.line("health: the process that accepted the request still answers");
            }
            Ok(instance) => {
                new_seen = true;
                last_was_old = false;
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
                last_was_old = false;
                log.line(&format!("health: {e}"));
            }
        }
        std::thread::sleep(interval);
    }
    if last_was_old {
        Waited::OldStillAnswers
    } else {
        Waited::Silence { new_seen }
    }
}

/// Whether a second run of the restart command is tried: on `gz-tmp` only,
/// where the init can return without registering an instance and nothing
/// else starts the agent before the next boot (ADR-014 decision 5).
fn second_run_allowed(layout: Layout) -> bool {
    layout == Layout::GzTmp
}

/// The helper's whole run, with its collaborators injected. Returns the
/// record it wrote. `settle` is the upgrade's 2 s in production and zero in
/// tests.
pub fn apply(
    plan: &Plan,
    log_path: &Path,
    runner: &mut dyn Runner,
    probe: &mut dyn Probe,
    window: Duration,
    interval: Duration,
    settle: Duration,
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
    std::thread::sleep(settle);

    let mut run = |attempt: u32, log: &mut Log| {
        log.line(&format!("restart (run {attempt}): {}", plan.restart));
        match runner.run(&plan.restart) {
            Ok(code) => log.line(&format!("restart exited {code}")),
            Err(e) => log.line(&format!("restart: {e}")),
        }
    };
    run(1, &mut log);
    state.attempts = 1;
    let mut waited = wait_new(probe, &plan.instance_before, window, interval, &mut log);
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
        waited = wait_new(probe, &plan.instance_before, window, interval, &mut log);
    }
    let first_run_started_nothing = format!(
        "the first run of the restart command started nothing within {}s; it was run a second time",
        window.as_secs()
    );
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
        Waited::Silence { new_seen } => {
            let why = if new_seen {
                "a new process answered and then stopped answering"
            } else {
                "no process answered"
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
    let tail = super::state::log_tail(log_path);
    let state = state.finished(outcome, detail).with_log_tail(tail);
    if let Err(e) = restart::save(&plan.record_path, &state) {
        log.line(&format!(
            "could not write {}: {e}",
            plan.record_path.display()
        ));
    }
    state
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
/// on `not_restarted`, 2 on `not_healthy`, 4 when the plan cannot be read
/// or the directory is locked.
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
    let mut runner = Shell;
    let mut probe = HttpProbe {
        url: plan.health_url.clone(),
    };
    let state = apply(
        &plan,
        &dir.join(LOG_FILE),
        &mut runner,
        &mut probe,
        HEALTH_WINDOW,
        HEALTH_INTERVAL,
        apply::SETTLE,
    );
    match state.outcome {
        Some(Outcome::Ok) => 0,
        Some(Outcome::NotRestarted) => 1,
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

    /// Run the helper over `fake` with a window of `checks` health reads.
    fn run(bench: &Bench, fake: Fake, checks: u64) -> (RestartState, Fake) {
        let fake = std::cell::RefCell::new(fake);
        let state = apply(
            &bench.plan,
            &bench.dir.join("upgrade").join("restart").join(LOG_FILE),
            &mut Split(&fake),
            &mut Split(&fake),
            Duration::from_millis(20 * checks),
            Duration::from_millis(20),
            Duration::ZERO,
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
        let (state, fake) = run(&bench, Fake::new(vec![vec![Some("new")]]), 6);
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
        };
        assert_eq!(probe.instance().as_deref(), Ok("abc"));
        // An agent from before the route carries none: not an answer.
        assert_eq!(probe.instance().unwrap_err(), "health has no instance");
        server.join().unwrap();
    }
}
