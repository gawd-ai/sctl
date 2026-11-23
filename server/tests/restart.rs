//! `POST /api/restart` with real processes and no relay (ADR-014, TRD-10
//! 3.7): an agent the test starts on the loopback, with no `[tunnel]`
//! section, and the helper that agent starts.
//!
//! Nothing is restarted here. The staged `install.json` names a restart
//! command that only writes a marker file in the test's scratch directory,
//! and a `health_url` on a loopback port where the test itself answers:
//! the agent's own `instance` until the marker exists, another after. So
//! the whole path runs (the route, the record, the detached helper, its
//! settle time, the command, two healthy answers 5 s apart, the outcome,
//! the agent's watch of the record) while the agent the test started stays
//! up until the test stops it.
//!
//! The test stops only the agent it started. It runs no init script and no
//! `systemctl`, and signals no process it did not start. `SCTL_CLI_PATH`
//! and `SCTL_DISABLED_PATH` keep the agent's hands off the host's
//! `/usr/bin/sctl` and `/etc/sctl`.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const SCTL: &str = env!("CARGO_BIN_EXE_sctl");
const PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
/// What the stand-in health answers once the restart command has run.
const NEW_INSTANCE: &str = "the-stand-in-for-a-new-process";

/// A key no other run shares: 16 bytes of `/dev/urandom`, in hex.
fn run_key() -> String {
    use std::fmt::Write as _;
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut bytes))
        .expect("/dev/urandom");
    bytes.iter().fold(String::with_capacity(32), |mut key, b| {
        let _ = write!(key, "{b:02x}");
        key
    })
}

fn free_port() -> u16 {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A bench unit in a scratch directory: its config, its staged install
/// record, and the stand-ins the record names.
struct Bench {
    dir: PathBuf,
    port: u16,
    key: String,
    /// Where the stand-in health answers.
    health_port: u16,
}

impl Bench {
    /// Stage a `usr-bin` unit. With `install` false there is no install
    /// record, as on a unit installed before 0.6.7.
    fn new(name: &str, install: bool) -> Self {
        let dir =
            std::env::temp_dir().join(format!("sctl-restart-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for sub in ["data", "state", "bin"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        let bench = Self {
            port: free_port(),
            key: run_key(),
            health_port: free_port(),
            dir,
        };
        std::fs::write(
            bench.path("sctl.toml"),
            format!(
                "[server]\nlisten = \"127.0.0.1:{}\"\ndata_dir = \"{}\"\nstate_dir = \"{}\"\n\n\
                 [auth]\napi_key = \"{}\"\n",
                bench.port,
                bench.path("data").display(),
                bench.path("state").display(),
                bench.key
            ),
        )
        .unwrap();
        // The restart command: a stand-in that writes a marker and stops
        // nothing.
        let script = bench.path("restart.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho restarted > '{}'\n",
                bench.marker().display()
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        if install {
            let record = json!({
                "v": 1,
                "layout": "usr-bin",
                "files": {"server": SCTL},
                "restart": script.display().to_string(),
                "health_url": format!("http://127.0.0.1:{}/api/health", bench.health_port),
            });
            std::fs::write(bench.path("install.json"), record.to_string()).unwrap();
        }
        bench
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }

    fn marker(&self) -> PathBuf {
        self.path("restarted.marker")
    }

    /// One request to the agent, with its key: the status and the body.
    fn request(&self, method: &str, path: &str, body: &str) -> (u16, Value) {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, self.port)).expect("the agent");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            self.key,
            body.len()
        )
        .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        let text = String::from_utf8_lossy(&response);
        let (head, body) = text.split_once("\r\n\r\n").expect("an HTTP response");
        let status = head
            .get(9..12)
            .and_then(|s| s.parse().ok())
            .expect("a status");
        (
            status,
            serde_json::from_str(body).unwrap_or_else(|_| json!({"raw": body})),
        )
    }

    fn restart_record(&self) -> Value {
        let (status, report) = self.request("GET", "/api/upgrade", "");
        assert_eq!(status, 200, "{report}");
        report["restart"].clone()
    }
}

impl Drop for Bench {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// `sctl serve` on the bench, stopped on drop: the one process the test
/// stops.
struct Agent<'a> {
    bench: &'a Bench,
    child: Child,
}

impl<'a> Agent<'a> {
    fn start(bench: &'a Bench) -> Self {
        let child = Command::new(SCTL)
            .env_clear()
            .env("PATH", PATH)
            .env("SCTL_INSTALL_JSON", bench.path("install.json"))
            .env("SCTL_CLI_PATH", bench.path("bin/sctl"))
            .env("SCTL_DISABLED_PATH", bench.path("disabled"))
            .stdin(Stdio::null())
            .arg("serve")
            .arg("--config")
            .arg(bench.path("sctl.toml"))
            .stdout(std::fs::File::create(bench.path("agent.log")).unwrap())
            .stderr(std::fs::File::create(bench.path("agent.err")).unwrap())
            .spawn()
            .expect("the agent starts");
        let mut agent = Self { bench, child };
        let started = Instant::now();
        while agent.try_health().is_err() {
            assert!(
                agent.child.try_wait().unwrap().is_none(),
                "the agent exited: {}",
                agent.said()
            );
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "the agent answers: {}",
                agent.said()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        agent
    }

    fn said(&self) -> String {
        ["agent.log", "agent.err"]
            .iter()
            .map(|f| std::fs::read_to_string(self.bench.path(f)).unwrap_or_default())
            .collect()
    }

    fn try_health(&self) -> std::io::Result<Value> {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, self.bench.port))?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.write_all(
            b"GET /api/health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        )?;
        let mut response = Vec::new();
        stream.read_to_end(&mut response)?;
        let text = String::from_utf8_lossy(&response);
        let body = text.split_once("\r\n\r\n").map_or("", |(_, body)| body);
        serde_json::from_str(body).map_err(std::io::Error::other)
    }

    /// The agent's `instance`, from the route that needs no key.
    fn instance(&self) -> String {
        self.try_health().expect("health answers")["instance"]
            .as_str()
            .expect("an instance")
            .to_string()
    }
}

impl Drop for Agent<'_> {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The health the helper reads, stood in by the test: `old` until the
/// marker exists, [`NEW_INSTANCE`] after. Stopped on drop.
struct StandInHealth {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<u32>>,
}

impl StandInHealth {
    fn serve(port: u16, old: String, marker: PathBuf) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).expect("the health port");
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut answered = 0;
            while !stopped.load(Ordering::SeqCst) {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = [0_u8; 512];
                let _ = stream.read(&mut request);
                let instance = if marker.exists() { NEW_INSTANCE } else { &old };
                let body = json!({"status": "ok", "instance": instance}).to_string();
                let _ = write!(
                    stream,
                    "HTTP/1.0 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                answered += 1;
            }
            answered
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }

    /// Stop, and say how many reads were answered.
    fn answered(mut self) -> u32 {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap()
    }
}

impl Drop for StandInHealth {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The request is answered before anything runs; the helper the agent
/// starts runs the restart command, reads health until another process has
/// answered twice, and writes the outcome; the agent reports it.
#[test]
fn a_restart_runs_its_command_through_a_real_helper_and_reports_the_outcome() {
    let bench = Bench::new("ok", true);
    let agent = Agent::start(&bench);
    let instance = agent.instance();
    assert_eq!(instance.len(), 32, "opaque: {instance}");
    let health = StandInHealth::serve(bench.health_port, instance.clone(), bench.marker());

    // Never asked: the record is there, idle. That is how a caller knows
    // the agent has the route.
    assert_eq!(bench.restart_record(), json!({"v": 1, "phase": "idle"}));

    let asked = Instant::now();
    let (status, answer) = bench.request("POST", "/api/restart", r#"{"request_id": "bench-1"}"#);
    assert_eq!(status, 202, "{answer}");
    assert_eq!(answer["accepted"], true);
    assert_eq!(answer["restart"]["request_id"], "bench-1");
    assert_eq!(answer["restart"]["instance_before"], instance);
    assert_eq!(answer["window_secs"], 180);
    assert_eq!(answer["second_run"], false);
    assert_eq!(answer["safe_mode"], false);
    assert_eq!(
        answer["ends"],
        json!({"sessions": 0, "attached": 0, "jobs": [], "transfers": 0})
    );
    // The answer left before the command ran: the helper waits first.
    assert!(!bench.marker().exists(), "answered before anything ran");

    // One in flight at a time.
    let (status, refused) = bench.request("POST", "/api/restart", "");
    assert_eq!(status, 409, "{refused}");
    assert_eq!(refused["code"], "RESTART_IN_FLIGHT");
    assert_eq!(refused["detail"]["restart"]["request_id"], "bench-1");

    // The helper: 2 s, the command, two answers 5 s apart.
    let mut record = bench.restart_record();
    while record["phase"] != "done" {
        assert!(
            asked.elapsed() < Duration::from_mins(1),
            "the helper ends the record: {record}\n{}",
            agent.said()
        );
        std::thread::sleep(Duration::from_millis(250));
        record = bench.restart_record();
    }
    assert!(bench.marker().exists(), "the restart command ran");
    assert_eq!(record["outcome"], "ok", "{record}");
    assert_eq!(record["attempts"], 1);
    assert_eq!(record["request_id"], "bench-1");
    assert_eq!(record["instance_before"], instance);
    assert_eq!(record["instance_after"], NEW_INSTANCE);
    // The process the helper proved is the stand-in, not this agent: no
    // agent wrote its boot.
    assert!(record.get("boot_after").is_none(), "{record}");
    assert!(
        record["log_tail"]
            .as_str()
            .is_some_and(|log| log.contains("restart exited 0") && log.contains("outcome: \"ok\"")),
        "{record}"
    );
    assert!(
        asked.elapsed() >= Duration::from_secs(6),
        "2 s, then two answers 5 s apart: {:?}",
        asked.elapsed()
    );
    // The helper read health; the agent the test started was never stopped.
    assert!(health.answered() >= 2);
    assert_eq!(agent.instance(), instance, "the same process still answers");

    // The record ended: the next request is taken again.
    std::fs::write(bench.path("disabled"), "").unwrap();
    let (status, refused) = bench.request("POST", "/api/restart", "");
    assert_eq!(status, 422, "{refused}");
    assert_eq!(refused["code"], "RESTART_REFUSED");
    assert_eq!(refused["detail"]["reason"], "disabled");
}

/// The kill switch refuses before anything runs.
#[test]
fn the_kill_switch_refuses_and_nothing_runs() {
    let bench = Bench::new("disabled", true);
    std::fs::write(bench.path("disabled"), "").unwrap();
    let agent = Agent::start(&bench);
    let (status, refused) = bench.request("POST", "/api/restart", r#"{"request_id": "bench-2"}"#);
    assert_eq!(status, 422, "{refused}");
    assert_eq!(refused["code"], "RESTART_REFUSED");
    assert_eq!(refused["detail"]["reason"], "disabled");
    assert_eq!(bench.restart_record(), json!({"v": 1, "phase": "idle"}));
    // Longer than the helper's settle time: nothing was started.
    std::thread::sleep(Duration::from_secs(3));
    assert!(!bench.marker().exists());
    assert!(
        !bench.path("state/upgrade/restart").exists(),
        "no helper's plan"
    );
    drop(agent);
}

/// A unit with no install record does not know how it is started: refused,
/// and its bad requests are refused as such.
#[test]
fn a_unit_with_no_install_record_refuses() {
    let bench = Bench::new("none", false);
    let agent = Agent::start(&bench);
    let (status, refused) = bench.request("POST", "/api/restart", "");
    assert_eq!(status, 422, "{refused}");
    assert_eq!(refused["detail"]["reason"], "no_install_json");
    let (status, refused) = bench.request("POST", "/api/restart", r#"{"unless": ["everything"]}"#);
    assert_eq!(status, 400, "{refused}");
    assert_eq!(refused["code"], "RESTART_BAD_REQUEST");
    // Without the key, nothing.
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, bench.port)).unwrap();
    stream
        .write_all(b"POST /api/restart HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    assert!(response.starts_with("HTTP/1.1 401"), "{response}");
    assert!(!Path::new(&bench.marker()).exists());
    drop(agent);
}

/// `sctl upgrade` from a shell is another process than the agent that
/// accepted a restart. It reads the restart helper's lock on disk and
/// refuses while it is held, before it stages or fetches anything.
#[test]
fn sctl_upgrade_from_a_shell_refuses_while_a_restarts_helper_is_at_work() {
    use std::os::unix::io::AsRawFd;
    let bench = Bench::new("cli", true);
    // What an agent and its helper leave on disk mid-restart. No agent runs
    // here: the files are the test's own.
    let upgrade = bench.path("state/upgrade");
    std::fs::create_dir_all(upgrade.join("restart")).unwrap();
    std::fs::write(
        upgrade.join("restart.json"),
        json!({"v": 1, "phase": "restarting", "request_id": "bench-9",
               "instance_before": "an-instance", "boot_before": 1_790_000_000_000_u64})
        .to_string(),
    )
    .unwrap();
    let lock = std::fs::File::create(upgrade.join("restart/lock")).unwrap();
    // SAFETY: flock on a file descriptor this test owns.
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0,
        "the helper's lock, held by the test"
    );

    let mut child = Command::new(SCTL)
        .env_clear()
        .env("PATH", PATH)
        .env("SCTL_INSTALL_JSON", bench.path("install.json"))
        .env("SCTL_CLI_PATH", bench.path("bin/sctl"))
        .env("SCTL_DISABLED_PATH", bench.path("disabled"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(bench.path("upgrade.err")).unwrap())
        .arg("upgrade")
        .arg("9.9.9.1")
        .arg("--config")
        .arg(bench.path("sctl.toml"))
        // Were it not refused, it would ask the loopback, where nothing
        // listens.
        .arg("--manifest")
        .arg("http://127.0.0.1:1/release.json")
        .spawn()
        .expect("sctl upgrade starts");
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > Duration::from_secs(20) {
            // The test's own child.
            let _ = child.kill();
            let _ = child.wait();
            panic!("sctl upgrade went on with a restart in flight");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let said = std::fs::read_to_string(bench.path("upgrade.err")).unwrap_or_default();
    assert_eq!(status.code(), Some(1), "{said}");
    assert!(
        said.contains("a restart is in flight (request bench-9)"),
        "{said}"
    );
    assert!(
        !upgrade.join("state.json").exists(),
        "nothing was staged: {said}"
    );
    drop(lock);
}
