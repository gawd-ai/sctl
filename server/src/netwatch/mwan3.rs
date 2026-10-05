//! The verdict of the unit's own failover engine (mwan3) on each uplink,
//! read beside the kernel's routes (ADR-005).
//!
//! mwan3 tracks every uplink it manages by pinging through it and writes
//! `online` or `offline` to `/var/run/mwan3/iface_state/<iface>` whenever
//! its verdict changes; its traffic steering (policy tables and fwmark
//! rules) follows that file, not the main table, so a wire with link and a
//! lease but no internet keeps its main-table default route while mwan3
//! calls it offline. The agent reports that verdict per kernel interface
//! and interprets nothing: which uplink carries the site is the engine's
//! call, what to show and alert on is the fleet's.
//!
//! Nothing polls. The directory is watched with inotify and its events wake
//! the same dump-and-publish cycle netlink events do; the files are read on
//! every dump. mwan3 names interfaces by their netifd (logical) name, so a
//! read resolves them to kernel devices through `ubus call network.interface
//! dump` once per change. A unit without mwan3 has no directory, no watch
//! and no field.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use tokio::io::unix::AsyncFd;
use tracing::{debug, info, warn};

use super::NetState;

/// Where mwan3 keeps its verdicts; `SCTL_MWAN3_STATE_DIR` overrides it for
/// tests and benches.
pub const STATE_DIR: &str = "/var/run/mwan3/iface_state";
const STATE_DIR_ENV: &str = "SCTL_MWAN3_STATE_DIR";
/// `ubus` answers in milliseconds; a hung rpcd must not hold the dump.
const UBUS_TIMEOUT: Duration = Duration::from_secs(2);

/// What the failover engine holds about an uplink.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Online,
    Offline,
}

impl Verdict {
    fn parse(text: &str) -> Option<Self> {
        match text.trim() {
            "online" => Some(Self::Online),
            "offline" => Some(Self::Offline),
            _ => None,
        }
    }
}

/// The directory to read: the override when set, else [`STATE_DIR`].
pub fn state_dir() -> PathBuf {
    std::env::var_os(STATE_DIR_ENV)
        .filter(|v| !v.is_empty())
        .map_or_else(|| PathBuf::from(STATE_DIR), PathBuf::from)
}

/// The verdict per logical (netifd) interface: one file each, `online` or
/// `offline`. Other contents (mwan3 writes nothing else today) and unreadable
/// files are skipped. An absent directory is an empty map, not an error.
pub fn read_states(dir: &Path) -> HashMap<String, Verdict> {
    let mut states = HashMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return states;
    };
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        if let Some(v) = Verdict::parse(&text) {
            states.insert(name, v);
        } else {
            debug!("mwan3: {name}: unknown verdict {:?}", text.trim());
        }
    }
    states
}

/// Logical interface to kernel device, from netifd's own dump: `l3_device`
/// when the interface is up, else `device`. None when `ubus` is missing,
/// times out or answers something else.
pub async fn resolve_devices() -> Option<HashMap<String, String>> {
    let output = tokio::time::timeout(
        UBUS_TIMEOUT,
        tokio::process::Command::new("ubus")
            .args(["call", "network.interface", "dump"])
            .kill_on_drop(true)
            .output(),
    )
    .await;
    let output = match output {
        Ok(Ok(o)) if o.status.success() => o,
        Ok(Ok(o)) => {
            debug!("mwan3: ubus network.interface dump exited {}", o.status);
            return None;
        }
        Ok(Err(e)) => {
            debug!("mwan3: ubus not run: {e}");
            return None;
        }
        Err(_) => {
            warn!("mwan3: ubus network.interface dump took over {UBUS_TIMEOUT:?}; verdicts not mapped this round");
            return None;
        }
    };
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    Some(parse_dump(&value))
}

/// The mapping in a `network.interface dump` answer.
pub fn parse_dump(value: &serde_json::Value) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Some(list) = value.get("interface").and_then(|v| v.as_array()) else {
        return map;
    };
    for entry in list {
        let Some(name) = entry.get("interface").and_then(|v| v.as_str()) else {
            continue;
        };
        let dev = entry
            .get("l3_device")
            .and_then(|v| v.as_str())
            .or_else(|| entry.get("device").and_then(|v| v.as_str()));
        if let Some(dev) = dev.filter(|d| !d.is_empty()) {
            map.insert(name.to_string(), dev.to_string());
        }
    }
    map
}

/// Set `verdict` on every interface of `net` that a verdict names, through
/// `devices` (logical name to kernel device). A verdict whose interface maps
/// to no device, or to a device the dump did not list, is dropped with a
/// debug line: never guessed.
pub fn apply(
    net: &mut NetState,
    states: &HashMap<String, Verdict>,
    devices: &HashMap<String, String>,
) {
    for (logical, verdict) in states {
        let Some(dev) = devices.get(logical) else {
            debug!("mwan3: {logical} ({verdict:?}) maps to no device; dropped");
            continue;
        };
        if let Some(iface) = net.interfaces.iter_mut().find(|i| &i.name == dev) {
            iface.verdict = Some(*verdict);
        } else {
            debug!("mwan3: {logical} maps to {dev}, not in the dump; dropped");
        }
    }
}

/// Read the verdicts and stamp them onto `net`. A unit without mwan3 (no
/// directory) returns at once without running `ubus`.
pub async fn annotate(net: &mut NetState) {
    let states = read_states(&state_dir());
    if states.is_empty() {
        return;
    }
    let Some(devices) = resolve_devices().await else {
        return;
    };
    apply(net, &states, &devices);
}

/// nix's Inotify offers `AsFd`; tokio's AsyncFd wants `AsRawFd`.
struct Watched(nix::sys::inotify::Inotify);

impl std::os::unix::io::AsRawFd for Watched {
    fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        use std::os::unix::io::AsFd;
        self.0.as_fd().as_raw_fd()
    }
}

/// The inotify watch on the verdict directory. Absent until the directory
/// exists; [`Watcher::arm`] tries again after each dump (a `stat`, no
/// timer), so mwan3 starting after the agent is still seen.
pub struct Watcher {
    dir: PathBuf,
    fd: Option<AsyncFd<Watched>>,
    warned: bool,
}

impl Watcher {
    pub fn new() -> Self {
        Self {
            dir: state_dir(),
            fd: None,
            warned: false,
        }
    }

    /// Open the watch when the directory exists and none is held.
    pub fn arm(&mut self) {
        if self.fd.is_some() || !self.dir.is_dir() {
            return;
        }
        match open_watch(&self.dir) {
            Ok(fd) => {
                info!("mwan3: watching {} for uplink verdicts", self.dir.display());
                self.fd = Some(fd);
            }
            Err(e) => {
                if !self.warned {
                    warn!(
                        "mwan3: cannot watch {}: {e}; verdicts are read on network changes only",
                        self.dir.display()
                    );
                    self.warned = true;
                }
            }
        }
    }

    /// Wait until a verdict file is written. Pends forever without a watch.
    /// A failed read drops the watch; the next [`Watcher::arm`] reopens it.
    pub async fn changed(&mut self) {
        let Some(fd) = self.fd.as_ref() else {
            return std::future::pending().await;
        };
        loop {
            let mut guard = match fd.readable().await {
                Ok(g) => g,
                Err(e) => {
                    warn!("mwan3: watch ended: {e}");
                    self.fd = None;
                    return std::future::pending().await;
                }
            };
            match guard.try_io(|inner| inner.get_ref().0.read_events().map_err(io::Error::from)) {
                Ok(Ok(events)) => {
                    if !events.is_empty() {
                        debug!("mwan3: verdict directory changed");
                        return;
                    }
                }
                Ok(Err(e)) => {
                    warn!("mwan3: watch read: {e}");
                    self.fd = None;
                    return std::future::pending().await;
                }
                Err(_would_block) => {}
            }
        }
    }
}

impl Default for Watcher {
    fn default() -> Self {
        Self::new()
    }
}

fn open_watch(dir: &Path) -> io::Result<AsyncFd<Watched>> {
    use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};
    let inotify = Inotify::init(InitFlags::IN_NONBLOCK | InitFlags::IN_CLOEXEC)?;
    inotify.add_watch(
        dir,
        AddWatchFlags::IN_CLOSE_WRITE
            | AddWatchFlags::IN_MOVED_TO
            | AddWatchFlags::IN_CREATE
            | AddWatchFlags::IN_DELETE,
    )?;
    AsyncFd::new(Watched(inotify))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::netwatch::Interface;

    fn tmp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sctl-mwan3-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn iface(name: &str) -> Interface {
        Interface {
            name: name.to_string(),
            index: 0,
            operstate: "up",
            carrier: Some(true),
            master: None,
            ipv4: None,
            default_metric: None,
            verdict: None,
        }
    }

    #[test]
    fn reads_online_offline_and_skips_the_rest() {
        let dir = tmp();
        std::fs::write(dir.join("wan"), "offline\n").unwrap();
        std::fs::write(dir.join("modem_1_1_2"), "online").unwrap();
        std::fs::write(dir.join("tethering"), "disabled").unwrap();
        std::fs::write(dir.join("garbage"), [0xff, 0xfe]).unwrap();
        let states = read_states(&dir);
        assert_eq!(states.len(), 2);
        assert_eq!(states["wan"], Verdict::Offline);
        assert_eq!(states["modem_1_1_2"], Verdict::Online);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_directory_is_no_verdict() {
        assert!(read_states(Path::new("/nonexistent/sctl-mwan3")).is_empty());
    }

    #[test]
    fn netifd_dump_maps_logical_names_to_devices() {
        let dump = serde_json::json!({"interface": [
            {"interface": "wan", "up": true, "device": "eth1", "l3_device": "eth1"},
            {"interface": "modem_1_1_2", "up": true, "device": "wwan0", "l3_device": "wwan0"},
            {"interface": "tethering", "up": false, "device": "eth2"},
            {"interface": "loopback", "up": true, "l3_device": "lo"},
            {"up": true, "l3_device": "nameless"}
        ]});
        let map = parse_dump(&dump);
        assert_eq!(map["wan"], "eth1");
        assert_eq!(map["modem_1_1_2"], "wwan0");
        assert_eq!(map["tethering"], "eth2");
        assert_eq!(map.len(), 4);
        assert!(parse_dump(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn apply_stamps_mapped_interfaces_and_drops_the_rest() {
        let mut net = NetState {
            interfaces: vec![iface("eth1"), iface("wwan0"), iface("br-lan")],
            ..NetState::default()
        };
        let states = HashMap::from([
            ("wan".to_string(), Verdict::Offline),
            ("modem_1_1_2".to_string(), Verdict::Online),
            ("tethering".to_string(), Verdict::Offline),
            ("ghost".to_string(), Verdict::Online),
        ]);
        let devices = HashMap::from([
            ("wan".to_string(), "eth1".to_string()),
            ("modem_1_1_2".to_string(), "wwan0".to_string()),
            ("tethering".to_string(), "eth2".to_string()),
        ]);
        apply(&mut net, &states, &devices);
        assert_eq!(
            net.interface("eth1").unwrap().verdict,
            Some(Verdict::Offline)
        );
        assert_eq!(
            net.interface("wwan0").unwrap().verdict,
            Some(Verdict::Online)
        );
        assert_eq!(net.interface("br-lan").unwrap().verdict, None);
    }

    #[test]
    fn a_verdict_flip_alone_is_a_network_change() {
        let before = NetState {
            interfaces: vec![iface("eth1")],
            ..NetState::default()
        };
        let mut after = before.clone();
        after.interfaces[0].verdict = Some(Verdict::Offline);
        assert!(!before.same_network(&after));
    }

    #[tokio::test]
    async fn the_watch_wakes_on_a_verdict_write() {
        let dir = tmp();
        let mut watcher = Watcher {
            dir: dir.clone(),
            fd: None,
            warned: false,
        };
        watcher.arm();
        assert!(watcher.fd.is_some(), "inotify should be available in tests");
        let writer = dir.clone();
        let write = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            std::fs::write(writer.join("wan"), "offline").unwrap();
        });
        tokio::time::timeout(Duration::from_secs(5), watcher.changed())
            .await
            .expect("the watch wakes on the write");
        write.await.unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn without_the_directory_the_watch_pends() {
        let mut watcher = Watcher {
            dir: PathBuf::from("/nonexistent/sctl-mwan3"),
            fd: None,
            warned: false,
        };
        watcher.arm();
        assert!(watcher.fd.is_none());
        assert!(
            tokio::time::timeout(Duration::from_millis(100), watcher.changed())
                .await
                .is_err()
        );
    }
}
