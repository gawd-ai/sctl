//! The device tree, read as text (ADR-014 decision 5, TRD-10 3.6): a
//! `gz-tmp` init runs its own checks before its `restart` stops the running
//! agent, each device's install script writes the threshold its init
//! demands, and every kill switch is the path the agent checks.
//!
//! No script is run here. They name fixed paths under `/tmp` and `/etc`,
//! and a `gz-tmp` init removes its run directory. A script is read, and
//! parsed with `sh -n`, which runs nothing.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the workspace root")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let path = repo().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The body of the shell function `name` in `script`: from its opening
/// line to the first line that is a lone closing brace.
fn function<'a>(script: &'a str, name: &str) -> &'a str {
    let open = format!("\n{name}() {{\n");
    let start = script
        .find(&open)
        .unwrap_or_else(|| panic!("no function {name}"))
        + open.len();
    let end = script[start..]
        .find("\n}\n")
        .unwrap_or_else(|| panic!("{name} is not closed"));
    &script[start..start + end]
}

/// The lines of `body` that are commands: no blank line, no comment.
fn commands(body: &str) -> Vec<&str> {
    body.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect()
}

/// Where the first command that starts with `word` is.
fn first(commands: &[&str], word: &str) -> usize {
    commands
        .iter()
        .position(|c| c.split_whitespace().next() == Some(word))
        .unwrap_or_else(|| panic!("no command {word} in {commands:?}"))
}

/// The value of the assignment `NAME=value` at the start of a line.
fn assigned(script: &str, name: &str) -> Option<String> {
    script
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{name}=")))
        .map(|v| v.trim().to_string())
}

/// `restart_tmp_kb` in every `printf` that writes `install.json` in
/// `script`.
fn written_thresholds(script: &str) -> Vec<u64> {
    script
        .lines()
        .filter(|l| l.contains("> /etc/sctl/install.json") && l.contains("printf"))
        .map(|l| {
            let key = "\"restart_tmp_kb\":";
            let at = l
                .find(key)
                .unwrap_or_else(|| panic!("no restart_tmp_kb in: {l}"));
            l[at + key.len()..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .unwrap_or_else(|_| panic!("restart_tmp_kb is not a number in: {l}"))
        })
        .collect()
}

/// A `gz-tmp` device: its init, and the scripts that write its install
/// record.
const GZ_TMP: [(&str, &[&str]); 2] = [
    (
        "devices/xe300/sctl.init",
        &["devices/xe300/install.sh", "devices/xe300/upgrade.sh"],
    ),
    ("devices/rut241/sctl.init", &["devices/rut241/install.sh"]),
];

/// `restart` asks before it stops: with a check that fails, the running
/// agent is left running. `rc.common`'s own `restart` stops first, and with
/// `/tmp` short of room nothing then started the agent before the next
/// boot.
#[test]
fn a_gz_tmp_init_checks_before_its_restart_stops() {
    for (init, _) in GZ_TMP {
        let script = read(init);
        let restart = commands(function(&script, "restart"));
        let (checks, stop, start) = (
            first(&restart, "start_checks"),
            first(&restart, "stop"),
            first(&restart, "start"),
        );
        assert!(checks < stop && stop < start, "{init}: {restart:?}");
        // A failed check returns before the stop.
        let failed = first(&restart, "return");
        assert!(checks < failed && failed < stop, "{init}: {restart:?}");
        assert!(
            restart[checks].ends_with("|| {"),
            "{init}: a failed check must take the branch that returns: {restart:?}"
        );
    }
}

/// A start makes the same checks, before it removes the run directory:
/// one function, so the two cannot drift apart.
#[test]
fn a_gz_tmp_start_checks_before_it_removes_the_run_directory() {
    for (init, _) in GZ_TMP {
        let script = read(init);
        let start = commands(function(&script, "start_service"));
        assert!(
            first(&start, "start_checks") < first(&start, "rm"),
            "{init}: {start:?}"
        );
        let checks = function(&script, "start_checks");
        for needed in [
            "\"$CONFIG\"",
            "\"$SERVER_PAYLOAD\"",
            "\"$PLUGIN_PAYLOAD\"",
            "check_tmp_space",
        ] {
            assert!(
                checks.contains(needed),
                "{init}: start_checks lacks {needed}"
            );
        }
        assert!(
            !checks.contains("rm ") && !checks.contains("procd_"),
            "{init}: a check changes nothing"
        );
    }
}

/// The threshold belongs to the device's install record (ADR-014, ruling
/// 9): what the install script writes is what that device's own init
/// demands. A stricter route would refuse every restart on a model with
/// less room; a laxer one would accept a restart the init then refuses.
#[test]
fn each_install_record_names_the_threshold_its_init_demands() {
    for (init, installers) in GZ_TMP {
        let demanded: u64 = assigned(&read(init), "MIN_TMP_KB")
            .unwrap_or_else(|| panic!("{init}: no MIN_TMP_KB"))
            .parse()
            .unwrap_or_else(|_| panic!("{init}: MIN_TMP_KB is not a number"));
        for installer in installers {
            let written = written_thresholds(&read(installer));
            assert!(!written.is_empty(), "{installer} writes no install.json");
            for kb in written {
                assert_eq!(kb, demanded, "{installer} against {init}");
            }
        }
    }
    // Where a record gives none, the agent takes the XE300's.
    assert_eq!(
        assigned(&read("devices/xe300/sctl.init"), "MIN_TMP_KB").as_deref(),
        Some("16384")
    );
    assert_eq!(sctl::upgrade::install::GZ_TMP_RESTART_TMP_KB, 16384);
}

/// A unit installed before its record carried the threshold takes it from
/// its own init (TRD-11 4.5). The agent reads this repository's inits as
/// it would read a unit's: each gives the number it checks, read as text.
#[test]
fn the_agent_reads_each_gz_tmp_inits_own_threshold() {
    use sctl::upgrade::install::{InstallInfo, ThresholdFrom};
    let scratch = std::env::temp_dir().join(format!("sctl-restart-inits-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).unwrap();
    for (init, _) in GZ_TMP {
        let script = repo().join(init);
        let demanded: u64 = assigned(&read(init), "MIN_TMP_KB")
            .unwrap()
            .parse()
            .unwrap();
        let record = scratch.join("install.json");
        std::fs::write(
            &record,
            format!(
                r#"{{"v":1,"layout":"gz-tmp","target":"mips_24kc","restart":"{} restart"}}"#,
                script.display()
            ),
        )
        .unwrap();
        let info = InstallInfo::load(&record, "mips_24kc").unwrap().unwrap();
        assert_eq!(info.restart_tmp_kb, demanded, "{init}");
        assert_eq!(info.restart_tmp_kb_from, ThresholdFrom::Init, "{init}");
    }
    let _ = std::fs::remove_dir_all(&scratch);
}

/// One kill switch: the path every init that has one reads is the path
/// the agent refuses a restart by.
#[test]
fn every_kill_switch_is_the_path_the_agent_checks() {
    let with_a_switch = [
        "devices/xe300/sctl.init",
        "devices/common/sctl-ramboot.init",
    ];
    for init in with_a_switch {
        assert_eq!(
            assigned(&read(init), "DISABLED").as_deref(),
            Some(sctl::upgrade::install::DISABLED_PATH),
            "{init}"
        );
    }
    // The inits with none: no init gains a kill switch by this decision.
    for init in ["devices/rut241/sctl.init", "server/files/sctl.init"] {
        assert_eq!(assigned(&read(init), "DISABLED"), None, "{init}");
    }
    // The wrapper the agent checks is the one the ramboot init runs.
    assert_eq!(
        assigned(&read("devices/common/sctl-ramboot.init"), "WRAPPER").as_deref(),
        Some(sctl::upgrade::install::RAMBOOT_WRAPPER_PATH)
    );
}

/// Each script still parses. `sh -n` reads a script and runs nothing.
#[test]
fn the_scripts_parse() {
    for (script, shell) in [
        ("devices/xe300/sctl.init", "sh"),
        ("devices/rut241/sctl.init", "sh"),
        ("devices/common/sctl-ramboot.init", "sh"),
        ("devices/xe300/upgrade.sh", "sh"),
        ("devices/xe300/install.sh", "bash"),
        ("devices/rut241/install.sh", "bash"),
    ] {
        let output = Command::new(shell)
            .arg("-n")
            .arg(repo().join(script))
            .output()
            .unwrap_or_else(|e| panic!("{shell} -n {script}: {e}"));
        assert!(
            output.status.success(),
            "{script} does not parse: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
