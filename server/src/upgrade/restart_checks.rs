//! Whether a start of the agent would be refused (ADR-014 decision 5):
//! what `POST /api/restart` judges before it accepts, so that the init's
//! own refusals cannot strand a unit whose agent was just stopped.
//!
//! The judgment has no side effect. It reads the install record and the
//! unit's files, and answers the first reason in a fixed order. A path is
//! judged by its metadata before anything is opened: one that is not a
//! regular file is never opened, so a FIFO placed there cannot hold the
//! request.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::install::{conf_value, InstallInfo, Layout, Running};

/// Why `POST /api/restart` refused, in the 422's `detail.reason`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// No `install.json`: the agent does not know how it is started.
    NoInstallJson,
    /// The kill switch exists: an agent stopped now would not start again.
    Disabled,
    /// A `ramboot` unit runs its agent under a loader `install.json` does
    /// not name: the helper cannot be started.
    LoaderUnknown,
    /// A file the init needs to start is missing or unreadable.
    StartUnreadable,
    /// `ramboot`: a payload is not in the cache as `ramboot.conf` names it,
    /// so the start would download it over the uplink.
    CacheInvalid,
    /// A layout that runs from `/tmp`: less room there than a start needs.
    NoSpace,
    /// An Infra recovery run is in progress.
    RecoveryRunning,
    /// The agent is a relay.
    Relay,
    /// `unless` named sessions, and a terminal has a client attached.
    SessionsOpen,
    /// `unless` named jobs, and one is running.
    JobsRunning,
    /// `unless` named transfers, and one is in flight.
    TransfersRunning,
}

/// A refusal: the reason, and one line for a person.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub reason: Reason,
    pub detail: String,
}

impl Refusal {
    pub fn new(reason: Reason, detail: impl Into<String>) -> Self {
        Self {
            reason,
            detail: detail.into(),
        }
    }
}

/// What the judgment reads besides the install record. Every path and the
/// free-space reader are given, so a test stages a unit in a scratch root.
pub struct Site<'a> {
    /// The config the agent was started with, when it was given one.
    pub config: Option<&'a Path>,
    /// The kill switch (`install::disabled_path`).
    pub disabled: &'a Path,
    /// The ramboot init's wrapper (`install::RAMBOOT_WRAPPER_PATH`).
    pub wrapper: &'a Path,
    /// How the agent runs: the binary the helper is started from, and its
    /// loader words (`InstallInfo::running`).
    pub running: Result<Running, String>,
    /// Free space on the filesystem that holds a path, in KB.
    pub free_kb: &'a dyn Fn(&Path) -> Option<u64>,
}

/// The payloads a `ramboot.conf` may name: its key prefix, the file's name
/// in the cache, and whether the boot fetcher needs it whatever the config
/// says (`devices/common/sctl-ramboot.sh`).
const RAMBOOT_PAYLOADS: [(&str, &str, bool); 4] = [
    ("SERVER", "sctl-server.payload", true),
    ("PLUGIN", "sctl-comms.payload", false),
    ("MUSL_LIBC", "musl-libc.payload", false),
    ("LIBGCC", "libgcc.payload", false),
];

/// Whether `path` is a regular file this process can open for reading. A
/// path that is not a regular file is never opened.
fn readable_file(path: &Path) -> Result<(), String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    std::fs::File::open(path)
        .map(drop)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Whether `path` is a regular file with an execute bit.
fn executable_file(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    if meta.permissions().mode() & 0o111 == 0 {
        return Err(format!("{} is not executable", path.display()));
    }
    Ok(())
}

/// The size of a regular file in KB, rounded up; 0 for anything else.
fn file_kb(path: &Path) -> u64 {
    std::fs::metadata(path)
        .ok()
        .filter(std::fs::Metadata::is_file)
        .map_or(0, |m| m.len().div_ceil(1024))
}

/// The boot fetcher's cache directory, as its config and its defaults give
/// it: `CACHE_DIR`, else `RUN_DIR/cache`, else the install record's.
fn ramboot_cache_dir(install: &InstallInfo, conf: &str) -> PathBuf {
    if let Some(dir) = conf_value(conf, "CACHE_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(run) = conf_value(conf, "RUN_DIR") {
        return Path::new(&run).join("cache");
    }
    install
        .cache_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("/tmp/sctl/cache"))
}

/// The boot fetcher's own rule for a start that downloads nothing
/// (`cache_is_valid`): the server's payload, and each other payload whose
/// URL the config names, is in the cache with the SHA-256 the config
/// gives, or, given none, is there under `ALLOW_UNSIGNED=1`.
fn ramboot_cache_valid(install: &InstallInfo, conf: &str) -> Result<(), String> {
    let cache = ramboot_cache_dir(install, conf);
    let unsigned_ok = conf_value(conf, "ALLOW_UNSIGNED").as_deref() == Some("1");
    for (prefix, name, always) in RAMBOOT_PAYLOADS {
        if !always && conf_value(conf, &format!("{prefix}_URL")).is_none() {
            continue;
        }
        let path = cache.join(name);
        readable_file(&path)?;
        match conf_value(conf, &format!("{prefix}_SHA256")) {
            None if unsigned_ok => {}
            None => {
                return Err(format!(
                "{}: the config gives no SHA-256 for it, so the boot fetcher downloads it again",
                path.display()
            ))
            }
            Some(expected) => {
                let actual =
                    super::sha256_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                if actual != expected {
                    return Err(format!(
                        "{}: its SHA-256 is not the config's, so the boot fetcher downloads it again",
                        path.display()
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Judge whether a start of the agent would be refused. `Ok` when the
/// agent may be stopped: its init would start it again.
pub fn judge(install: Option<&InstallInfo>, site: &Site<'_>) -> Result<(), Refusal> {
    let Some(install) = install else {
        return Err(Refusal::new(
            Reason::NoInstallJson,
            "no install.json: the agent does not know how it is started",
        ));
    };
    if std::fs::symlink_metadata(site.disabled).is_ok() {
        return Err(Refusal::new(
            Reason::Disabled,
            format!(
                "{} exists: an agent stopped now would not start again",
                site.disabled.display()
            ),
        ));
    }
    let ramboot = install.layout == Layout::Ramboot;
    if ramboot && install.helper_prefix.is_empty() {
        match install.ramboot_loader() {
            Ok(None) => {}
            Ok(Some(key)) => {
                return Err(Refusal::new(
                    Reason::LoaderUnknown,
                    format!(
                        "{} runs the agent under a loader ({key}) that install.json does not name (helper_prefix): the helper cannot be started",
                        install.ramboot_conf_path().display()
                    ),
                ))
            }
            Err(e) => {
                return Err(Refusal::new(
                    Reason::LoaderUnknown,
                    format!("{e}: whether the agent runs under a loader is not known"),
                ))
            }
        }
    }
    let unreadable = |e: String| Refusal::new(Reason::StartUnreadable, e);
    if let Some(config) = site.config {
        readable_file(config).map_err(unreadable)?;
    }
    match install.layout {
        Layout::GzTmp => {
            for path in install.files.values() {
                readable_file(path).map_err(unreadable)?;
            }
        }
        Layout::Ramboot => {
            executable_file(site.wrapper).map_err(unreadable)?;
            readable_file(&install.ramboot_conf_path()).map_err(unreadable)?;
        }
        Layout::UsrBin | Layout::Systemd => {}
    }
    if ramboot {
        let conf_path = install.ramboot_conf_path();
        let conf = std::fs::read_to_string(&conf_path)
            .map_err(|e| unreadable(format!("{}: {e}", conf_path.display())))?;
        ramboot_cache_valid(install, &conf).map_err(|e| Refusal::new(Reason::CacheInvalid, e))?;
    }
    if matches!(install.layout, Layout::GzTmp | Layout::Ramboot) {
        let running = site.running.as_ref().map_err(|e| unreadable(e.clone()))?;
        let dir = running.binary.parent().unwrap_or(Path::new("/"));
        // The helper runs from the old binary, through its loader where
        // there is one, so it pins their room while the init writes the new
        // copy, on the first run and on a second. The init's own threshold
        // is measured while the old copy is still there and counts none of
        // that, so the room demanded is the device's threshold (never less
        // than the upgrade's margin) plus what the helper pins.
        let held_kb = file_kb(&running.binary)
            + running
                .loader
                .first()
                .map_or(0, |loader| file_kb(Path::new(loader)));
        let need_kb = install
            .restart_tmp_kb
            .max(install.min_free_kb)
            .saturating_add(held_kb);
        match (site.free_kb)(dir) {
            Some(free) if free >= need_kb => {}
            Some(free) => {
                return Err(Refusal::new(
                    Reason::NoSpace,
                    format!(
                        "{} has {free} KB free, a start needs {need_kb} KB",
                        dir.display()
                    ),
                ))
            }
            None => {
                return Err(Refusal::new(
                    Reason::NoSpace,
                    format!("the free space of {} could not be read", dir.display()),
                ))
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;
    use std::os::unix::fs::PermissionsExt;

    /// A unit staged in a scratch root: its files, and what the judgment is
    /// handed.
    struct Unit {
        root: PathBuf,
        install: InstallInfo,
        config: PathBuf,
        disabled: PathBuf,
        wrapper: PathBuf,
        binary: PathBuf,
        free_kb: Option<u64>,
    }

    impl Unit {
        fn new(tag: &str, layout: Layout) -> Self {
            let root = std::env::temp_dir()
                .join(format!("sctl-restart-checks-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("run")).unwrap();
            std::fs::create_dir_all(root.join("etc")).unwrap();
            let config = root.join("etc/sctl.toml");
            std::fs::write(&config, "").unwrap();
            let binary = root.join("run/sctl-server");
            std::fs::write(&binary, vec![0_u8; 3 * 1024 * 1024]).unwrap();
            let mut install = InstallInfo::defaults(layout, "mips_24kc");
            match layout {
                Layout::GzTmp => {
                    for (role, name) in [("server", "server.gz"), ("plugin", "plugin.so.gz")] {
                        let path = root.join(name);
                        std::fs::write(&path, b"payload").unwrap();
                        install.files.insert(role.to_string(), path);
                    }
                }
                Layout::Ramboot => {
                    install.ramboot_conf = Some(root.join("etc/ramboot.conf"));
                    install.cache_dir = Some(root.join("run/cache"));
                }
                Layout::UsrBin | Layout::Systemd => {}
            }
            let unit = Self {
                wrapper: root.join("etc/ramboot.sh"),
                disabled: root.join("etc/disabled"),
                root,
                install,
                config,
                binary,
                free_kb: Some(60_000),
            };
            if layout == Layout::Ramboot {
                std::fs::write(&unit.wrapper, "#!/bin/sh\n").unwrap();
                std::fs::set_permissions(&unit.wrapper, std::fs::Permissions::from_mode(0o755))
                    .unwrap();
                unit.cache("sctl-server.payload", b"server");
                unit.conf(&[
                    ("SERVER_URL", "http://mirror.invalid/server"),
                    ("SERVER_SHA256", &unit.sha("sctl-server.payload")),
                ]);
            }
            unit
        }

        fn cache(&self, name: &str, bytes: &[u8]) {
            let dir = self.root.join("run/cache");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(name), bytes).unwrap();
        }

        fn sha(&self, name: &str) -> String {
            crate::upgrade::sha256_file(&self.root.join("run/cache").join(name)).unwrap()
        }

        /// Write `ramboot.conf` with these keys, each as the install script
        /// quotes it.
        fn conf(&self, keys: &[(&str, &str)]) {
            let mut text = String::new();
            for (k, v) in keys {
                let _ = writeln!(text, "{k}='{v}'");
            }
            std::fs::write(self.root.join("etc/ramboot.conf"), text).unwrap();
        }

        fn judge(&self) -> Result<(), Refusal> {
            let free = self.free_kb;
            judge(
                Some(&self.install),
                &Site {
                    config: Some(&self.config),
                    disabled: &self.disabled,
                    wrapper: &self.wrapper,
                    running: Ok(Running {
                        binary: self.binary.clone(),
                        loader: Vec::new(),
                    }),
                    free_kb: &move |_| free,
                },
            )
        }

        fn reason(&self) -> Option<Reason> {
            self.judge().err().map(|r| r.reason)
        }
    }

    impl Drop for Unit {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn no_install_record_is_refused_before_anything_is_read() {
        let unit = Unit::new("none", Layout::UsrBin);
        let refusal = judge(
            None,
            &Site {
                config: None,
                disabled: &unit.disabled,
                wrapper: &unit.wrapper,
                running: Err("not asked".into()),
                free_kb: &|_| panic!("free space is not read with no install record"),
            },
        )
        .unwrap_err();
        assert_eq!(refusal.reason, Reason::NoInstallJson);
        assert_eq!(
            serde_json::to_value(refusal.reason).unwrap(),
            "no_install_json"
        );
    }

    #[test]
    fn every_layout_passes_as_staged_and_is_refused_by_the_kill_switch() {
        for layout in [
            Layout::UsrBin,
            Layout::GzTmp,
            Layout::Ramboot,
            Layout::Systemd,
        ] {
            let unit = Unit::new(&format!("switch-{}", layout.as_str()), layout);
            assert_eq!(unit.judge(), Ok(()), "{}", layout.as_str());
            std::fs::write(&unit.disabled, "").unwrap();
            let refusal = unit.judge().unwrap_err();
            assert_eq!(refusal.reason, Reason::Disabled, "{}", layout.as_str());
            assert!(refusal.detail.contains("would not start again"));
        }
    }

    #[test]
    fn a_file_the_init_needs_is_start_unreadable() {
        // The agent's config, on any layout.
        let unit = Unit::new("config", Layout::UsrBin);
        std::fs::remove_file(&unit.config).unwrap();
        assert_eq!(unit.reason(), Some(Reason::StartUnreadable));

        // A gz-tmp payload: missing, then a directory in its place.
        let unit = Unit::new("payload", Layout::GzTmp);
        let plugin = unit.install.files["plugin"].clone();
        std::fs::remove_file(&plugin).unwrap();
        assert_eq!(unit.reason(), Some(Reason::StartUnreadable));
        std::fs::create_dir(&plugin).unwrap();
        let refusal = unit.judge().unwrap_err();
        assert_eq!(refusal.reason, Reason::StartUnreadable);
        assert!(
            refusal.detail.contains("not a regular file"),
            "{}",
            refusal.detail
        );

        // The ramboot wrapper: there but not executable, then gone.
        let unit = Unit::new("wrapper", Layout::Ramboot);
        std::fs::set_permissions(&unit.wrapper, std::fs::Permissions::from_mode(0o644)).unwrap();
        let refusal = unit.judge().unwrap_err();
        assert_eq!(refusal.reason, Reason::StartUnreadable);
        assert!(refusal.detail.contains("not executable"));
        std::fs::remove_file(&unit.wrapper).unwrap();
        assert_eq!(unit.reason(), Some(Reason::StartUnreadable));
    }

    /// A FIFO with no writer at a payload's path would hold a plain open
    /// for good. It is judged by its metadata and never opened.
    #[test]
    fn a_fifo_where_a_payload_should_be_is_refused_at_once() {
        let unit = Unit::new("fifo", Layout::GzTmp);
        let server = unit.install.files["server"].clone();
        std::fs::remove_file(&server).unwrap();
        nix::unistd::mkfifo(&server, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();
        let at = std::time::Instant::now();
        let refusal = unit.judge().unwrap_err();
        assert!(at.elapsed() < std::time::Duration::from_secs(2));
        assert_eq!(refusal.reason, Reason::StartUnreadable);
        assert!(refusal.detail.contains("not a regular file"));

        let unit = Unit::new("fifo-cache", Layout::Ramboot);
        let cached = unit.root.join("run/cache/sctl-server.payload");
        std::fs::remove_file(&cached).unwrap();
        nix::unistd::mkfifo(&cached, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();
        let at = std::time::Instant::now();
        assert_eq!(unit.reason(), Some(Reason::CacheInvalid));
        assert!(at.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn a_ramboot_start_that_would_fetch_is_cache_invalid() {
        let unit = Unit::new("cache", Layout::Ramboot);
        assert_eq!(unit.judge(), Ok(()));

        // One byte of the cached server changed.
        unit.cache("sctl-server.payload", b"servex");
        let refusal = unit.judge().unwrap_err();
        assert_eq!(refusal.reason, Reason::CacheInvalid);
        assert!(refusal.detail.contains("downloads it again"));
        unit.cache("sctl-server.payload", b"server");
        assert_eq!(unit.judge(), Ok(()));

        // A payload the config names by URL must be there too; one it does
        // not name is not asked for.
        let server_sha = unit.sha("sctl-server.payload");
        unit.conf(&[
            ("SERVER_URL", "http://mirror.invalid/server"),
            ("SERVER_SHA256", &server_sha),
            ("PLUGIN_URL", "http://mirror.invalid/plugin"),
            ("PLUGIN_SHA256", "00"),
        ]);
        assert_eq!(unit.reason(), Some(Reason::CacheInvalid), "not cached");
        unit.cache("sctl-comms.payload", b"plugin");
        assert_eq!(unit.reason(), Some(Reason::CacheInvalid), "another hash");
        let plugin_sha = unit.sha("sctl-comms.payload");
        unit.conf(&[
            ("SERVER_URL", "http://mirror.invalid/server"),
            ("SERVER_SHA256", &server_sha),
            ("PLUGIN_URL", "http://mirror.invalid/plugin"),
            ("PLUGIN_SHA256", &plugin_sha),
        ]);
        assert_eq!(unit.judge(), Ok(()));

        // The server's payload is needed whatever the config says.
        std::fs::remove_file(unit.root.join("run/cache/sctl-server.payload")).unwrap();
        assert_eq!(unit.reason(), Some(Reason::CacheInvalid));
    }

    /// The boot fetcher takes a cached payload with no SHA-256 only under
    /// `ALLOW_UNSIGNED=1`; otherwise it downloads it again.
    #[test]
    fn an_unsigned_payload_passes_only_where_the_fetcher_allows_it() {
        let unit = Unit::new("unsigned", Layout::Ramboot);
        unit.conf(&[("SERVER_URL", "http://mirror.invalid/server")]);
        assert_eq!(unit.reason(), Some(Reason::CacheInvalid));
        unit.conf(&[
            ("SERVER_URL", "http://mirror.invalid/server"),
            ("ALLOW_UNSIGNED", "1"),
        ]);
        assert_eq!(unit.judge(), Ok(()));
    }

    #[test]
    fn the_cache_is_where_the_config_puts_it() {
        let unit = Unit::new("cachedir", Layout::Ramboot);
        let server_sha = unit.sha("sctl-server.payload");
        let elsewhere = unit.root.join("elsewhere");
        std::fs::create_dir_all(elsewhere.join("cache")).unwrap();
        // RUN_DIR moves the cache with it; nothing is there.
        unit.conf(&[
            ("RUN_DIR", elsewhere.to_str().unwrap()),
            ("SERVER_URL", "http://mirror.invalid/server"),
            ("SERVER_SHA256", &server_sha),
        ]);
        assert_eq!(unit.reason(), Some(Reason::CacheInvalid));
        std::fs::write(elsewhere.join("cache/sctl-server.payload"), b"server").unwrap();
        assert_eq!(unit.judge(), Ok(()));
        // CACHE_DIR wins over RUN_DIR.
        unit.conf(&[
            ("RUN_DIR", elsewhere.to_str().unwrap()),
            ("CACHE_DIR", unit.root.join("run/cache").to_str().unwrap()),
            ("SERVER_URL", "http://mirror.invalid/server"),
            ("SERVER_SHA256", &server_sha),
        ]);
        std::fs::remove_file(elsewhere.join("cache/sctl-server.payload")).unwrap();
        assert_eq!(unit.judge(), Ok(()));
    }

    #[test]
    fn a_ramboot_unit_under_a_loader_the_record_does_not_name_is_loader_unknown() {
        let mut unit = Unit::new("loader", Layout::Ramboot);
        let server_sha = unit.sha("sctl-server.payload");
        unit.cache("musl-libc.payload", b"libc");
        let libc_sha = unit.sha("musl-libc.payload");
        unit.conf(&[
            ("SERVER_URL", "http://mirror.invalid/server"),
            ("SERVER_SHA256", &server_sha),
            ("MUSL_LIBC_URL", "http://mirror.invalid/libc"),
            ("MUSL_LIBC_SHA256", &libc_sha),
        ]);
        let refusal = unit.judge().unwrap_err();
        assert_eq!(refusal.reason, Reason::LoaderUnknown);
        assert!(refusal.detail.contains("MUSL_LIBC_URL"));
        // With the loader's words in the record, the unit passes.
        unit.install.helper_prefix = vec!["/lib/libc.so".into(), "--library-path".into()];
        assert_eq!(unit.judge(), Ok(()));
        // A config that cannot be read says nothing of a loader.
        unit.install.helper_prefix.clear();
        std::fs::remove_file(unit.root.join("etc/ramboot.conf")).unwrap();
        assert_eq!(unit.reason(), Some(Reason::LoaderUnknown));
    }

    #[test]
    fn free_space_is_judged_where_the_agent_runs_from_tmp_and_nowhere_else() {
        // gz-tmp: the record's threshold, the init's own, plus the room the
        // helper pins: it runs from the old binary (3 MiB here), which
        // stays in /tmp while the init extracts the new copy beside it.
        let mut unit = Unit::new("space", Layout::GzTmp);
        assert_eq!(unit.install.restart_tmp_kb, 16384);
        // The init's threshold alone is not room enough.
        unit.free_kb = Some(16384);
        let refusal = unit.judge().unwrap_err();
        assert_eq!(refusal.reason, Reason::NoSpace);
        assert!(
            refusal.detail.contains("16384 KB free") && refusal.detail.contains("19456 KB"),
            "{}",
            refusal.detail
        );
        unit.free_kb = Some(16384 + 3072 - 1);
        assert_eq!(unit.reason(), Some(Reason::NoSpace));
        unit.free_kb = Some(16384 + 3072);
        assert_eq!(unit.judge(), Ok(()));
        // A device whose init asks for less: its record says so.
        unit.install.restart_tmp_kb = 8192;
        unit.free_kb = Some(8192 + 3072 - 1);
        assert_eq!(unit.reason(), Some(Reason::NoSpace));
        unit.free_kb = Some(8192 + 3072);
        assert_eq!(unit.judge(), Ok(()));
        // A loader the helper runs through is pinned too.
        let loader = unit.root.join("run/libc.so");
        std::fs::write(&loader, vec![0_u8; 1024 * 1024]).unwrap();
        let through_a_loader = |free| {
            judge(
                Some(&unit.install),
                &Site {
                    config: Some(&unit.config),
                    disabled: &unit.disabled,
                    wrapper: &unit.wrapper,
                    running: Ok(Running {
                        binary: unit.binary.clone(),
                        loader: vec![loader.display().to_string(), "--library-path".into()],
                    }),
                    free_kb: &move |_| Some(free),
                },
            )
            .err()
            .map(|r| r.reason)
        };
        assert_eq!(through_a_loader(8192 + 3072 + 1023), Some(Reason::NoSpace));
        assert_eq!(through_a_loader(8192 + 3072 + 1024), None);
        // Never under the upgrade's margin plus what the helper pins:
        // gz-tmp's 4096 KB.
        unit.install.restart_tmp_kb = 0;
        unit.free_kb = Some(4096 + 3072 - 1);
        assert_eq!(unit.reason(), Some(Reason::NoSpace));
        unit.free_kb = Some(4096 + 3072);
        assert_eq!(unit.judge(), Ok(()));
        // Space that cannot be read is not room.
        unit.free_kb = None;
        assert_eq!(unit.reason(), Some(Reason::NoSpace));

        // ramboot: no threshold of its own, the helper's room only.
        let mut unit = Unit::new("space-ramboot", Layout::Ramboot);
        assert_eq!(unit.install.restart_tmp_kb, 0);
        unit.free_kb = Some(3072 + 256 - 1);
        assert_eq!(unit.reason(), Some(Reason::NoSpace));
        unit.free_kb = Some(3072 + 256);
        assert_eq!(unit.judge(), Ok(()));

        // usr-bin and systemd run from flash: free space is not asked.
        for layout in [Layout::UsrBin, Layout::Systemd] {
            let mut unit = Unit::new(&format!("space-{}", layout.as_str()), layout);
            unit.free_kb = Some(0);
            assert_eq!(unit.judge(), Ok(()));
        }
    }

    /// The order is fixed: the first reason a caller reads is the first a
    /// person has to put right.
    #[test]
    fn the_reasons_come_in_order() {
        let mut unit = Unit::new("order", Layout::Ramboot);
        unit.free_kb = Some(0);
        unit.cache("sctl-server.payload", b"servex");
        std::fs::remove_file(&unit.config).unwrap();
        std::fs::write(&unit.disabled, "").unwrap();
        assert_eq!(unit.reason(), Some(Reason::Disabled));
        std::fs::remove_file(&unit.disabled).unwrap();
        assert_eq!(unit.reason(), Some(Reason::StartUnreadable));
        std::fs::write(&unit.config, "").unwrap();
        assert_eq!(unit.reason(), Some(Reason::CacheInvalid));
        unit.cache("sctl-server.payload", b"server");
        assert_eq!(unit.reason(), Some(Reason::NoSpace));
        unit.free_kb = Some(60_000);
        assert_eq!(unit.judge(), Ok(()));
    }
}
