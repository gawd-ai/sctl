//! The name the agent's tools run by (TRD-9 3.6).
//!
//! The wrapper the agent writes at `/usr/bin/sctl` on a unit that runs its
//! agent under a loader is run here by the system `sh` and, when BusyBox is
//! installed, by BusyBox's `sh`, the WE826's shell. It runs a stand-in
//! binary through a stand-in loader, both named by absolute path in a
//! scratch directory whose name a shell would split and expand, and both
//! only print their arguments (the loader then runs the binary, as a loader
//! does). Every argument is passed as an argument vector, never through a
//! shell string, and none names a command that acts: if one were expanded,
//! it would only print EXPANDED.
//!
//! `rundev.sh`'s paths that write `/usr/bin/sctl` first ask the unit whether
//! the name is the agent's: the question answers yes for the link and the
//! wrapper the agent writes, and no for the agent's own binary.
//!
//! An agent started on the bench layout `gz-tmp` (`SCTL_INSTALL_JSON`) makes
//! the name where `SCTL_CLI_PATH` puts it, `/api/health` names it as `cli`,
//! `net tcp` to the agent's own port answers through it, a restart writes
//! nothing, and a regular file at the name is left as it is. It listens on
//! the loopback under a key of this run's own, kept in a scratch directory
//! only this account can read.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use sctl::upgrade::cli_name::{self, Want, MARKER};
use serde_json::Value;

const SCTL: &str = env!("CARGO_BIN_EXE_sctl");
const PATH: &str = "/usr/bin:/bin";

/// The stand-in loader: it prints its arguments, then runs the binary with
/// the words after its two options, as the musl loader runs the agent.
const LOADER: &str = "#!/bin/sh\n\
printf 'loader\\0%s\\0' \"$#\"\n\
for a do printf '%s\\0' \"$a\"; done\n\
shift 2\n\
exec \"$@\"\n";

/// The stand-in binary: it prints its arguments.
const BINARY: &str = "#!/bin/sh\n\
printf 'binary\\0%s\\0' \"$#\"\n\
for a do printf '%s\\0' \"$a\"; done\n";

/// What a shell would split, glob, expand or run, were it given the chance.
const ARGS: [&str; 15] = [
    "two words",
    " leading and trailing ",
    "it's",
    "\"double quoted\"",
    "*",
    "/*",
    ";",
    "a; b",
    "`printf EXPANDED`",
    "$(printf EXPANDED)",
    "$HOME",
    "",
    "\\n\\t",
    "-e",
    "--config=/etc/sctl/sctl.toml",
];

/// A scratch directory, removed on drop. It is private (0700) and made
/// here: a directory another account made at its name first is refused,
/// since the scripts and the agent's key written into it are this run's.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let dir = std::env::temp_dir().join(format!("sctl-cli-name-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{}: private", dir.display());
        Self(dir)
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn executable(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// busybox, when installed.
fn busybox() -> Option<PathBuf> {
    let found = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join("busybox"))
            .find(|candidate| candidate.is_file())
    });
    if found.is_none() {
        eprintln!("busybox is not installed: only the system sh runs the wrapper");
    }
    found
}

/// The records the stand-ins printed: each its role and its arguments.
fn records(out: &[u8]) -> Vec<(String, Vec<String>)> {
    let mut fields = out
        .split(|&b| b == 0)
        .map(|f| String::from_utf8(f.to_vec()).unwrap());
    let mut records = Vec::new();
    while let Some(role) = fields.next() {
        if role.is_empty() {
            break;
        }
        let count: usize = fields.next().unwrap().parse().unwrap();
        let args = (0..count).map(|_| fields.next().unwrap()).collect();
        records.push((role, args));
    }
    assert!(fields.next().is_none(), "nothing after the records");
    records
}

/// Write the wrapper as the agent does, run it with `shell` (`None`: the
/// kernel runs it, through its `#!/bin/sh`), and check what reached the
/// loader and the binary.
fn check_wrapper(busybox: Option<&Path>, name: &str) {
    let scratch = Scratch::new(name);
    // A directory a shell would split at its space, end a quote at its quote
    // and expand at its `$(...)`.
    let dir = scratch.path("it's a $(printf EXPANDED) dir");
    std::fs::create_dir_all(dir.join("lib")).unwrap();
    let loader = dir.join("loader");
    let binary = dir.join("binary");
    executable(&loader, LOADER);
    executable(&binary, BINARY);
    let words = vec![
        loader.display().to_string(),
        "--library-path".to_string(),
        dir.join("lib").display().to_string(),
    ];
    let wrapper = dir.join("sctl");
    let made = cli_name::make(&wrapper, &Want::wrapper(&words, &binary)).unwrap();
    assert!(made.written);

    let mut command = match busybox {
        Some(busybox) => {
            let mut c = Command::new(busybox);
            c.arg("sh").arg(&wrapper);
            c
        }
        None => Command::new(&wrapper),
    };
    let out = command
        .args(ARGS)
        .env_clear()
        .env("PATH", PATH)
        .env("HOME", "/nonexistent-home")
        .current_dir(&dir)
        .output()
        .expect("the wrapper runs");
    assert!(
        out.status.success(),
        "{name}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stderr.is_empty(),
        "{name}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut through_loader = words[1..].to_vec();
    through_loader.push(binary.display().to_string());
    through_loader.extend(ARGS.iter().map(ToString::to_string));
    let args: Vec<String> = ARGS.iter().map(ToString::to_string).collect();
    let got = records(&out.stdout);
    assert_eq!(
        got,
        [
            ("loader".to_string(), through_loader),
            ("binary".to_string(), args)
        ],
        "{name}: every word arrives as it was given"
    );
    assert!(
        !got.iter().flat_map(|(_, a)| a).any(|a| a == "EXPANDED"),
        "{name}: nothing was expanded"
    );
}

#[test]
fn the_wrapper_runs_the_binary_through_its_loader_with_the_system_sh() {
    check_wrapper(None, "sh");
}

#[test]
fn the_wrapper_runs_the_binary_through_its_loader_with_busybox() {
    if let Some(busybox) = busybox() {
        check_wrapper(Some(&busybox), "busybox");
    }
}

// ─── rundev.sh's question ───────────────────────────────────────────────

/// `rundev.sh`'s `AGENT_CLI_TEST`, as its paths that write `/usr/bin/sctl`
/// ask it on a unit first, with `/usr/bin/sctl` moved to `name`. The one
/// shell string here: a constant of `rundev.sh` that only tests, reads and
/// prints, and a scratch path the shell takes as it is.
fn rundev_question(name: &Path) -> String {
    let rundev = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../rundev.sh"))
        .expect("rundev.sh");
    let test = rundev
        .lines()
        .find_map(|line| line.strip_prefix("AGENT_CLI_TEST='")?.strip_suffix('\''))
        .expect("rundev.sh defines AGENT_CLI_TEST");
    assert!(
        test.contains(MARKER),
        "rundev.sh asks for the agent's marker"
    );
    let name = name.to_str().unwrap();
    assert!(
        name.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b)),
        "a path no shell splits or expands: {name}"
    );
    format!(
        "if {}; then echo agent-cli; else echo plain; fi",
        test.replace("/usr/bin/sctl", name)
    )
}

fn ask(busybox: Option<&Path>, question: &str) -> String {
    let mut command = match busybox {
        Some(busybox) => {
            let mut c = Command::new(busybox);
            c.arg("sh");
            c
        }
        None => Command::new("sh"),
    };
    let out = command
        .arg("-c")
        .arg(question)
        .env_clear()
        .env("PATH", PATH)
        .output()
        .expect("the shell runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// What `rundev.sh` asks before it writes `/usr/bin/sctl` answers yes for
/// the link and the wrapper the agent writes, and no for the agent's own
/// binary, which holds the marker line too: only a file's first two lines
/// are read.
#[test]
fn rundev_knows_the_agents_name_and_not_its_binary() {
    let scratch = Scratch::new("rundev-question");
    let name = scratch.path("sctl");
    let binary = std::fs::canonicalize(SCTL).unwrap();
    let words = vec![
        "/tmp/sctl/lib/libc.so".to_string(),
        "--library-path".to_string(),
        "/tmp/sctl/lib".to_string(),
    ];
    let busybox = busybox();
    let mut shells = vec![None];
    if let Some(busybox) = busybox.as_deref() {
        shells.push(Some(busybox));
    }
    for shell in shells {
        let _ = std::fs::remove_file(&name);
        assert_eq!(
            ask(shell, &rundev_question(&name)),
            "plain",
            "{shell:?}: no name"
        );
        cli_name::make(&name, &Want::Link(binary.clone())).unwrap();
        assert_eq!(
            ask(shell, &rundev_question(&name)),
            "agent-cli",
            "{shell:?}: the link"
        );
        std::fs::remove_file(&name).unwrap();
        cli_name::make(
            &name,
            &Want::wrapper(&words, Path::new("/tmp/sctl/sctl-server")),
        )
        .unwrap();
        assert_eq!(
            ask(shell, &rundev_question(&name)),
            "agent-cli",
            "{shell:?}: the wrapper"
        );
        assert_eq!(
            ask(shell, &rundev_question(&binary)),
            "plain",
            "{shell:?}: the agent's binary"
        );
    }
}

// ─── a started agent ────────────────────────────────────────────────────

/// A bench unit: a config, an `install.json` staging the `gz-tmp` layout,
/// and a directory for the name. The agent's routes that run commands
/// answer only its key, which is this run's own and sits in the private
/// scratch directory: the test itself uses only the routes that need none.
struct Bench {
    scratch: Scratch,
    port: u16,
}

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

impl Bench {
    fn new(name: &str) -> Self {
        let scratch = Scratch::new(name);
        for dir in ["data", "state", "bin"] {
            std::fs::create_dir_all(scratch.path(dir)).unwrap();
        }
        // A free port on the loopback for the agent.
        let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        std::fs::write(
            scratch.path("sctl.toml"),
            format!(
                "[server]\nlisten = \"127.0.0.1:{port}\"\ndata_dir = \"{}\"\nstate_dir = \"{}\"\n\n\
                 [auth]\napi_key = \"{}\"\n",
                scratch.path("data").display(),
                scratch.path("state").display(),
                run_key()
            ),
        )
        .unwrap();
        std::fs::write(
            scratch.path("install.json"),
            "{\"v\":1,\"layout\":\"gz-tmp\"}\n",
        )
        .unwrap();
        Self { scratch, port }
    }

    fn name(&self) -> PathBuf {
        self.scratch.path("bin/sctl")
    }

    /// A command with the bench's environment and nothing else.
    fn command(&self, program: &Path) -> Command {
        let mut c = Command::new(program);
        c.env_clear()
            .env("PATH", PATH)
            .env("SCTL_INSTALL_JSON", self.scratch.path("install.json"))
            .env("SCTL_CLI_PATH", self.name())
            .stdin(Stdio::null());
        c
    }

    /// `install-info` by `program`: its report, and what it said on stderr.
    fn install_info(&self, program: &Path) -> (Value, String) {
        let out = self
            .command(program)
            .arg("install-info")
            .arg("--config")
            .arg(self.scratch.path("sctl.toml"))
            .output()
            .expect("install-info runs");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        (
            serde_json::from_slice(&out.stdout).unwrap(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }
}

/// `sctl serve` on the bench, killed on drop.
struct Agent<'a> {
    bench: &'a Bench,
    child: Child,
}

impl<'a> Agent<'a> {
    /// Start the agent and wait until it answers.
    fn start(bench: &'a Bench) -> Self {
        let child = bench
            .command(Path::new(SCTL))
            .arg("serve")
            .arg("--config")
            .arg(bench.scratch.path("sctl.toml"))
            .stdout(std::fs::File::create(bench.scratch.path("agent.log")).unwrap())
            .stderr(std::fs::File::create(bench.scratch.path("agent.err")).unwrap())
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
            .map(|f| std::fs::read_to_string(self.bench.scratch.path(f)).unwrap_or_default())
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

    fn health(&self) -> Value {
        self.try_health().expect("health answers")
    }
}

impl Drop for Agent<'_> {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The entry's inode and modification time: the entry, not what it names.
fn stamp(path: &Path) -> (u64, i64, i64) {
    let meta = std::fs::symlink_metadata(path).unwrap();
    (meta.ino(), meta.mtime(), meta.mtime_nsec())
}

/// After one start the name runs the agent's tools and health names it; a
/// restart writes nothing; a regular file at the name is left, and then
/// health has no `cli`.
#[test]
fn a_started_agent_makes_the_name_and_health_names_it() {
    let bench = Bench::new("agent");
    let name = bench.name();
    let binary = std::fs::canonicalize(SCTL).unwrap();

    let first = {
        let agent = Agent::start(&bench);
        let health = agent.health();
        assert_eq!(health["layout"], "gz-tmp", "{health}");
        assert_eq!(health["cli"], name.display().to_string(), "{health}");
        assert_eq!(std::fs::read_link(&name).unwrap(), binary);

        // The tools, by the name: the agent's own port answers.
        let out = bench
            .command(&name)
            .args(["net", "tcp", "127.0.0.1", &bench.port.to_string()])
            .output()
            .expect("the tools run by the name");
        let said: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(out.status.code(), Some(0), "{said}");
        assert_eq!(said["open"], true, "{said}");

        // install-info, by the name, reads it and writes nothing.
        let (report, _) = bench.install_info(&name);
        assert_eq!(report["layout"], "gz-tmp");
        assert_eq!(report["cli"], name.display().to_string(), "{report}");
        stamp(&name)
    };

    {
        let agent = Agent::start(&bench);
        assert_eq!(agent.health()["cli"], name.display().to_string());
    }
    assert_eq!(stamp(&name), first, "a restart writes nothing");
    let mut entries: Vec<_> = std::fs::read_dir(bench.scratch.path("bin"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    entries.sort();
    assert_eq!(entries, ["sctl"], "no temporary entry left");

    // A file of a person's own at the name: left, and no cli.
    std::fs::remove_file(&name).unwrap();
    std::fs::write(&name, "a person's own sctl\n").unwrap();
    {
        let agent = Agent::start(&bench);
        let health = agent.health();
        assert!(health.get("cli").is_none(), "{health}");
        let said = agent.said();
        assert!(said.contains("No name for the agent's tools"), "{said}");
    }
    assert_eq!(
        std::fs::read_to_string(&name).unwrap(),
        "a person's own sctl\n"
    );
    let (report, said) = bench.install_info(Path::new(SCTL));
    assert!(report.get("cli").is_none(), "{report}");
    assert!(said.contains("no cli:"), "{said}");
}
