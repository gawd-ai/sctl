//! `/etc/sctl/install.json`: how the agent is installed on this box. The
//! install script writes it; the agent reads it at start and reports the
//! layout; the upgrade uses it to know which staged file replaces which
//! installed one, how to restart, and where the rollback set lives.
//!
//! Every key but `layout` (and `target`) has a default per layout, so a
//! script writes only what differs:
//!
//! ```json
//! {"v": 1, "layout": "gz-tmp", "target": "mips_24kc"}
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Where the install script writes it.
pub const DEFAULT_PATH: &str = "/etc/sctl/install.json";
/// The file `[upgrade] hold` is the config form of.
pub const HOLD_PATH: &str = "/etc/sctl/upgrade-hold";
/// The boot fetcher's config when `install.json` names none.
pub const RAMBOOT_CONF_PATH: &str = "/etc/sctl/ramboot.conf";
/// The kill switch: while this file exists, an init that has a kill switch
/// starts no agent (`devices/xe300/sctl.init`,
/// `devices/common/sctl-ramboot.init`). The agent refuses to be restarted
/// while it exists, on every layout (ADR-014).
pub const DISABLED_PATH: &str = "/etc/sctl/disabled";
/// Moves the kill switch for tests and benches, as `SCTL_INSTALL_JSON`
/// moves `install.json`.
pub const DISABLED_PATH_ENV: &str = "SCTL_DISABLED_PATH";
/// The wrapper the ramboot init runs (`devices/common/sctl-ramboot.init`).
pub const RAMBOOT_WRAPPER_PATH: &str = "/etc/sctl/ramboot.sh";
/// The free `/tmp` a `gz-tmp` init asks for before it starts, where the
/// install record gives none: the XE300's own threshold.
pub const GZ_TMP_RESTART_TMP_KB: u64 = 16384;

/// The most of an init script that is read for its `/tmp` threshold.
pub const INIT_READ_CAP: usize = 64 * 1024;
/// The largest `/tmp` threshold an init is believed to ask for, in KB
/// (1 GiB). A larger number is not a threshold.
pub const INIT_TMP_KB_MAX: u64 = 1024 * 1024;

/// Where `restart_tmp_kb` came from, so a bench can see it in `GET
/// /api/upgrade`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ThresholdFrom {
    /// The install record's own `restart_tmp_kb`.
    Record,
    /// The line `MIN_TMP_KB=<digits>` of the init script the record names.
    Init,
    /// The layout's default: nothing better was there.
    Default,
}

/// The `/tmp` threshold an init script asks for: the number of its first
/// line of the form `MIN_TMP_KB=<digits>`, read as text. `None` when the
/// script has no such line, when its first one holds anything but digits
/// (an expression, a variable, a trailing command), or when the number is
/// 0 or past [`INIT_TMP_KB_MAX`]. Nothing of the script is run.
pub fn init_tmp_kb(script: &str) -> Option<u64> {
    let value = script
        .lines()
        .find_map(|line| line.strip_prefix("MIN_TMP_KB="))?
        .trim_end();
    if value.is_empty() || value.len() > 9 || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value
        .parse()
        .ok()
        .filter(|kb| (1..=INIT_TMP_KB_MAX).contains(kb))
}

/// Where the kill switch is: [`DISABLED_PATH_ENV`] for a bench, else
/// [`DISABLED_PATH`].
pub fn disabled_path() -> PathBuf {
    std::env::var_os(DISABLED_PATH_ENV).map_or_else(|| PathBuf::from(DISABLED_PATH), PathBuf::from)
}

/// How the agent is installed. Every layout runs the agent's tools as
/// `/usr/bin/sctl`, or `/usr/local/bin/sctl` on the relay
/// ([`super::cli_name`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Layout {
    /// The raw binary at `/usr/bin/sctl`, procd (the BPIs).
    UsrBin,
    /// Gzipped payloads under `/usr/local/lib/sctl/`, expanded to `/tmp/sctl/`
    /// by the init at every start (the XE300, and a RUT241 as installed).
    /// The agent makes `/usr/bin/sctl` its guarded wrapper for the binary it
    /// runs from `/tmp`.
    GzTmp,
    /// Nothing but `/etc/sctl/ramboot.conf`; the boot fetcher downloads the
    /// payloads into `/tmp/sctl/cache/` at boot (the WE826, and a RUT241
    /// after `ramboot-migrate.sh`). The agent makes `/usr/bin/sctl` its
    /// guarded wrapper for the binary it runs from `/tmp`, through the
    /// loader on a unit that runs it under one (the WE826, `helper_prefix`).
    Ramboot,
    /// The raw binary at `/usr/local/bin/sctl` under a systemd unit (the relay).
    Systemd,
}

impl Layout {
    /// The wire form.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UsrBin => "usr-bin",
            Self::GzTmp => "gz-tmp",
            Self::Ramboot => "ramboot",
            Self::Systemd => "systemd",
        }
    }

    /// Whether the binary can be executed with `--version` before the swap
    /// (a ramboot box needs its loader; a gz file is expanded first).
    pub fn can_prove_binary(self) -> bool {
        !matches!(self, Self::Ramboot)
    }
}

/// The file as written: everything optional but `layout`.
#[derive(Clone, Debug, Default, Deserialize)]
struct Raw {
    #[serde(default)]
    v: Option<u32>,
    layout: String,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    files: BTreeMap<String, String>,
    #[serde(default)]
    restart: Option<String>,
    #[serde(default)]
    rollback_dir: Option<String>,
    #[serde(default)]
    health_url: Option<String>,
    #[serde(default)]
    min_free_kb: Option<u64>,
    #[serde(default)]
    restart_tmp_kb: Option<u64>,
    #[serde(default)]
    unit: Option<String>,
    #[serde(default)]
    ramboot_conf: Option<String>,
    #[serde(default)]
    cache_dir: Option<String>,
    #[serde(default)]
    helper_prefix: Option<Vec<String>>,
}

/// How the agent runs: its binary, and the loader words before it when the
/// kernel does not run the binary itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Running {
    /// The agent's binary.
    pub binary: PathBuf,
    /// The loader and its options (`helper_prefix`); empty on every unit but
    /// one that runs its agent under a loader.
    pub loader: Vec<String>,
}

/// The resolved install: every field filled.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InstallInfo {
    pub layout: Layout,
    /// The target the script installed; the running binary's own target
    /// wins when they differ (a script cannot know better than the binary).
    pub target: String,
    /// Installed file by role: `server`, `plugin`, and on ramboot `libc`,
    /// `libgcc`. On ramboot these are the boot cache paths.
    pub files: BTreeMap<String, PathBuf>,
    /// The shell command that restarts the service.
    pub restart: String,
    /// Where the rollback set is kept (persistent storage).
    pub rollback_dir: PathBuf,
    /// `GET` here must show the new version; `None` means derive it from
    /// `server.listen`.
    pub health_url: Option<String>,
    /// Free space to keep on the target filesystem after a stage.
    pub min_free_kb: u64,
    /// The free `/tmp` this device's init asks for before it starts the
    /// agent, in KB: what a restart on request needs there (ADR-014). The
    /// device's install script writes it beside the restart command; where
    /// it gives none, [`GZ_TMP_RESTART_TMP_KB`] on `gz-tmp` and 0 elsewhere.
    pub restart_tmp_kb: u64,
    /// Where `restart_tmp_kb` came from: the record, the unit's own init,
    /// or the layout's default.
    pub restart_tmp_kb_from: ThresholdFrom,
    /// The systemd unit (systemd layout).
    pub unit: Option<String>,
    /// The boot fetcher's config (ramboot layout).
    pub ramboot_conf: Option<PathBuf>,
    /// The boot fetcher's cache (ramboot layout).
    pub cache_dir: Option<PathBuf>,
    /// How the unit runs its agent when a loader is needed (the WE826's musl
    /// loader): the helper is started through the same words, e.g.
    /// `["/tmp/sctl/lib/libc.so", "--library-path", "/tmp/sctl/lib"]`.
    /// Empty on every other unit.
    pub helper_prefix: Vec<String>,
}

impl InstallInfo {
    /// Read `path`; `Ok(None)` when it does not exist.
    pub fn load(path: &Path, binary_target: &str) -> Result<Option<Self>, String> {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let mut info =
            Self::parse(&bytes, binary_target).map_err(|e| format!("{}: {e}", path.display()))?;
        info.read_init_threshold();
        Ok(Some(info))
    }

    /// A `gz-tmp` unit installed before its record carried
    /// `restart_tmp_kb` (a managed upgrade writes no install record): take
    /// the threshold its own init demands, where the init says it on one
    /// line (TRD-11 4.5). The init is the first word of the record's
    /// restart command, read as text through the file gate's handle: only a
    /// regular file is opened, without waiting, and at most
    /// [`INIT_READ_CAP`] of it is read. Anything else leaves the default.
    /// Nothing in the init is run or sourced.
    pub fn read_init_threshold(&mut self) {
        if self.layout != Layout::GzTmp || self.restart_tmp_kb_from == ThresholdFrom::Record {
            return;
        }
        let Some(init) = self
            .restart
            .split_whitespace()
            .next()
            .map(Path::new)
            .filter(|p| p.is_absolute())
        else {
            return;
        };
        let read = crate::file_access::read_own(init, INIT_READ_CAP)
            .ok()
            .flatten()
            .and_then(|bytes| init_tmp_kb(&String::from_utf8_lossy(&bytes)));
        if let Some(kb) = read {
            self.restart_tmp_kb = kb;
            self.restart_tmp_kb_from = ThresholdFrom::Init;
        }
    }

    /// Parse the file's bytes and fill the defaults.
    pub fn parse(bytes: &[u8], binary_target: &str) -> Result<Self, String> {
        let raw: Raw = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        if let Some(v) = raw.v {
            if v != 1 {
                return Err(format!("install.json v{v} is not v1"));
            }
        }
        let layout: Layout =
            serde_json::from_value(Value::String(raw.layout.clone())).map_err(|_| {
                format!(
                    "install.json layout '{}' is not one of usr-bin, gz-tmp, ramboot, systemd",
                    raw.layout
                )
            })?;
        let target = match raw.target {
            Some(t) if t != binary_target => {
                tracing::warn!(
                    "install.json says target {t}, the binary is {binary_target}; using the binary's"
                );
                binary_target.to_string()
            }
            _ => binary_target.to_string(),
        };
        let mut info = Self::defaults(layout, &target);
        for (role, path) in raw.files {
            info.files.insert(role, PathBuf::from(path));
        }
        if let Some(r) = raw.restart {
            info.restart = r;
        }
        if let Some(d) = raw.rollback_dir {
            info.rollback_dir = PathBuf::from(d);
        }
        if raw.health_url.is_some() {
            info.health_url = raw.health_url;
        }
        if let Some(kb) = raw.min_free_kb {
            info.min_free_kb = kb;
        }
        if let Some(kb) = raw.restart_tmp_kb {
            info.restart_tmp_kb = kb;
            info.restart_tmp_kb_from = ThresholdFrom::Record;
        }
        if let Some(unit) = raw.unit {
            info.restart = format!("systemctl restart {unit}");
            info.unit = Some(unit);
        }
        if let Some(c) = raw.ramboot_conf {
            info.ramboot_conf = Some(PathBuf::from(c));
        }
        if let Some(c) = raw.cache_dir {
            info.cache_dir = Some(PathBuf::from(c));
        }
        if let Some(prefix) = raw.helper_prefix {
            if prefix.iter().any(String::is_empty) {
                return Err("install.json helper_prefix has an empty word".to_string());
            }
            info.helper_prefix = prefix;
        }
        if !info.files.contains_key("server") && layout != Layout::Ramboot {
            return Err("install.json names no server file".to_string());
        }
        Ok(info)
    }

    /// The defaults of a layout, with the target's file names.
    pub fn defaults(layout: Layout, target: &str) -> Self {
        let mut files = BTreeMap::new();
        let (restart, rollback_dir, min_free_kb, unit, ramboot_conf, cache_dir) = match layout {
            Layout::UsrBin => {
                files.insert("server".to_string(), PathBuf::from("/usr/bin/sctl"));
                files.insert(
                    "plugin".to_string(),
                    PathBuf::from("/usr/lib/sctl/comms/libsctl_comms_quectel.so"),
                );
                (
                    "/etc/init.d/sctl restart".to_string(),
                    PathBuf::from("/usr/lib/sctl/rollback"),
                    4096,
                    None,
                    None,
                    None,
                )
            }
            Layout::GzTmp => {
                files.insert(
                    "server".to_string(),
                    PathBuf::from(format!("/usr/local/lib/sctl/sctl-server-{target}.gz")),
                );
                files.insert(
                    "plugin".to_string(),
                    PathBuf::from(format!(
                        "/usr/local/lib/sctl/sctl-comms-quectel-{target}.so.gz"
                    )),
                );
                (
                    "/etc/init.d/sctl restart".to_string(),
                    PathBuf::from("/usr/local/lib/sctl/rollback"),
                    4096,
                    None,
                    None,
                    None,
                )
            }
            Layout::Ramboot => {
                files.insert(
                    "server".to_string(),
                    PathBuf::from("/tmp/sctl/cache/sctl-server.payload"),
                );
                files.insert(
                    "plugin".to_string(),
                    PathBuf::from("/tmp/sctl/cache/sctl-comms.payload"),
                );
                files.insert(
                    "libc".to_string(),
                    PathBuf::from("/tmp/sctl/cache/musl-libc.payload"),
                );
                files.insert(
                    "libgcc".to_string(),
                    PathBuf::from("/tmp/sctl/cache/libgcc.payload"),
                );
                (
                    "/etc/init.d/sctl restart".to_string(),
                    PathBuf::from("/etc/sctl/rollback"),
                    256,
                    None,
                    Some(PathBuf::from("/etc/sctl/ramboot.conf")),
                    Some(PathBuf::from("/tmp/sctl/cache")),
                )
            }
            Layout::Systemd => {
                files.insert("server".to_string(), PathBuf::from("/usr/local/bin/sctl"));
                (
                    "systemctl restart sctl-relay".to_string(),
                    PathBuf::from("/var/lib/sctl/rollback"),
                    65536,
                    Some("sctl-relay".to_string()),
                    None,
                    None,
                )
            }
        };
        Self {
            layout,
            target: target.to_string(),
            files,
            restart,
            rollback_dir,
            health_url: None,
            min_free_kb,
            restart_tmp_kb: if layout == Layout::GzTmp {
                GZ_TMP_RESTART_TMP_KB
            } else {
                0
            },
            restart_tmp_kb_from: ThresholdFrom::Default,
            unit,
            ramboot_conf,
            cache_dir,
            helper_prefix: Vec::new(),
        }
    }

    /// The boot fetcher's config: the file's, else `/etc/sctl/ramboot.conf`.
    pub fn ramboot_conf_path(&self) -> PathBuf {
        self.ramboot_conf
            .clone()
            .unwrap_or_else(|| PathBuf::from(RAMBOOT_CONF_PATH))
    }

    /// The boot fetcher's config, read: its path and its text.
    fn read_ramboot_conf(&self) -> Result<(PathBuf, String), String> {
        let conf_path = self.ramboot_conf_path();
        let conf = std::fs::read_to_string(&conf_path)
            .map_err(|e| format!("{}: {e}", conf_path.display()))?;
        Ok((conf_path, conf))
    }

    /// The binary the boot fetcher runs: `ramboot.conf`'s `BIN`.
    pub fn ramboot_bin(&self) -> Result<PathBuf, String> {
        let (conf_path, conf) = self.read_ramboot_conf()?;
        conf_value(&conf, "BIN").map(PathBuf::from).ok_or_else(|| {
            format!(
                "{}: no BIN, the binary the boot fetcher runs",
                conf_path.display()
            )
        })
    }

    /// The key by which `ramboot.conf` has the boot fetcher run the agent
    /// under a loader: `LOADER`, or `MUSL_LIBC_URL`, whose `libc` the
    /// fetcher then runs as the loader (`devices/common/sctl-ramboot.sh`).
    /// `None` when neither holds a value: the kernel runs `BIN` itself.
    pub fn ramboot_loader(&self) -> Result<Option<&'static str>, String> {
        let (_, conf) = self.read_ramboot_conf()?;
        Ok(["LOADER", "MUSL_LIBC_URL"]
            .into_iter()
            .find(|key| conf_value(&conf, key).is_some()))
    }

    /// How this agent runs, for the upgrade's helper and the tools' name.
    /// Under a loader (`helper_prefix`, the WE826's musl `libc.so
    /// --library-path`) `current_exe()` is the loader, so the binary is the
    /// one `ramboot.conf` names as `BIN`; everywhere else it is
    /// `current_exe()`.
    pub fn running(&self) -> Result<Running, String> {
        let binary = if self.helper_prefix.is_empty() {
            std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?
        } else {
            self.ramboot_bin()?
        };
        Ok(Running {
            binary,
            loader: self.helper_prefix.clone(),
        })
    }

    /// The health URL: the file's, or `server.listen`'s port on loopback.
    pub fn health_url(&self, listen: &str) -> String {
        if let Some(url) = &self.health_url {
            return url.clone();
        }
        let port = listen.rsplit(':').next().unwrap_or("1337");
        format!("http://127.0.0.1:{port}/api/health")
    }

    /// What `sctl install-info` and `/api/health` show.
    pub fn report(&self, listen: &str) -> Value {
        let files: BTreeMap<&str, String> = self
            .files
            .iter()
            .map(|(k, v)| (k.as_str(), v.display().to_string()))
            .collect();
        json!({
            "layout": self.layout.as_str(),
            "target": self.target,
            "files": files,
            "restart": self.restart,
            "rollback_dir": self.rollback_dir.display().to_string(),
            "health_url": self.health_url(listen),
            "min_free_kb": self.min_free_kb,
            "restart_tmp_kb": self.restart_tmp_kb,
            "restart_tmp_kb_from": self.restart_tmp_kb_from,
            "unit": self.unit,
            "ramboot_conf": self.ramboot_conf.as_ref().map(|p| p.display().to_string()),
            "cache_dir": self.cache_dir.as_ref().map(|p| p.display().to_string()),
            "helper_prefix": self.helper_prefix,
        })
    }
}

/// One value of a `ramboot.conf` (`KEY='value'` or `KEY=value`).
pub fn conf_value(conf: &str, key: &str) -> Option<String> {
    conf.lines().find_map(|line| {
        let rest = line.strip_prefix(key)?.strip_prefix('=')?;
        let value = rest.trim().trim_matches('\'').trim_matches('"');
        (!value.is_empty()).then(|| value.to_string())
    })
}

/// The hold file's reason, if the file exists (its first line, or "held").
pub fn hold_file_reason(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let line = text.lines().next().unwrap_or("").trim();
    Some(if line.is_empty() {
        "held".to_string()
    } else {
        line.chars().take(160).collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minimal_file_gets_its_layout_defaults() {
        let info = InstallInfo::parse(
            br#"{"v":1,"layout":"gz-tmp","target":"mips_24kc"}"#,
            "mips_24kc",
        )
        .unwrap();
        assert_eq!(info.layout, Layout::GzTmp);
        assert_eq!(
            info.files["server"],
            PathBuf::from("/usr/local/lib/sctl/sctl-server-mips_24kc.gz")
        );
        assert_eq!(info.restart, "/etc/init.d/sctl restart");
        assert_eq!(
            info.rollback_dir,
            PathBuf::from("/usr/local/lib/sctl/rollback")
        );
        assert_eq!(
            info.health_url("0.0.0.0:1337"),
            "http://127.0.0.1:1337/api/health"
        );
        assert_eq!(info.min_free_kb, 4096);
    }

    /// `restart_tmp_kb` belongs to the device's install record (ADR-014,
    /// ruling 9): what the record says, and only where it says nothing the
    /// `gz-tmp` default, 0 on the layouts whose init asks for no room.
    #[test]
    fn the_restart_threshold_is_the_records_and_defaults_per_layout() {
        let parse = |json: &str| InstallInfo::parse(json.as_bytes(), "mipsel_24kc").unwrap();
        let rut241 =
            parse(r#"{"v":1,"layout":"gz-tmp","target":"mipsel_24kc","restart_tmp_kb":8192}"#);
        assert_eq!(rut241.restart_tmp_kb, 8192);
        assert_eq!(rut241.report("127.0.0.1:1337")["restart_tmp_kb"], 8192);
        assert_eq!(
            parse(r#"{"layout":"gz-tmp","target":"mipsel_24kc"}"#).restart_tmp_kb,
            GZ_TMP_RESTART_TMP_KB
        );
        for layout in ["usr-bin", "ramboot", "systemd"] {
            let json = format!(r#"{{"layout":"{layout}","target":"mipsel_24kc"}}"#);
            assert_eq!(parse(&json).restart_tmp_kb, 0, "{layout}");
        }
        assert_eq!(
            parse(r#"{"layout":"ramboot","target":"mipsel_24kc","restart_tmp_kb":4096}"#)
                .restart_tmp_kb,
            4096
        );
    }

    /// One line of an init, read by its shape and by nothing else.
    #[test]
    fn an_inits_threshold_is_its_first_min_tmp_kb_line_of_digits() {
        assert_eq!(init_tmp_kb("START=95\nMIN_TMP_KB=8192\n"), Some(8192));
        assert_eq!(init_tmp_kb("MIN_TMP_KB=16384\r\nfoo\r\n"), Some(16384));
        assert_eq!(init_tmp_kb("MIN_TMP_KB=8192   \n"), Some(8192));
        // No such line; an indented or commented one is not it.
        assert_eq!(init_tmp_kb("START=95\n"), None);
        assert_eq!(
            init_tmp_kb("# MIN_TMP_KB=8192\n    MIN_TMP_KB=8192\n"),
            None
        );
        assert_eq!(init_tmp_kb("XMIN_TMP_KB=8192\n"), None);
        // Anything but digits: a variable, an expression, a second command,
        // quotes, a sign, a space.
        for value in [
            "${OTHER:-8192}",
            "$((8 * 1024))",
            "8192; rm -rf /tmp/x",
            "'8192'",
            "\"8192\"",
            "-1",
            "+8192",
            "81 92",
            "8192kb",
            "0x2000",
            "",
        ] {
            assert_eq!(
                init_tmp_kb(&format!("MIN_TMP_KB={value}\n")),
                None,
                "{value}"
            );
        }
        // 0, and a number no init asks for.
        assert_eq!(init_tmp_kb("MIN_TMP_KB=0\n"), None);
        assert_eq!(init_tmp_kb("MIN_TMP_KB=000\n"), None);
        assert_eq!(init_tmp_kb("MIN_TMP_KB=1048576\n"), Some(1_048_576));
        assert_eq!(init_tmp_kb("MIN_TMP_KB=1048577\n"), None);
        assert_eq!(init_tmp_kb("MIN_TMP_KB=99999999999999999999\n"), None);
        // The first match only: a later line does not mend a first one
        // that is not digits, nor change a first one that is.
        assert_eq!(init_tmp_kb("MIN_TMP_KB=$X\nMIN_TMP_KB=4096\n"), None);
        assert_eq!(
            init_tmp_kb("MIN_TMP_KB=4096\nMIN_TMP_KB=8192\n"),
            Some(4096)
        );
    }

    /// An installed `gz-tmp` unit whose record gives no threshold takes the
    /// one its own init demands, and the report says where it came from.
    #[test]
    fn a_gz_tmp_record_with_no_threshold_takes_its_inits() {
        let dir = std::env::temp_dir().join(format!("sctl-init-threshold-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let init = dir.join("sctl.init");
        let record = dir.join("install.json");
        let load = |extra: &str| {
            std::fs::write(
                &record,
                format!(
                    r#"{{"v":1,"layout":"gz-tmp","target":"mipsel_24kc","restart":"{} restart"{extra}}}"#,
                    init.display()
                ),
            )
            .unwrap();
            InstallInfo::load(&record, "mipsel_24kc").unwrap().unwrap()
        };
        let read = |info: &InstallInfo| {
            let report = info.report("127.0.0.1:1337");
            (
                report["restart_tmp_kb"].as_u64().unwrap(),
                report["restart_tmp_kb_from"].as_str().unwrap().to_string(),
            )
        };

        // No init there: the default, said so.
        assert_eq!(read(&load("")), (16384, "default".to_string()));
        // The init's own line.
        std::fs::write(
            &init,
            "#!/bin/sh /etc/rc.common\nSTART=95\nMIN_TMP_KB=8192\n",
        )
        .unwrap();
        assert_eq!(read(&load("")), (8192, "init".to_string()));
        // The record's own number wins, whatever the init says.
        assert_eq!(
            read(&load(r#","restart_tmp_kb":12000"#)),
            (12000, "record".to_string())
        );
        // An init that says nothing usable: the default.
        std::fs::write(&init, "MIN_TMP_KB=$(expr 8 \\* 1024)\n").unwrap();
        assert_eq!(read(&load("")), (16384, "default".to_string()));
        // The line past what is read of the file is not read.
        let mut long = "#".repeat(INIT_READ_CAP);
        long.push_str("\nMIN_TMP_KB=8192\n");
        std::fs::write(&init, long).unwrap();
        assert_eq!(read(&load("")), (16384, "default".to_string()));
        // Another layout asks its init nothing.
        std::fs::write(&init, "MIN_TMP_KB=8192\n").unwrap();
        std::fs::write(
            &record,
            format!(
                r#"{{"layout":"usr-bin","target":"mipsel_24kc","restart":"{} restart"}}"#,
                init.display()
            ),
        )
        .unwrap();
        let other = InstallInfo::load(&record, "mipsel_24kc").unwrap().unwrap();
        assert_eq!(read(&other), (0, "default".to_string()));
        // A restart command that names no file by its path: the default.
        std::fs::write(
            &record,
            r#"{"layout":"gz-tmp","target":"mipsel_24kc","restart":"service sctl restart"}"#,
        )
        .unwrap();
        let named = InstallInfo::load(&record, "mipsel_24kc").unwrap().unwrap();
        assert_eq!(read(&named), (16384, "default".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A FIFO or a device where the init should be is never opened: the
    /// load answers at once with the default.
    #[test]
    fn an_init_that_is_not_a_regular_file_is_never_opened() {
        use std::os::unix::fs::OpenOptionsExt;
        let dir = std::env::temp_dir().join(format!("sctl-init-fifo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fifo = dir.join("sctl.init");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).unwrap();
        let record = dir.join("install.json");
        for init in [fifo.as_path(), Path::new("/dev/zero")] {
            std::fs::write(
                &record,
                format!(
                    r#"{{"layout":"gz-tmp","target":"mipsel_24kc","restart":"{} restart"}}"#,
                    init.display()
                ),
            )
            .unwrap();
            // On its own thread: a read that waited on the FIFO would hang
            // the run, so on a timeout the FIFO gets a writer first.
            let (tx, rx) = std::sync::mpsc::channel();
            let path = record.clone();
            std::thread::spawn(move || {
                let _ = tx.send(InstallInfo::load(&path, "mipsel_24kc"));
            });
            let Ok(loaded) = rx.recv_timeout(std::time::Duration::from_secs(5)) else {
                let _ = std::fs::OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(&fifo);
                panic!("the load waited on {}", init.display());
            };
            let info = loaded.unwrap().unwrap();
            assert_eq!(info.restart_tmp_kb, GZ_TMP_RESTART_TMP_KB);
            assert_eq!(info.restart_tmp_kb_from, ThresholdFrom::Default);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_loader_run_unit_names_its_helper_prefix() {
        let info = InstallInfo::parse(
            br#"{"layout":"ramboot","target":"mips_24kc","helper_prefix":["/tmp/sctl/lib/libc.so","--library-path","/tmp/sctl/lib"]}"#,
            "mips_24kc",
        )
        .unwrap();
        assert_eq!(
            info.helper_prefix,
            vec!["/tmp/sctl/lib/libc.so", "--library-path", "/tmp/sctl/lib"]
        );
        assert_eq!(
            info.report("127.0.0.1:1337")["helper_prefix"],
            serde_json::json!(["/tmp/sctl/lib/libc.so", "--library-path", "/tmp/sctl/lib"])
        );
        let plain = InstallInfo::parse(
            br#"{"layout":"ramboot","target":"mipsel_24kc"}"#,
            "mipsel_24kc",
        )
        .unwrap();
        assert!(plain.helper_prefix.is_empty());
        let err = InstallInfo::parse(
            br#"{"layout":"ramboot","target":"mips_24kc","helper_prefix":[""]}"#,
            "mips_24kc",
        )
        .unwrap_err();
        assert!(err.contains("helper_prefix"), "{err}");
    }

    #[test]
    fn overrides_win_and_a_unit_sets_the_restart() {
        let info = InstallInfo::parse(
            br#"{"layout":"systemd","unit":"sctl-relay","files":{"server":"/opt/sctl/sctl"},"min_free_kb":1000,"health_url":"http://127.0.0.1:8443/api/health"}"#,
            "x86_64",
        )
        .unwrap();
        assert_eq!(info.restart, "systemctl restart sctl-relay");
        assert_eq!(info.files["server"], PathBuf::from("/opt/sctl/sctl"));
        assert_eq!(info.min_free_kb, 1000);
        assert_eq!(
            info.health_url("0.0.0.0:1337"),
            "http://127.0.0.1:8443/api/health"
        );
        assert_eq!(info.unit.as_deref(), Some("sctl-relay"));
    }

    #[test]
    fn the_binary_target_wins_over_the_files() {
        let info =
            InstallInfo::parse(br#"{"layout":"usr-bin","target":"armv7"}"#, "riscv64").unwrap();
        assert_eq!(info.target, "riscv64");
    }

    #[test]
    fn bad_files_are_refused() {
        assert!(InstallInfo::parse(br#"{"layout":"floppy"}"#, "x86_64").is_err());
        assert!(InstallInfo::parse(br#"{"v":2,"layout":"usr-bin"}"#, "x86_64").is_err());
        assert!(InstallInfo::parse(b"not json", "x86_64").is_err());
    }

    #[test]
    fn a_missing_file_is_none_and_ramboot_has_its_conf() {
        let missing = Path::new("/nonexistent/sctl-install.json");
        assert_eq!(InstallInfo::load(missing, "x86_64").unwrap(), None);
        let info = InstallInfo::parse(br#"{"layout":"ramboot"}"#, "mips_24kc").unwrap();
        assert_eq!(
            info.ramboot_conf,
            Some(PathBuf::from("/etc/sctl/ramboot.conf"))
        );
        assert_eq!(
            info.files["server"],
            PathBuf::from("/tmp/sctl/cache/sctl-server.payload")
        );
        assert!(!info.layout.can_prove_binary());
    }

    #[test]
    fn the_report_names_every_field() {
        let info = InstallInfo::defaults(Layout::UsrBin, "x86_64");
        let report = info.report("127.0.0.1:1337");
        assert_eq!(report["layout"], "usr-bin");
        assert_eq!(report["files"]["server"], "/usr/bin/sctl");
        assert_eq!(report["health_url"], "http://127.0.0.1:1337/api/health");
    }

    #[test]
    fn conf_values_are_read_quoted_or_bare() {
        let conf =
            "RUN_DIR='/tmp/sctl'\nBIN=\"/tmp/sctl/sctl-server\"\nMIN_TMP_KB=24576\nEMPTY=''\n";
        assert_eq!(
            conf_value(conf, "BIN").as_deref(),
            Some("/tmp/sctl/sctl-server")
        );
        assert_eq!(conf_value(conf, "RUN_DIR").as_deref(), Some("/tmp/sctl"));
        assert_eq!(conf_value(conf, "MIN_TMP_KB").as_deref(), Some("24576"));
        assert_eq!(conf_value(conf, "EMPTY"), None);
        assert_eq!(conf_value(conf, "BINARY"), None);
    }

    /// The upgrade's helper and the tools' name find the agent the same way:
    /// `current_exe()`, or under a loader `ramboot.conf`'s `BIN` with the
    /// loader's words.
    #[test]
    fn the_running_binary_is_current_exe_or_under_a_loader_ramboots_bin() {
        let dir = std::env::temp_dir().join(format!("sctl-install-running-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let plain = InstallInfo::defaults(Layout::GzTmp, "mips_24kc");
        let running = plain.running().unwrap();
        assert_eq!(running.binary, std::env::current_exe().unwrap());
        assert!(running.loader.is_empty());

        let conf = dir.join("ramboot.conf");
        let mut loader_run = InstallInfo::defaults(Layout::Ramboot, "mips_24kc");
        loader_run.ramboot_conf = Some(conf.clone());
        loader_run.helper_prefix = vec![
            "/tmp/sctl/lib/libc.so".into(),
            "--library-path".into(),
            "/tmp/sctl/lib".into(),
        ];
        let err = loader_run.running().unwrap_err();
        assert!(err.contains("ramboot.conf"), "no file: {err}");
        std::fs::write(
            &conf,
            "RUN_DIR='/tmp/sctl'\nSERVER_URL='http://192.0.2.10/s'\n",
        )
        .unwrap();
        let err = loader_run.running().unwrap_err();
        assert!(err.contains("no BIN"), "{err}");
        std::fs::write(&conf, "RUN_DIR='/tmp/sctl'\nBIN='/tmp/sctl/sctl-server'\n").unwrap();
        assert_eq!(
            loader_run.running().unwrap(),
            Running {
                binary: PathBuf::from("/tmp/sctl/sctl-server"),
                loader: loader_run.helper_prefix.clone(),
            }
        );
        assert_eq!(
            InstallInfo::defaults(Layout::GzTmp, "mips_24kc").ramboot_conf_path(),
            PathBuf::from(RAMBOOT_CONF_PATH)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The boot fetcher runs the agent under a loader when `ramboot.conf`
    /// names one (`LOADER`) or names the musl `libc` it runs as one
    /// (`MUSL_LIBC_URL`); an empty value names none, as in the fetcher.
    #[test]
    fn ramboot_conf_says_whether_a_loader_runs_the_agent() {
        let dir = std::env::temp_dir().join(format!("sctl-install-loader-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let conf = dir.join("ramboot.conf");
        let mut install = InstallInfo::defaults(Layout::Ramboot, "mipsel_24kc");
        install.ramboot_conf = Some(conf.clone());
        let err = install.ramboot_loader().unwrap_err();
        assert!(err.contains("ramboot.conf"), "no file, not known: {err}");
        for (text, loader) in [
            ("RUN_DIR='/tmp/sctl'\nBIN='/tmp/sctl/sctl-server'\n", None),
            ("SERVER_URL='http://192.0.2.10/s'\n", None),
            ("MUSL_LIBC_URL=''\nLOADER=\n", None),
            ("# LOADER='/tmp/sctl/lib/libc.so'\nLOADER_ARGS=1\n", None),
            (
                "BIN='/tmp/sctl/sctl-server'\nMUSL_LIBC_URL='http://192.0.2.10/libc.so.gz'\n",
                Some("MUSL_LIBC_URL"),
            ),
            ("LOADER='/tmp/sctl/lib/libc.so'\n", Some("LOADER")),
        ] {
            std::fs::write(&conf, text).unwrap();
            assert_eq!(install.ramboot_loader().unwrap(), loader, "{text}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_hold_file_reason_is_its_first_line() {
        let dir = std::env::temp_dir().join(format!("sctl-hold-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("upgrade-hold");
        assert_eq!(hold_file_reason(&path), None);
        std::fs::write(&path, "").unwrap();
        assert_eq!(hold_file_reason(&path).as_deref(), Some("held"));
        std::fs::write(&path, "tournament weekend\nmore").unwrap();
        assert_eq!(
            hold_file_reason(&path).as_deref(),
            Some("tournament weekend")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
