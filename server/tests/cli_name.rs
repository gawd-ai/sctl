//! The name the agent's tools run by (TRD-9 3.6).
//!
//! Where the agent runs from `/tmp`, the name is a guarded wrapper: it runs
//! the agent's files only while root alone can change them. Its guard is
//! run here by the system `sh` and, when BusyBox is installed, by BusyBox's
//! `sh`, the units' shell, with each path passed as an argument, never
//! inside a shell string. It passes a regular file and a directory that
//! root owns and no one else can write, and refuses a link, a file or a
//! directory others can write, one this account owns, a missing one and a
//! file named as a directory, saying why, with exit 126 and nothing run.
//! The whole wrapper runs root's own `env` as the loader and `printf` as
//! the binary, programs that only run the words after their options and
//! print their arguments, with arguments a shell would split, glob, expand
//! or run, passed as an argument vector: each arrives as it was given, and
//! nothing is expanded. A wrapper for a stand-in of this account's own, in
//! a scratch directory, runs nothing.
//!
//! `rundev.sh`'s paths that write `/usr/bin/sctl` first ask the unit whether
//! the name is the agent's: the question answers yes for the link and the
//! wrapper the agent writes, and no for a file whose marker line is past its
//! second line, a script's or the agent's own binary. `device deploy` and
//! `device upgrade` run under bash as they are, every call beyond bash stood
//! in by a function that records it and reaches nothing: they write only
//! after the unit answered plain, and ask again just before the first write.
//!
//! An agent started on the bench layout `gz-tmp` (`SCTL_INSTALL_JSON`) makes
//! the name where `SCTL_CLI_PATH` puts it, and `/api/health` names it as
//! `cli`. For a build of this account's own it is the guarded wrapper, which
//! refuses to run that build (exit 126, saying why); were the build root's
//! alone, `net tcp` to the agent's own port would answer through it. A
//! restart writes nothing, and a regular file at the name is left as it is.
//! With no `install.json`, or one it cannot use, it makes no name, and its
//! log says which. It listens on the loopback under a key of this run's
//! own, kept in a scratch directory only this account can read.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use sctl::upgrade::cli_name::{self, Want, GUARD, MARKER};
use serde_json::Value;

const SCTL: &str = env!("CARGO_BIN_EXE_sctl");
const PATH: &str = "/usr/bin:/bin";

/// A stand-in of this account's own: were it run, it would say so.
const RAN: &str = "#!/bin/sh\nprintf RAN\n";

/// Root's own programs as the stand-ins of a run: `env -u <name>` runs the
/// words after its option, as the loader runs the agent with its own, and
/// `printf '%s\0'` prints each argument it is given.
const ENV: &str = "/usr/bin/env";
const PRINTF: &str = "/usr/bin/printf";

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

fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
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

/// The system `sh`, and BusyBox's when it is installed.
fn shells() -> Vec<Option<PathBuf>> {
    let mut shells = vec![None];
    if let Some(busybox) = busybox() {
        shells.push(Some(busybox));
    }
    shells
}

/// `sh`, the system's or BusyBox's, with an environment of `PATH` alone.
fn sh(busybox: Option<&Path>) -> Command {
    let mut command = match busybox {
        Some(busybox) => {
            let mut c = Command::new(busybox);
            c.arg("sh");
            c
        }
        None => Command::new("sh"),
    };
    command.env_clear().env("PATH", PATH).stdin(Stdio::null());
    command
}

/// Whether the guard's rule holds for `path`: not a link, owned by root,
/// writable by neither its group nor others.
fn roots(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .is_ok_and(|m| !m.file_type().is_symlink() && m.uid() == 0 && m.mode() & 0o022 == 0)
}

/// Root's own `env` and `printf`, when the guard passes them and their
/// directory, as it passes the agent's files on a unit.
fn root_s_stand_ins() -> Option<(PathBuf, PathBuf)> {
    let (env, printf) = (PathBuf::from(ENV), PathBuf::from(PRINTF));
    if [&env, &printf, &PathBuf::from("/usr/bin")]
        .iter()
        .all(|path| roots(path))
    {
        Some((env, printf))
    } else {
        eprintln!("{ENV} and {PRINTF} are not root's alone here: no run of the wrapper to show");
        None
    }
}

/// What the guard answers for `path` taken as a `kind`: the exit code, and
/// what it printed on stdout and stderr. The script is a constant: the
/// guard, one check of `$1` as `$2`, and a word that it passed.
fn guard_answers(busybox: Option<&Path>, path: &Path, kind: &str) -> (i32, String, String) {
    let script = format!("{GUARD}root_only \"$1\" \"$2\"\nprintf 'passed\\n'\n");
    let out = sh(busybox)
        .arg("-c")
        .arg(&script)
        .arg("sh")
        .arg(path)
        .arg(kind)
        .output()
        .expect("the shell runs");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The guard passes what root alone can change, and refuses anything else
/// before the wrapper runs it, saying why.
#[test]
fn the_guard_passes_only_what_root_alone_can_change() {
    let scratch = Scratch::new("guard");
    // A directory a shell would split at its space, end a quote at its
    // quote and expand at its `$(...)`.
    let dir = scratch.path("it's a $(printf EXPANDED) dir");
    std::fs::create_dir(&dir).unwrap();
    let mine = dir.join("mine");
    executable(&mine, RAN);
    let open_file = dir.join("anyone may write it");
    executable(&open_file, RAN);
    set_mode(&open_file, 0o777);
    let open_dir = dir.join("anyone may write in it");
    std::fs::create_dir(&open_dir).unwrap();
    set_mode(&open_dir, 0o777);
    let link_to_file = dir.join("a link to printf");
    std::os::unix::fs::symlink(PRINTF, &link_to_file).unwrap();
    let link_to_dir = dir.join("a link to usr-bin");
    std::os::unix::fs::symlink("/usr/bin", &link_to_dir).unwrap();
    let mine_is_root_s = std::fs::metadata(&mine).unwrap().uid() == 0;

    let file = "regular file";
    let mut refused = vec![
        (link_to_file.clone(), file),
        (link_to_dir.clone(), "directory"),
        (open_file.clone(), file),
        (open_dir.clone(), "directory"),
        (dir.join("missing"), file),
        (mine.clone(), "directory"),
    ];
    if !mine_is_root_s {
        refused.push((mine.clone(), file));
        refused.push((dir.clone(), "directory"));
    }
    for shell in shells() {
        let shell = shell.as_deref();
        if root_s_stand_ins().is_some() {
            for (path, kind) in [
                (Path::new(PRINTF), file),
                (Path::new("/usr/bin"), "directory"),
            ] {
                assert_eq!(
                    guard_answers(shell, path, kind),
                    (0, "passed\n".to_string(), String::new()),
                    "{shell:?}: {}",
                    path.display()
                );
            }
        }
        for (path, kind) in &refused {
            let (code, out, err) = guard_answers(shell, path, kind);
            assert_eq!(
                (code, out.as_str()),
                (126, ""),
                "{shell:?}: {}: {err}",
                path.display()
            );
            assert_eq!(
                err,
                format!(
                    "sctl: not run: {} is not a {kind} owned by root and writable by no one else\n",
                    path.display()
                ),
                "{shell:?}: the path as given, nothing expanded"
            );
        }
    }
}

/// The wrapper, as the agent writes it, runs root's own programs through a
/// loader's words with every argument as it was given. The kernel runs it
/// through its `#!/bin/sh`, and so do the system `sh` and BusyBox's.
#[test]
fn the_wrapper_runs_root_s_programs_with_every_word_as_given() {
    let Some((env, printf)) = root_s_stand_ins() else {
        return;
    };
    let scratch = Scratch::new("wrapper");
    let words = vec![
        env.display().to_string(),
        "-u".to_string(),
        "SCTL_NO_SUCH_NAME".to_string(),
    ];
    let wrapper = scratch.path("sctl");
    let made = cli_name::make(&wrapper, &Want::wrapper(&words, &printf)).unwrap();
    assert!(made.written);
    // The kernel runs a copy that `cp` wrote: an exec of a file this
    // process wrote fails (ETXTBSY) while a child another test forked
    // meanwhile still holds the descriptor it inherited, and this process
    // never opens the copy for writing.
    let copy = scratch.path("sctl-copy");
    let copied = Command::new("cp")
        .arg(&wrapper)
        .arg(&copy)
        .env_clear()
        .env("PATH", PATH)
        .status()
        .expect("cp runs");
    assert!(copied.success());
    assert_eq!(
        std::fs::read(&copy).unwrap(),
        std::fs::read(&wrapper).unwrap()
    );
    let mut runs: Vec<(String, Command)> = vec![("the kernel".into(), Command::new(&copy))];
    for shell in shells() {
        let mut command = sh(shell.as_deref());
        command.arg(&wrapper);
        runs.push((format!("{shell:?}"), command));
    }
    for (by, mut command) in runs {
        let out = command
            .arg("%s\\0")
            .args(ARGS)
            .env_clear()
            .env("PATH", PATH)
            .env("HOME", "/nonexistent-home")
            .current_dir(&scratch.0)
            .output()
            .unwrap_or_else(|e| panic!("{by}: the wrapper runs: {e}"));
        assert!(
            out.status.success() && out.stderr.is_empty(),
            "{by}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let printed: Vec<String> = out
            .stdout
            .split(|&b| b == 0)
            .map(|field| String::from_utf8(field.to_vec()).unwrap())
            .collect();
        let mut expected: Vec<String> = ARGS.iter().map(ToString::to_string).collect();
        expected.push(String::new());
        assert_eq!(
            printed, expected,
            "{by}: every word arrives as it was given"
        );
        assert!(
            !printed.iter().any(|a| a == "EXPANDED"),
            "{by}: nothing was expanded"
        );
    }
}

/// A wrapper for a stand-in this account owns, or that anyone may write,
/// as a unit's would be for a binary another account put under /tmp, runs
/// nothing: exit 126, saying why.
#[test]
fn a_wrapper_for_a_binary_root_cannot_trust_runs_nothing() {
    let scratch = Scratch::new("untrusted");
    let binary = scratch.path("sctl-server");
    executable(&binary, RAN);
    // Writable by anyone: refused whoever runs this test.
    set_mode(&binary, 0o777);
    let wrapper = scratch.path("sctl");
    cli_name::make(&wrapper, &Want::wrapper(&[], &binary)).unwrap();
    for shell in shells() {
        let out = sh(shell.as_deref())
            .arg(&wrapper)
            .arg("net")
            .output()
            .expect("the wrapper runs");
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(126), "{shell:?}: {err}");
        assert!(out.stdout.is_empty(), "{shell:?}: nothing ran");
        assert!(
            err.starts_with(&format!("sctl: not run: {}", scratch.0.display())),
            "{shell:?}: {err}"
        );
    }
}

// ─── rundev.sh's question ───────────────────────────────────────────────

/// `rundev.sh`, as it is.
fn rundev_text() -> String {
    std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../rundev.sh"))
        .expect("rundev.sh")
}

/// `rundev.sh`'s `AGENT_CLI_TEST`: the condition it runs on a unit.
fn agent_cli_test(rundev: &str) -> &str {
    let test = rundev
        .lines()
        .find_map(|line| line.strip_prefix("AGENT_CLI_TEST='")?.strip_suffix('\''))
        .expect("rundev.sh defines AGENT_CLI_TEST");
    assert!(
        test.contains(MARKER),
        "rundev.sh asks for the agent's marker"
    );
    test
}

/// `rundev.sh`'s question, as its paths that write `/usr/bin/sctl` ask it
/// on a unit, with `/usr/bin/sctl` moved to `name`: a constant of
/// `rundev.sh` that only tests, reads and prints, and a scratch path the
/// shell takes as it is.
fn rundev_question(name: &Path) -> String {
    let rundev = rundev_text();
    let test = agent_cli_test(&rundev);
    let name = name.to_str().unwrap();
    assert!(
        name.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b)),
        "a path no shell splits or expands: {name}"
    );
    question(&test.replace("/usr/bin/sctl", name))
}

/// The question `rundev.sh` sends: it answers `agent-cli` or `plain`, so a
/// unit that answers neither has not answered.
fn question(test: &str) -> String {
    format!("if {test}; then echo agent-cli; else echo plain; fi")
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
/// the link and the wrapper the agent writes, and no for a file that holds
/// the marker line only past its second line: only a file's first two lines
/// are read. A script whose third line is the marker shows it whatever the
/// build; the agent's own binary, which in this build holds the marker as a
/// line of its own too, is the case met on a unit.
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
    // A script of a person's own whose third line is the marker. A read of
    // the whole file finds that line (grep, as an argument vector), so only
    // the two-line read answers plain.
    let third_line = scratch.path("third-line");
    std::fs::write(
        &third_line,
        format!("#!/bin/sh\n# a script of one's own\n{MARKER}\n"),
    )
    .unwrap();
    let whole_file = Command::new("grep")
        .args(["-qxF", MARKER])
        .arg(&third_line)
        .env_clear()
        .env("PATH", PATH)
        .status()
        .expect("grep runs");
    assert!(whole_file.success(), "the third line is the marker, whole");
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
            ask(shell, &rundev_question(&third_line)),
            "plain",
            "{shell:?}: the marker on a third line"
        );
        assert_eq!(
            ask(shell, &rundev_question(&binary)),
            "plain",
            "{shell:?}: the agent's binary"
        );
    }
}

// ─── rundev.sh's paths that write /usr/bin/sctl ─────────────────────────

/// Stand-ins for everything `do_device_deploy` and `do_device_upgrade` call
/// beyond bash's builtins and the functions taken from `rundev.sh`. Each
/// records its call in `$CALLS` and runs nothing: ssh and scp reach no
/// host, the build builds nothing. `PATH` names an empty directory, so a
/// call with no stand-in fails rather than reaches anything. ssh answers a
/// command that asks for the agent's name (one that holds `echo agent-cli`)
/// with the next line of `$REPLIES`: `fail` exits 255 with no output, as a
/// connection that drops; `silent` exits 0 with none; anything else is
/// printed. It answers any other command with nothing.
const STAND_INS: &str = r#"
log()  { printf 'log %s\n' "$*" >> "$CALLS"; }
ok()   { printf 'ok %s\n' "$*" >> "$CALLS"; }
warn() { printf 'warn %s\n' "$*" >> "$CALLS"; }
err()  { printf 'err %s\n' "$*" >> "$CALLS"; }
require_jq() { :; }
ensure_config() { :; }
cfg_device_exists() { :; }
cfg_device_get() {
    case $2 in
        host) echo 192.0.2.10 ;;
        arch) echo mipsel ;;
        url) echo http://192.0.2.10:1337 ;;
        api_key) echo bench ;;
    esac
}
arch_to_target() { echo mipsel-unknown-linux-musl; }
arch_to_bin() { echo "$SCRATCH/sctl-built"; }
device_comms_provider() { :; }
build_comms_provider() { :; }
upload_comms_provider() { :; }
deploy_playbooks_to_device() { :; }
install_json_usr_bin() { printf '{"v":1,"layout":"usr-bin","target":"mipsel_24kc"}'; }
cargo() { printf 'build cargo\n' >> "$CALLS"; }
cross() { printf 'build cross\n' >> "$CALLS"; }
curl() { return 7; }
jq() { return 1; }
sleep() { :; }
seq() { local i=$1; while [ "$i" -le "$2" ]; do echo "$i"; i=$((i + 1)); done; }
scp() { printf 'scp %s\n' "${!#}" >> "$CALLS"; }
ssh() {
    local command=${!#} reply replies
    printf 'ssh %s\n' "$command" >> "$CALLS"
    case $command in
        *'echo agent-cli'*)
            mapfile -t replies < "$REPLIES"
            reply=${replies[0]:-fail}
            printf '%s\n' "${replies[@]:1}" > "$REPLIES"
            case $reply in
                fail) return 255 ;;
                silent) return 0 ;;
                *) printf '%s\n' "$reply" ;;
            esac
            ;;
    esac
}
"#;

/// bash, which runs `rundev.sh`.
fn bash() -> PathBuf {
    std::env::var_os("PATH")
        .and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join("bash"))
                .find(|candidate| candidate.is_file())
        })
        .expect("bash, which runs rundev.sh")
}

/// One function of `rundev.sh`, as it is: from its `name() {` line to the
/// first line that is `}`.
fn rundev_function(rundev: &str, name: &str) -> String {
    let start = format!("{name}() {{");
    let mut text = String::new();
    for line in rundev.lines().skip_while(|line| *line != start) {
        text.push_str(line);
        text.push('\n');
        if line == "}" {
            return text;
        }
    }
    panic!("rundev.sh has no function {name}");
}

/// What a run of a `rundev.sh` device command did.
struct RundevRun {
    /// Whether it exited 0.
    ok: bool,
    /// Every call it made to a stand-in, in order.
    calls: Vec<String>,
}

impl RundevRun {
    /// Where the first call that begins with `prefix` is.
    fn find(&self, prefix: &str) -> Option<usize> {
        self.calls.iter().position(|call| call.starts_with(prefix))
    }

    /// Where every call that begins with `prefix` is.
    fn every(&self, prefix: &str) -> Vec<usize> {
        self.calls
            .iter()
            .enumerate()
            .filter(|(_, call)| call.starts_with(prefix))
            .map(|(i, _)| i)
            .collect()
    }
}

/// Run `function` (`do_device_deploy` or `do_device_upgrade`) for a unit
/// named bench under bash: it and `refuse_agent_cli_over_ssh` as they are in
/// `rundev.sh`, everything else stood in ([`STAND_INS`]), and ssh's answers
/// to the questions taken from `replies` in order.
fn rundev_run(case: &str, function: &str, replies: &[&str]) -> RundevRun {
    let rundev = rundev_text();
    let test_line = rundev
        .lines()
        .find(|line| line.starts_with("AGENT_CLI_TEST='"))
        .expect("rundev.sh defines AGENT_CLI_TEST");
    let script = format!(
        "set -euo pipefail\n{test_line}\n{STAND_INS}\n{}{}SCTL_DIR=$SCRATCH\nCONFIG_FILE=$SCRATCH/devices.json\n{function} bench\n",
        rundev_function(&rundev, "refuse_agent_cli_over_ssh"),
        rundev_function(&rundev, function),
    );
    let scratch = Scratch::new(case);
    let nothing = scratch.path("path");
    std::fs::create_dir(&nothing).unwrap();
    let mut queued = replies.join("\n");
    queued.push('\n');
    std::fs::write(scratch.path("replies"), queued).unwrap();
    let out = Command::new(bash())
        .arg("-c")
        .arg(&script)
        .env_clear()
        .env("PATH", &nothing)
        .env("CALLS", scratch.path("calls"))
        .env("REPLIES", scratch.path("replies"))
        .env("SCRATCH", &scratch.0)
        .current_dir(&scratch.0)
        .stdin(Stdio::null())
        .output()
        .expect("bash runs");
    let calls: Vec<String> = std::fs::read_to_string(scratch.path("calls"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("command not found"),
        "{case}: a call with no stand-in: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    RundevRun {
        ok: out.status.success(),
        calls,
    }
}

/// `device deploy` copies its binary to `/usr/bin/sctl` only after the unit
/// answered plain twice: before the build, and again just before the copy.
/// No answer, an empty one, another one or the agent's name stops it before
/// the build; the name made during the build, or no answer then, stops it
/// before the copy.
#[test]
fn rundev_device_deploy_writes_only_after_two_plain_answers() {
    let rundev = rundev_text();
    let asked = format!("ssh {}", question(agent_cli_test(&rundev)));

    let run = rundev_run("deploy-plain", "do_device_deploy", &["plain", "plain"]);
    assert!(run.ok, "{:?}", run.calls);
    let questions = run.every("ssh if ");
    assert_eq!(questions.len(), 2, "{:?}", run.calls);
    for &i in &questions {
        assert_eq!(run.calls[i], asked);
    }
    let built = run.find("build ").expect("built");
    let copied = run
        .find("scp root@192.0.2.10:/usr/bin/sctl")
        .expect("copied");
    assert!(
        questions[0] < built && built < questions[1] && questions[1] < copied,
        "{:?}",
        run.calls
    );
    assert_eq!(
        run.find("scp "),
        Some(copied),
        "the binary is the first write"
    );

    for (case, reply) in [
        ("deploy-fail", "fail"),
        ("deploy-silent", "silent"),
        ("deploy-garbled", "yes"),
        ("deploy-agent-cli", "agent-cli"),
    ] {
        let run = rundev_run(case, "do_device_deploy", &[reply]);
        assert!(!run.ok, "{case}: {:?}", run.calls);
        assert_eq!(run.find("build "), None, "{case}: {:?}", run.calls);
        assert_eq!(run.find("scp "), None, "{case}: {:?}", run.calls);
    }
    for (case, reply) in [("deploy-made", "agent-cli"), ("deploy-lost", "fail")] {
        let run = rundev_run(case, "do_device_deploy", &["plain", reply]);
        assert!(!run.ok, "{case}: {:?}", run.calls);
        assert!(run.find("build ").is_some(), "{case}: {:?}", run.calls);
        assert_eq!(run.find("scp "), None, "{case}: {:?}", run.calls);
    }
}

/// `device upgrade` stops the agent and removes `/usr/bin/sctl` only in a
/// command that asks first and touches nothing when the name is the
/// agent's, and copies its binary only after that command said it stopped.
/// No answer before the build stops it there; the agent's name, or no
/// answer, at the stop leaves it before the copy.
#[test]
fn rundev_device_upgrade_writes_only_after_a_plain_answer_and_its_own_stop() {
    let rundev = rundev_text();
    let test = agent_cli_test(&rundev);
    let stop = format!("ssh if {test}; then echo agent-cli; exit 0; fi; ");

    let run = rundev_run("upgrade-plain", "do_device_upgrade", &["plain", "stopped"]);
    assert!(run.ok, "{:?}", run.calls);
    assert_eq!(
        run.calls[run.find("ssh if ").unwrap()],
        format!("ssh {}", question(test))
    );
    let built = run.find("build ").expect("built");
    let stopped = run.find(&stop).expect("the stop asks first");
    let removal = run.calls[stopped].find("rm -f /usr/bin/sctl").unwrap();
    assert!(stop.len() <= removal, "{}", run.calls[stopped]);
    let copied = run
        .find("scp root@192.0.2.10:/usr/bin/sctl")
        .expect("copied");
    assert!(built < stopped && stopped < copied, "{:?}", run.calls);
    let removals: Vec<_> = run
        .calls
        .iter()
        .filter(|call| call.contains("rm -f /usr/bin/sctl"))
        .collect();
    assert_eq!(removals.len(), 1, "only the stop removes the name");

    for (case, reply) in [
        ("upgrade-fail", "fail"),
        ("upgrade-silent", "silent"),
        ("upgrade-agent-cli", "agent-cli"),
    ] {
        let run = rundev_run(case, "do_device_upgrade", &[reply]);
        assert!(!run.ok, "{case}: {:?}", run.calls);
        assert_eq!(run.find("build "), None, "{case}: {:?}", run.calls);
        assert!(
            !run.calls.iter().any(|call| call.contains("rm -f")),
            "{case}: {:?}",
            run.calls
        );
        assert_eq!(run.find("scp "), None, "{case}: {:?}", run.calls);
    }
    for (case, reply) in [
        ("upgrade-made", "agent-cli"),
        ("upgrade-lost", "fail"),
        ("upgrade-unconfirmed", "silent"),
    ] {
        let run = rundev_run(case, "do_device_upgrade", &["plain", reply]);
        assert!(!run.ok, "{case}: {:?}", run.calls);
        assert!(run.find(&stop).is_some(), "{case}: {:?}", run.calls);
        assert_eq!(run.find("scp "), None, "{case}: {:?}", run.calls);
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

/// After one start the name is made and health names it: for a build of
/// this account's own, the guarded wrapper, which refuses to run it; for
/// one root alone can change, a link, or the wrapper where its directory
/// is root's alone, either running the tools. A restart writes nothing; a
/// regular file at the name is left, and then health has no `cli`.
#[test]
fn a_started_agent_makes_the_name_and_health_names_it() {
    let bench = Bench::new("agent");
    let name = bench.name();
    let binary = std::fs::canonicalize(SCTL).unwrap();
    let dir = binary.parent().unwrap();

    let first = {
        let agent = Agent::start(&bench);
        let health = agent.health();
        assert_eq!(health["layout"], "gz-tmp", "{health}");
        assert_eq!(health["cli"], name.display().to_string(), "{health}");
        if cli_name::root_alone(&binary) {
            assert_eq!(std::fs::read_link(&name).unwrap(), binary);
        } else {
            let Want::Wrapper { text, .. } = Want::wrapper(&[], &binary) else {
                unreachable!()
            };
            assert_eq!(std::fs::read(&name).unwrap(), text, "the guarded wrapper");
        }

        // The tools, by the name: the agent's own port answers, when the
        // guard lets the build run.
        let out = bench
            .command(&name)
            .args(["net", "tcp", "127.0.0.1", &bench.port.to_string()])
            .output()
            .expect("the name runs");
        let runs = roots(dir) && roots(&binary);
        if runs {
            let said: Value = serde_json::from_slice(&out.stdout).unwrap();
            assert_eq!(out.status.code(), Some(0), "{said}");
            assert_eq!(said["open"], true, "{said}");
        } else {
            let err = String::from_utf8_lossy(&out.stderr);
            assert_eq!(out.status.code(), Some(126), "{err}");
            assert!(out.stdout.is_empty(), "nothing ran: {err}");
            assert!(
                err.starts_with("sctl: not run: ") && err.contains("owned by root"),
                "{err}"
            );
        }

        // install-info, by the name where it runs the build, reads it and
        // writes nothing.
        let by = if runs {
            name.clone()
        } else {
            PathBuf::from(SCTL)
        };
        let (report, _) = bench.install_info(&by);
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

/// With no `install.json` the agent makes no name and its log says there is
/// none; with one it cannot use, its log says that instead, never that there
/// is none.
#[test]
fn the_log_says_why_there_is_no_install_json_to_name_by() {
    let bench = Bench::new("no-install");
    let install_json = bench.scratch.path("install.json");
    for (text, why, not) in [
        (
            None,
            "No install.json: the agent makes no name for its tools",
            "unusable",
        ),
        (
            Some("{\"v\":2,\"layout\":\"gz-tmp\"}\n"),
            "No name for the agent's tools: install.json is unusable",
            "No install.json",
        ),
        (
            Some("not json\n"),
            "No name for the agent's tools: install.json is unusable",
            "No install.json",
        ),
    ] {
        match text {
            Some(text) => std::fs::write(&install_json, text).unwrap(),
            None => std::fs::remove_file(&install_json).unwrap(),
        }
        let agent = Agent::start(&bench);
        let health = agent.health();
        assert!(health.get("cli").is_none(), "{health}");
        assert!(health["layout"].is_null(), "{health}");
        let said = agent.said();
        assert!(said.contains(why), "{text:?}: {said}");
        assert!(!said.contains(not), "{text:?}: {said}");
    }
    assert!(
        std::fs::symlink_metadata(bench.name()).is_err(),
        "no name made"
    );
}
