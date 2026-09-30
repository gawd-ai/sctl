//! The helper's half of an upgrade: `sctl upgrade-apply --stage <dir>`,
//! run detached from the service by [`super::stage`]. It swaps the staged
//! files into place, restarts the service, waits for the new agent's
//! health, and restores the rollback set when health does not come. It is
//! plain synchronous code: the fewer moving parts, the fewer ways to leave a
//! box half-swapped.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::stage::Plan;
use super::state::{self, Outcome, Phase, UpgradeState};
use super::version::Version;

/// Health must pass twice, this far apart, within [`HEALTH_WINDOW`].
pub const HEALTH_INTERVAL: Duration = Duration::from_secs(5);
/// After the restart, how long health may take.
pub const HEALTH_WINDOW: Duration = Duration::from_mins(3);
/// Consecutive healthy answers needed.
pub const HEALTH_PASSES: u32 = 2;
/// Before touching anything: the 202 and the state push must leave first.
const SETTLE: Duration = Duration::from_secs(2);

/// How the helper judges health: the real thing, or a stand-in for tests.
pub trait Health {
    /// `Ok(version, tunnel_connected)` when the service answered.
    fn check(&mut self) -> Result<(String, bool), String>;
}

/// `GET health_url` over plain HTTP/1.0 on loopback, 5 s budget.
pub struct HttpHealth {
    pub url: String,
}

impl Health for HttpHealth {
    fn check(&mut self) -> Result<(String, bool), String> {
        let body = http_get(&self.url, Duration::from_secs(5))?;
        let json: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| format!("health is not JSON: {e}"))?;
        let version = json["version"]
            .as_str()
            .ok_or("health has no version")?
            .to_string();
        let tunnel = json["tunnel"]["connected"].as_bool().unwrap_or(false);
        Ok((version, tunnel))
    }
}

/// A minimal HTTP/1.0 GET: the body of a 200, or the error.
fn http_get(url: &str, budget: Duration) -> Result<String, String> {
    use std::io::Read;
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("{url} is not http://"))?;
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let addr = if authority.contains(':') {
        authority.to_string()
    } else {
        format!("{authority}:80")
    };
    let socket: std::net::SocketAddr = addr
        .parse()
        .map_err(|_| format!("{addr} is not an address"))?;
    let mut stream =
        std::net::TcpStream::connect_timeout(&socket, budget).map_err(|e| e.to_string())?;
    stream.set_read_timeout(Some(budget)).ok();
    stream.set_write_timeout(Some(budget)).ok();
    let request = format!("GET /{path} HTTP/1.0\r\nHost: {authority}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .map_err(|e| e.to_string())?;
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&response);
    let (head, body) = text.split_once("\r\n\r\n").ok_or("no HTTP response")?;
    let status: u16 = head
        .get(9..12)
        .and_then(|s| s.parse().ok())
        .ok_or("no HTTP status")?;
    if status != 200 {
        return Err(format!("HTTP {status}"));
    }
    Ok(body.to_string())
}

/// What runs commands: the shell, or a stand-in for tests.
pub trait Runner {
    fn run(&mut self, command: &str) -> Result<i32, String>;
}

/// `sh -c <command>`.
pub struct Shell;

impl Runner for Shell {
    fn run(&mut self, command: &str) -> Result<i32, String> {
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .status()
            .map_err(|e| e.to_string())?;
        Ok(status.code().unwrap_or(-1))
    }
}

/// The helper's log: the stage's `log` file, one stamped line per step.
pub struct Log {
    file: Option<std::fs::File>,
}

impl Log {
    fn open(path: &Path) -> Self {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok();
        Self { file }
    }

    fn line(&mut self, text: &str) {
        let line = format!("{} {text}\n", crate::infra::now_iso());
        // The helper's stderr is the same log file: write once.
        match &mut self.file {
            Some(f) => {
                let _ = f.write_all(line.as_bytes());
                let _ = f.flush();
            }
            None => eprint!("{line}"),
        }
    }
}

/// Copy `from` to `<target>.new` and rename over `target`: a full
/// filesystem fails at the copy, before anything moved.
fn place(from: &Path, target: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let new = target.with_extension(match target.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{ext}.new"),
        None => "new".to_string(),
    });
    std::fs::copy(from, &new)
        .map_err(|e| format!("copy {} to {}: {e}", from.display(), new.display()))?;
    std::fs::set_permissions(&new, std::fs::Permissions::from_mode(mode))
        .map_err(|e| format!("chmod {}: {e}", new.display()))?;
    std::fs::rename(&new, target)
        .map_err(|e| format!("rename {} over {}: {e}", new.display(), target.display()))?;
    Ok(())
}

fn sync_disks() {
    // SAFETY: sync(2) takes no arguments and cannot fail.
    unsafe { libc::sync() };
}

/// Wait for health: `expected` version (any when `None`), the tunnel up when
/// `need_tunnel`, [`HEALTH_PASSES`] times in a row within `window`.
pub fn wait_healthy(
    health: &mut dyn Health,
    expected: Option<&Version>,
    need_tunnel: bool,
    window: Duration,
    interval: Duration,
    log: &mut Log,
) -> bool {
    let start = Instant::now();
    let mut passes = 0;
    while start.elapsed() < window {
        match health.check() {
            Ok((version, tunnel)) => {
                let version_ok =
                    expected.is_none_or(|e| Version::parse(&version).as_ref() == Some(e));
                let tunnel_ok = !need_tunnel || tunnel;
                if version_ok && tunnel_ok {
                    passes += 1;
                    log.line(&format!(
                        "health ok ({passes}/{HEALTH_PASSES}): version {version}, tunnel {tunnel}"
                    ));
                    if passes >= HEALTH_PASSES {
                        return true;
                    }
                } else {
                    passes = 0;
                    log.line(&format!(
                        "health not yet: version {version}, tunnel {tunnel}"
                    ));
                }
            }
            Err(e) => {
                passes = 0;
                log.line(&format!("health: {e}"));
            }
        }
        std::thread::sleep(interval);
    }
    false
}

/// The helper's whole run, with its collaborators injected. Returns the
/// outcome it wrote. `settle` is [`SETTLE`] in production and zero in tests.
pub fn apply(
    plan: &Plan,
    runner: &mut dyn Runner,
    health: &mut dyn Health,
    window: Duration,
    interval: Duration,
    settle: Duration,
) -> Outcome {
    let mut log = Log::open(&plan.log_path);
    let mut state = UpgradeState::staging(
        plan.request_id.clone(),
        &plan.from_version,
        &plan.to_version,
    )
    .with_phase(Phase::Applying);
    let finish = |state: UpgradeState, outcome: Outcome, log: &mut Log| {
        log.line(&format!(
            "outcome: {}",
            serde_json::to_string(&outcome).unwrap_or_default()
        ));
        let tail = state::log_tail(&plan.log_path);
        let state = state.finished(outcome).with_log_tail(tail);
        if let Err(e) = state::save(&plan.state_path, &state) {
            log.line(&format!(
                "could not write {}: {e}",
                plan.state_path.display()
            ));
        }
        outcome
    };
    log.line(&format!(
        "upgrade-apply: {} to {} ({} files, layout {})",
        plan.from_version,
        plan.to_version,
        plan.swaps.len(),
        plan.layout.as_str()
    ));
    std::thread::sleep(settle);

    // Every staged file, hashed again just before the swap.
    for swap in &plan.swaps {
        match super::sha256_file(&swap.staged) {
            Ok(actual) if actual == swap.sha256 => {}
            Ok(actual) => {
                log.line(&format!(
                    "{}: sha256 {actual} is not {}",
                    swap.staged.display(),
                    swap.sha256
                ));
                state.reason = Some(super::Reason::Sha256Mismatch);
                state.detail = Some(format!(
                    "{} changed since it was staged",
                    swap.staged.display()
                ));
                return finish(state, Outcome::NotApplied, &mut log);
            }
            Err(e) => {
                log.line(&format!("{}: {e}", swap.staged.display()));
                state.reason = Some(super::Reason::Internal);
                state.detail = Some(format!("{}: {e}", swap.staged.display()));
                return finish(state, Outcome::NotApplied, &mut log);
            }
        }
    }
    // Room for the copies beside the targets.
    let total: u64 = plan.swaps.iter().map(|s| s.size).sum();
    if let Some(first) = plan.swaps.first() {
        let dir = first.target.parent().unwrap_or(Path::new("/"));
        let free = super::stage::free_bytes(dir).unwrap_or(u64::MAX);
        let need = total.saturating_add(plan.min_free_kb.saturating_mul(1024));
        if free < need {
            log.line(&format!(
                "{}: {} KB free, need {} KB",
                dir.display(),
                free / 1024,
                need / 1024
            ));
            state.reason = Some(super::Reason::NoSpace);
            state.detail = Some(format!(
                "{} has {} KB free, needs {} KB",
                dir.display(),
                free / 1024,
                need / 1024
            ));
            return finish(state, Outcome::NotApplied, &mut log);
        }
    }

    // The swap. From here on a failure is a rollback, not a refusal.
    let mut moved = false;
    for swap in &plan.swaps {
        match place(&swap.staged, &swap.target, swap.mode) {
            Ok(()) => {
                moved = true;
                log.line(&format!("placed {}", swap.target.display()));
            }
            Err(e) => {
                log.line(&format!("place failed: {e}"));
                if !moved {
                    state.reason = Some(super::Reason::Internal);
                    state.detail = Some(e);
                    return finish(state, Outcome::NotApplied, &mut log);
                }
                return rollback(plan, runner, health, window, interval, &mut log, state, e);
            }
        }
    }
    sync_disks();
    log.line(&format!("restart: {}", plan.restart));
    match runner.run(&plan.restart) {
        Ok(code) => log.line(&format!("restart exited {code}")),
        Err(e) => log.line(&format!("restart: {e}")),
    }
    let expected = Version::parse(&plan.to_version);
    if wait_healthy(
        health,
        expected.as_ref(),
        plan.need_tunnel,
        window,
        interval,
        &mut log,
    ) {
        return finish(state, Outcome::Ok, &mut log);
    }
    log.line(&format!(
        "{} did not pass health within {}s",
        plan.to_version,
        window.as_secs()
    ));
    rollback(
        plan,
        runner,
        health,
        window,
        interval,
        &mut log,
        state,
        format!("{} did not answer health", plan.to_version),
    )
}

#[allow(clippy::too_many_arguments)]
fn rollback(
    plan: &Plan,
    runner: &mut dyn Runner,
    health: &mut dyn Health,
    window: Duration,
    interval: Duration,
    log: &mut Log,
    mut state: UpgradeState,
    why: String,
) -> Outcome {
    log.line(&format!("rolling back: {why}"));
    state.detail = Some(why);
    let mut restored_all = true;
    for restore in &plan.restores {
        match place(&restore.backup, &restore.target, restore.mode) {
            Ok(()) => log.line(&format!("restored {}", restore.target.display())),
            Err(e) => {
                restored_all = false;
                log.line(&format!("restore failed: {e}"));
            }
        }
    }
    sync_disks();
    match runner.run(&plan.restart) {
        Ok(code) => log.line(&format!("restart exited {code}")),
        Err(e) => log.line(&format!("restart: {e}")),
    }
    // Back means the previous files answer health again; the tunnel is not
    // required here, since a network that fell during the upgrade is not
    // something the helper can repair and the old agent reconnects on its
    // own when it can.
    let back = wait_healthy(health, None, false, window, interval, log);
    let outcome = if restored_all && back {
        Outcome::RolledBack
    } else {
        Outcome::NeedsHands
    };
    log.line(&format!(
        "outcome: {}",
        serde_json::to_string(&outcome).unwrap_or_default()
    ));
    let tail = state::log_tail(&plan.log_path);
    let state = state.finished(outcome).with_log_tail(tail);
    if let Err(e) = state::save(&plan.state_path, &state) {
        log.line(&format!(
            "could not write {}: {e}",
            plan.state_path.display()
        ));
    }
    outcome
}

/// Take the stage's lock (`<stage>/lock`), non-blocking; `None` when held.
pub fn take_lock(stage: &Path) -> Option<std::fs::File> {
    use std::os::unix::io::AsRawFd;
    let path = stage.join("lock");
    let file = std::fs::File::create(&path).ok()?;
    // SAFETY: flock on a file descriptor we own.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    (rc == 0).then_some(file)
}

/// Whether the stage's lock is free (the helper is not running).
pub fn lock_is_free(stage: &Path) -> bool {
    take_lock(stage).is_some()
}

/// `sctl upgrade-apply --stage <dir>`: the process entry. Exit 0 on `ok`,
/// 1 on `rolled_back`, 2 on `needs_hands`, 3 on `not_applied`, 4 when the
/// stage cannot be read or is locked.
pub fn main(stage: &str) -> i32 {
    let stage = PathBuf::from(stage);
    let plan_path = stage.join("plan.json");
    let plan: Plan = match std::fs::read(&plan_path)
        .and_then(|b| serde_json::from_slice(&b).map_err(std::io::Error::other))
    {
        Ok(p) => p,
        Err(e) => {
            eprintln!("upgrade-apply: {}: {e}", plan_path.display());
            return 4;
        }
    };
    let Some(_lock) = take_lock(&stage) else {
        eprintln!(
            "upgrade-apply: {} is locked: another helper is running",
            stage.display()
        );
        return 4;
    };
    let mut runner = Shell;
    let mut health = HttpHealth {
        url: plan.health_url.clone(),
    };
    match apply(
        &plan,
        &mut runner,
        &mut health,
        HEALTH_WINDOW,
        HEALTH_INTERVAL,
        SETTLE,
    ) {
        Outcome::Ok => 0,
        Outcome::RolledBack => 1,
        Outcome::NeedsHands => 2,
        Outcome::NotApplied => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upgrade::install::Layout;
    use crate::upgrade::stage::{Restore, Swap};

    /// A scripted service: each `run` "restarts" it onto the version of the
    /// file now at `binary`, and health answers from that file's content.
    struct Fake {
        binary: PathBuf,
        running: String,
        tunnel: bool,
        restarts: u32,
        /// Health answers to swallow before answering (per restart).
        silent: u32,
    }

    impl Runner for Fake {
        fn run(&mut self, command: &str) -> Result<i32, String> {
            assert_eq!(command, "restart-it");
            self.restarts += 1;
            self.running = std::fs::read_to_string(&self.binary)
                .unwrap_or_default()
                .trim()
                .to_string();
            Ok(0)
        }
    }

    impl Health for Fake {
        fn check(&mut self) -> Result<(String, bool), String> {
            if self.silent > 0 {
                self.silent -= 1;
                return Err("connection refused".into());
            }
            if self.running == "broken" {
                return Err("connection refused".into());
            }
            Ok((self.running.clone(), self.tunnel))
        }
    }

    /// Both traits on one struct: a helper to hand the same fake twice.
    struct Split<'a>(&'a std::cell::RefCell<Fake>);
    impl Runner for Split<'_> {
        fn run(&mut self, c: &str) -> Result<i32, String> {
            self.0.borrow_mut().run(c)
        }
    }
    impl Health for Split<'_> {
        fn check(&mut self) -> Result<(String, bool), String> {
            self.0.borrow_mut().check()
        }
    }

    struct Bench {
        dir: PathBuf,
        plan: Plan,
    }

    fn bench(tag: &str, new_content: &str) -> Bench {
        let dir = std::env::temp_dir().join(format!("sctl-apply-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("stage")).unwrap();
        std::fs::create_dir_all(dir.join("rollback")).unwrap();
        let binary = dir.join("bin").join("sctl");
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        std::fs::write(&binary, "0.6.7.1\n").unwrap();
        let staged = dir.join("stage").join("sctl-x86_64");
        std::fs::write(&staged, new_content).unwrap();
        let backup = dir.join("rollback").join("server");
        std::fs::copy(&binary, &backup).unwrap();
        let plan = Plan {
            v: 1,
            request_id: Some("r-1".into()),
            from_version: "0.6.7.1".into(),
            to_version: "0.6.8.2".into(),
            layout: Layout::UsrBin,
            swaps: vec![Swap {
                staged: staged.clone(),
                target: binary.clone(),
                sha256: crate::upgrade::sha256_file(&staged).unwrap(),
                size: new_content.len() as u64,
                mode: 0o755,
            }],
            restores: vec![Restore {
                backup,
                target: binary,
                mode: 0o755,
            }],
            restart: "restart-it".into(),
            health_url: "http://127.0.0.1:1/api/health".into(),
            need_tunnel: true,
            min_free_kb: 0,
            state_path: dir.join("state.json"),
            log_path: dir.join("stage").join("log"),
        };
        Bench { dir, plan }
    }

    fn run(b: &Bench, fake: Fake) -> (Outcome, UpgradeState, Fake) {
        let cell = std::cell::RefCell::new(fake);
        let outcome = {
            let mut runner = Split(&cell);
            let mut health = Split(&cell);
            apply(
                &b.plan,
                &mut runner,
                &mut health,
                Duration::from_millis(300),
                Duration::from_millis(10),
                Duration::ZERO,
            )
        };
        let state = state::load(&b.plan.state_path).unwrap();
        (outcome, state, cell.into_inner())
    }

    fn fake(b: &Bench) -> Fake {
        Fake {
            binary: b.plan.swaps[0].target.clone(),
            running: "0.6.7.1".into(),
            tunnel: true,
            restarts: 0,
            silent: 1,
        }
    }

    #[test]
    fn a_good_binary_is_swapped_restarted_and_confirmed() {
        let b = bench("ok", "0.6.8.2\n");
        let (outcome, state, fake) = run(&b, fake(&b));
        assert_eq!(outcome, Outcome::Ok);
        assert_eq!(state.phase, Phase::Done);
        assert_eq!(state.outcome, Some(Outcome::Ok));
        assert_eq!(fake.restarts, 1);
        assert_eq!(
            std::fs::read_to_string(&b.plan.swaps[0].target)
                .unwrap()
                .trim(),
            "0.6.8.2"
        );
        assert!(state.log_tail.unwrap().contains("health ok (2/2)"));
        let _ = std::fs::remove_dir_all(&b.dir);
    }

    #[test]
    fn a_binary_with_the_wrong_version_is_rolled_back() {
        let b = bench("wrong", "0.6.8.9\n");
        let (outcome, state, fake) = run(&b, fake(&b));
        assert_eq!(outcome, Outcome::RolledBack);
        assert_eq!(state.outcome, Some(Outcome::RolledBack));
        assert_eq!(fake.restarts, 2);
        assert_eq!(
            std::fs::read_to_string(&b.plan.swaps[0].target)
                .unwrap()
                .trim(),
            "0.6.7.1"
        );
        assert!(state.detail.unwrap().contains("did not answer health"));
        let _ = std::fs::remove_dir_all(&b.dir);
    }

    #[test]
    fn a_binary_that_does_not_come_up_is_rolled_back() {
        let b = bench("broken", "broken\n");
        let (outcome, _, fake) = run(&b, fake(&b));
        assert_eq!(outcome, Outcome::RolledBack);
        assert_eq!(fake.restarts, 2);
        let _ = std::fs::remove_dir_all(&b.dir);
    }

    #[test]
    fn a_tunnel_that_stays_down_is_a_rollback() {
        let b = bench("tunnel", "0.6.8.2\n");
        let mut f = fake(&b);
        f.tunnel = false;
        let (outcome, _, _) = run(&b, f);
        assert_eq!(outcome, Outcome::RolledBack);
        let _ = std::fs::remove_dir_all(&b.dir);
    }

    #[test]
    fn a_restore_that_does_not_come_back_needs_hands() {
        let mut b = bench("hands", "broken\n");
        // The backup is broken too: nothing the helper can do.
        std::fs::write(&b.plan.restores[0].backup, "broken\n").unwrap();
        b.plan.need_tunnel = false;
        let (outcome, state, _) = run(&b, fake(&b));
        assert_eq!(outcome, Outcome::NeedsHands);
        assert_eq!(state.outcome, Some(Outcome::NeedsHands));
        let _ = std::fs::remove_dir_all(&b.dir);
    }

    #[test]
    fn a_staged_file_that_changed_is_not_applied() {
        let b = bench("sha", "0.6.8.2\n");
        std::fs::write(&b.plan.swaps[0].staged, "tampered\n").unwrap();
        let (outcome, state, fake) = run(&b, fake(&b));
        assert_eq!(outcome, Outcome::NotApplied);
        assert_eq!(state.reason, Some(crate::upgrade::Reason::Sha256Mismatch));
        assert_eq!(fake.restarts, 0);
        assert_eq!(
            std::fs::read_to_string(&b.plan.swaps[0].target)
                .unwrap()
                .trim(),
            "0.6.7.1"
        );
        let _ = std::fs::remove_dir_all(&b.dir);
    }

    #[test]
    fn the_lock_is_exclusive() {
        let dir = std::env::temp_dir().join(format!("sctl-apply-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let held = take_lock(&dir).unwrap();
        assert!(!lock_is_free(&dir));
        drop(held);
        assert!(lock_is_free(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn health_parses_a_real_answer() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 512];
            let _ = s.read(&mut buf);
            let body = r#"{"status":"ok","version":"0.6.8.2","tunnel":{"connected":true}}"#;
            let _ = s.write_all(
                format!(
                    "HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        });
        let mut h = HttpHealth {
            url: format!("http://{addr}/api/health"),
        };
        assert_eq!(h.check().unwrap(), ("0.6.8.2".to_string(), true));
    }
}
