//! `devices/xe300/relay-route.sh rp-filter`, run against a stand-in `/proc`
//! and `sysctl.conf`.
//!
//! The helper runs on the GL-XE300 under busybox. It is run here with the
//! system `sh` and, when busybox is installed, with busybox's own `sh`, `sed`,
//! `tail` and `cat`, the applets the device has. A stand-in `sysctl`, named
//! by absolute path so that no shell can pick the real one (busybox's `sh`
//! runs its own applets whatever PATH says), records its calls and writes
//! the value where the real one would.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const SCRIPT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../devices/xe300/relay-route.sh"
);

/// `sysctl -w key=value`: the call goes to $SYSCTL_CALLS, the value under
/// $FAKE_PROC_SYS.
const FAKE_SYSCTL: &str = r#"#!/bin/sh
[ "$1" = -w ] || exit 2
echo "$2" >> "$SYSCTL_CALLS"
key=${2%%=*}
value=${2#*=}
printf '%s\n' "$value" > "$FAKE_PROC_SYS/$(printf '%s' "$key" | tr . /)"
"#;

/// Interfaces with their rp_filter as the device might have them: strict on
/// conf/all, default, the wire and a VLAN (whose name has a dot).
const INTERFACES: [(&str, &str); 6] = [
    ("all", "1"),
    ("default", "1"),
    ("lo", "0"),
    ("eth0", "1"),
    ("eth0.2", "1"),
    ("wwan0", "0"),
];

/// A sysctl.conf with strict lines to replace, lines to keep, and a last
/// line without its newline.
const SYSCTL_CONF: &str = "# kept\n\
net.ipv4.conf.all.rp_filter=1\n\
net.ipv4.conf.eth1.ignore_routes_with_linkdown=1\n\
  net.ipv4.conf.default.rp_filter = 1\n\
net.ipv4.conf.eth0.rp_filter=1\n\
kernel.panic=3";

const EXPECTED_CONF: &str = "# kept\n\
net.ipv4.conf.eth1.ignore_routes_with_linkdown=1\n\
net.ipv4.conf.eth0.rp_filter=1\n\
kernel.panic=3\n\
net.ipv4.conf.all.rp_filter=2\n\
net.ipv4.conf.default.rp_filter=2\n";

/// A scratch directory, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("sctl-relay-route-sh-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        for (name, value) in INTERFACES {
            let conf = dir.join("proc/net/ipv4/conf").join(name);
            std::fs::create_dir_all(&conf).unwrap();
            std::fs::write(conf.join("rp_filter"), format!("{value}\n")).unwrap();
        }
        let sysctl = dir.join("bin/sysctl");
        std::fs::write(&sysctl, FAKE_SYSCTL).unwrap();
        std::fs::set_permissions(&sysctl, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self(dir)
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }

    fn rp_filter(&self, name: &str) -> String {
        std::fs::read_to_string(self.path(&format!("proc/net/ipv4/conf/{name}/rp_filter")))
            .unwrap()
            .trim()
            .to_string()
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.path("calls"))
            .unwrap_or_default()
            .lines()
            .map(ToString::to_string)
            .collect()
    }

    /// Run `relay-route.sh rp-filter` with `shell` and PATH `path`.
    fn run(&self, shell: &[&str], path: &str) -> String {
        let out = Command::new(shell[0])
            .args(&shell[1..])
            .arg(SCRIPT)
            .arg("rp-filter")
            .env("PATH", path)
            .env("RELAY_ROUTE_SYSCTL", self.path("bin/sysctl"))
            .env("RELAY_ROUTE_SYSCTL_CONF", self.path("sysctl.conf"))
            .env("RELAY_ROUTE_IPV4_CONF", self.path("proc/net/ipv4/conf"))
            .env("FAKE_PROC_SYS", self.path("proc"))
            .env("SYSCTL_CALLS", self.path("calls"))
            .output()
            .expect("the shell runs");
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            out.status.success(),
            "{shell:?}: {}{}",
            stdout,
            String::from_utf8_lossy(&out.stderr)
        );
        stdout
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// busybox, when installed.
fn busybox() -> Option<PathBuf> {
    let found = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join("busybox"))
            .find(|candidate| candidate.is_file())
    });
    if found.is_none() {
        eprintln!("busybox is not installed: only the system sh runs the helper");
    }
    found
}

/// Run the helper twice with a `sysctl.conf` to edit, then once without
/// one. With `busybox`, the shell is its `sh` and its applets come first on
/// PATH (a busybox built to prefer its applets uses them anyway).
fn check(busybox: Option<&Path>, name: &str) {
    let scratch = Scratch::new(name);
    let fresh = Scratch::new(&format!("{name}-fresh"));
    let system = std::env::var("PATH").unwrap_or_default();
    let (shell, path) = match busybox {
        Some(busybox) => {
            for applet in ["sed", "tail", "cat", "tr"] {
                std::os::unix::fs::symlink(busybox, scratch.path("bin").join(applet)).unwrap();
            }
            let path = format!("{}:{system}", scratch.path("bin").display());
            (vec![busybox.to_str().unwrap(), "sh"], path)
        }
        None => (vec!["sh"], system),
    };
    let shell = shell.as_slice();
    std::fs::write(scratch.path("sysctl.conf"), SYSCTL_CONF).unwrap();

    let said = scratch.run(shell, &path);
    assert!(said.contains("rp_filter is 2 (loose)"), "{said}");
    let conf = std::fs::read_to_string(scratch.path("sysctl.conf")).unwrap();
    assert_eq!(conf, EXPECTED_CONF, "{shell:?}");
    assert_eq!(
        scratch.calls(),
        [
            "net.ipv4.conf.all.rp_filter=2",
            "net.ipv4.conf.default.rp_filter=2"
        ],
        "{shell:?}"
    );
    for (name, _) in INTERFACES {
        assert_eq!(scratch.rp_filter(name), "2", "{shell:?} {name}");
    }

    // Again: the file stays as it is, and so does every interface.
    scratch.run(shell, &path);
    let again = std::fs::read_to_string(scratch.path("sysctl.conf")).unwrap();
    assert_eq!(again, EXPECTED_CONF, "{shell:?}: idempotent");
    for (name, _) in INTERFACES {
        assert_eq!(scratch.rp_filter(name), "2", "{shell:?} {name}");
    }

    // A device without the file gets one with the two lines.
    fresh.run(shell, &path);
    assert_eq!(
        std::fs::read_to_string(fresh.path("sysctl.conf")).unwrap(),
        "net.ipv4.conf.all.rp_filter=2\nnet.ipv4.conf.default.rp_filter=2\n"
    );
}

#[test]
fn rp_filter_goes_loose_now_and_at_boot_with_the_system_sh() {
    check(None, "sh");
}

#[test]
fn rp_filter_goes_loose_now_and_at_boot_with_busybox() {
    if let Some(busybox) = busybox() {
        check(Some(&busybox), "busybox");
    }
}

#[test]
fn the_helper_is_where_the_test_expects_it() {
    assert!(Path::new(SCRIPT).is_file(), "{SCRIPT}");
}
