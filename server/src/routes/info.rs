//! System information endpoint.
//!
//! `GET /api/info` returns a comprehensive snapshot of the device: identity,
//! kernel, uptime, CPU, memory, disk, and network interfaces with IP addresses.
//!
//! ## Data sources
//!
//! | Field          | Source                                              |
//! |----------------|-----------------------------------------------------|
//! | `hostname`     | `/proc/sys/kernel/hostname`                         |
//! | `kernel`       | `/proc/version`                                     |
//! | `system_uptime_secs` | `/proc/uptime`                                |
//! | `cpu_model`    | `/proc/cpuinfo` (`model name` or `Hardware`)        |
//! | `load_average` | `/proc/loadavg`                                     |
//! | `memory`       | `/proc/meminfo`                                     |
//! | `disk`         | `statvfs("/")` syscall                              |
//! | `interfaces`   | `ip -j addr show` (fallback: `/proc/net/dev` + sysfs) |

use axum::{
    extract::{Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::time::Instant;
use tracing::{debug, warn};

use crate::AppState;

#[derive(Debug, Default, Deserialize)]
pub struct InfoQuery {
    pub groups: Option<String>,
}

#[derive(Debug, Clone, Copy)]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct InfoGroups {
    core: bool,
    interfaces: bool,
    disk: bool,
    tunnel: bool,
    gps: bool,
    lte: bool,
}

impl InfoGroups {
    fn all() -> Self {
        Self {
            core: true,
            interfaces: true,
            disk: true,
            tunnel: true,
            gps: true,
            lte: true,
        }
    }

    fn from_csv(groups: Option<&str>) -> Self {
        let Some(groups) = groups else {
            return Self::all();
        };
        let requested: HashSet<_> = groups
            .split(',')
            .map(str::trim)
            .filter(|g| !g.is_empty())
            .collect();
        if requested.is_empty() || requested.contains("all") {
            return Self::all();
        }
        Self {
            core: requested.contains("core"),
            interfaces: requested.contains("interfaces"),
            disk: requested.contains("disk"),
            tunnel: requested.contains("tunnel"),
            gps: requested.contains("gps"),
            lte: requested.contains("lte"),
        }
    }
}

/// `GET /api/info` — system information snapshot.
///
/// Returns a JSON object with device serial, hostname, kernel version, uptime,
/// CPU model, load averages, memory stats, disk usage, and network interfaces
/// with IP addresses.
pub async fn info(
    State(state): State<AppState>,
    Query(query): Query<InfoQuery>,
) -> Result<Json<Value>, StatusCode> {
    info_with_groups(state, InfoGroups::from_csv(query.groups.as_deref())).await
}

pub(crate) async fn info_with_groups(
    state: AppState,
    groups: InfoGroups,
) -> Result<Json<Value>, StatusCode> {
    let start = Instant::now();
    let req_id = uuid::Uuid::new_v4().to_string();
    let has_lte = state.config.lte.is_some();
    debug!(
        req_id,
        has_lte,
        groups = ?groups,
        "api.info: begin"
    );

    let mut response = json!({});

    if groups.core {
        let proc_started = Instant::now();
        let hostname = read_proc_file("/proc/sys/kernel/hostname");
        let kernel = read_proc_file("/proc/version");
        let uptime_str = read_proc_file("/proc/uptime");
        let meminfo = read_proc_file("/proc/meminfo");
        let loadavg = read_proc_file("/proc/loadavg");
        let cpuinfo = read_proc_file("/proc/cpuinfo");
        #[allow(clippy::cast_possible_truncation)]
        let proc_ms = proc_started.elapsed().as_millis() as u64;
        debug!(req_id, proc_ms, "api.info: phase proc complete");

        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let system_uptime = uptime_str
            .split_whitespace()
            .next()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0) as u64;

        let (mem_total, mem_available) = parse_meminfo(&meminfo);
        let load = parse_loadavg(&loadavg);
        let cpu_model = parse_cpu_model(&cpuinfo);
        // Inspect safe_mode.flag — best-effort read.
        let safe_mode_flag_path =
            std::path::Path::new(&state.config.server.data_dir).join("safe_mode.flag");
        let safe_mode_block = if safe_mode_flag_path.exists() {
            std::fs::read_to_string(&safe_mode_flag_path)
                .ok()
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                .map_or_else(
                    || json!({ "active": true }),
                    |flag| json!({ "active": true, "flag": flag }),
                )
        } else {
            json!({ "active": false })
        };
        response = json!({
            "serial": state.config.device.serial,
            "hostname": hostname.trim(),
            "kernel": kernel.split(' ').take(3).collect::<Vec<_>>().join(" "),
            "system_uptime_secs": system_uptime,
            "cpu_model": cpu_model,
            "load_average": load,
            "memory": {
                "total_bytes": mem_total * 1024,
                "available_bytes": mem_available * 1024,
                "used_bytes": mem_total.saturating_sub(mem_available) * 1024,
            },
            "safe_mode": safe_mode_block,
        });
    }

    if groups.interfaces {
        let interfaces_started = Instant::now();
        let interfaces = collect_interfaces(
            &req_id,
            state.config.server.include_interface_addresses_in_info,
        )
        .await;
        #[allow(clippy::cast_possible_truncation)]
        let interfaces_ms = interfaces_started.elapsed().as_millis() as u64;
        debug!(
            req_id,
            interfaces_ms,
            interface_count = interfaces.len(),
            "api.info: phase interfaces complete"
        );
        response["interfaces"] = json!(interfaces);
    }

    if groups.disk {
        let disk_started = Instant::now();
        // `disk` stays the root filesystem for back-compat (existing consumers
        // read disk.total_bytes/used_bytes). `disks` is the full picture — a
        // device has multiple storages (e.g. an OpenWrt unit's read-only
        // squashfs root plus a writable overlay), and reporting only `/`
        // (always 100% on squashfs) is misleading.
        let disk = get_disk_usage("/");
        let disks = collect_disks();
        #[allow(clippy::cast_possible_truncation)]
        let disk_ms = disk_started.elapsed().as_millis() as u64;
        debug!(
            req_id,
            disk_ms,
            disk_count = disks.len(),
            "api.info: phase disk complete"
        );
        response["disk"] = disk;
        response["disks"] = json!(disks);
    }

    if groups.tunnel {
        if let Some(ref tc) = state.config.tunnel {
            if tc.url.is_some() && !tc.relay {
                response["tunnel"] = json!({
                    "connected": state.tunnel_stats.connected.load(std::sync::atomic::Ordering::Relaxed),
                    "relay_url": tc.url,
                    "reconnects": state.tunnel_stats.reconnects.load(std::sync::atomic::Ordering::Relaxed),
                });
            }
        }
    }

    if groups.gps && state.config.gps.is_some() {
        let gps_lock_started = Instant::now();
        if let Some(ref comms_state) = state.comms_state {
            let cs = comms_state.lock().await;
            if let Some(ref gps) = cs.gps {
                let mut projected = json!({
                    "status": gps.get("status").cloned().unwrap_or_else(|| json!("unknown")),
                });
                if let Some(fix) = gps.get("last_fix").filter(|v| !v.is_null()) {
                    projected["latitude"] = fix.get("latitude").cloned().unwrap_or(Value::Null);
                    projected["longitude"] = fix.get("longitude").cloned().unwrap_or(Value::Null);
                    projected["altitude"] = fix.get("altitude").cloned().unwrap_or(Value::Null);
                    projected["satellites"] = fix.get("satellites").cloned().unwrap_or(Value::Null);
                    projected["speed_kmh"] = fix.get("speed_kmh").cloned().unwrap_or(Value::Null);
                    // Course over ground. The driver has parsed this since the
                    // GPS support landed (parts[6] of the +QGPSLOC reply) and
                    // serialises it into the fix, but the projection dropped it,
                    // so no consumer has ever been able to see heading. On a
                    // vehicle that is the difference between a dot and a track.
                    projected["course"] = fix.get("course").cloned().unwrap_or(Value::Null);
                    projected["hdop"] = fix.get("hdop").cloned().unwrap_or(Value::Null);
                    projected["fix_age_secs"] =
                        gps.get("fix_age_secs").cloned().unwrap_or(Value::Null);
                }
                response["gps"] = projected;
            }
        }
        #[allow(clippy::cast_possible_truncation)]
        let gps_lock_wait_ms = gps_lock_started.elapsed().as_millis() as u64;
        debug!(req_id, gps_lock_wait_ms, "api.info: phase gps complete");
    }

    let mut lte_lock_wait_ms = 0u64;
    if groups.lte && state.config.lte.is_some() {
        if let Some(ref comms_state) = state.comms_state {
            let lock_started = Instant::now();
            let cs = comms_state.lock().await;
            #[allow(clippy::cast_possible_truncation)]
            {
                lte_lock_wait_ms = lock_started.elapsed().as_millis() as u64;
            }
            let mut lte = if let Some(sig) = cs.lte.as_ref().and_then(|v| v.get("signal")) {
                sig.clone()
            } else {
                json!({"status": "no_signal"})
            };
            if let Some(modem) = cs.lte.as_ref().and_then(|v| v.get("modem")) {
                lte["modem"] = modem.clone();
            }
            lte["detected_path"] = json!(cs.detected_path.clone());
            response["lte"] = lte;
            debug!(req_id, lte_lock_wait_ms, "api.info: phase lte complete");
        }
    }

    let serialize_started = Instant::now();
    let response_body_len = serde_json::to_string(&response).map_or(0, |s| s.len());
    #[allow(clippy::cast_possible_truncation)]
    let serialize_ms = serialize_started.elapsed().as_millis() as u64;
    debug!(
        req_id,
        serialize_ms, response_body_len, "api.info: phase serialize complete"
    );

    #[allow(clippy::cast_possible_truncation)]
    let total_ms = start.elapsed().as_millis() as u64;
    if lte_lock_wait_ms >= 250 {
        warn!(
            req_id,
            total_ms, lte_lock_wait_ms, "api.info: slow LTE state lock acquisition"
        );
    }
    debug!(
        req_id,
        total_ms, lte_lock_wait_ms, response_body_len, "api.info: end"
    );

    Ok(Json(response))
}

pub(crate) fn read_proc_file(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Parse `MemTotal` and `MemAvailable` from `/proc/meminfo` content.
pub(crate) fn parse_meminfo(meminfo: &str) -> (u64, u64) {
    let mut total = 0u64;
    let mut available = 0u64;
    for line in meminfo.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total = parse_kb_value(rest);
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            available = parse_kb_value(rest);
        }
    }
    (total, available)
}

fn parse_kb_value(s: &str) -> u64 {
    s.split_whitespace()
        .next()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// Parse the 1/5/15-minute load averages from `/proc/loadavg`.
pub(crate) fn parse_loadavg(loadavg: &str) -> Vec<f64> {
    loadavg
        .split_whitespace()
        .take(3)
        .filter_map(|s| s.parse::<f64>().ok())
        .collect()
}

/// Extract the CPU model string from `/proc/cpuinfo`.
///
/// Handles both x86 (`model name`) and ARM/OpenWrt (`Hardware`) formats.
fn parse_cpu_model(cpuinfo: &str) -> String {
    for line in cpuinfo.lines() {
        if let Some(rest) = line.strip_prefix("model name") {
            if let Some(value) = rest.strip_prefix('\t').and_then(|s| s.strip_prefix(": ")) {
                return value.trim().to_string();
            }
            // Handle "model name : ..." (with spaces)
            if let Some(value) = rest.split(':').nth(1) {
                return value.trim().to_string();
            }
        }
        // ARM / OpenWrt fallback: "Hardware" line
        if let Some(rest) = line.strip_prefix("Hardware") {
            if let Some(value) = rest.split(':').nth(1) {
                return value.trim().to_string();
            }
        }
    }
    "unknown".to_string()
}

/// Collect interfaces with addresses.
/// Primary approach: `getifaddrs` for IPs + sysfs for MAC/state.
/// Fallback: /proc/net/dev + /sys/class/net for names/MAC/state only.
async fn collect_interfaces(req_id: &str, include_addresses: bool) -> Vec<Value> {
    let start = Instant::now();
    let net_dev = read_proc_file("/proc/net/dev");
    let mut interfaces = Vec::new();

    for line in net_dev.lines().skip(2) {
        let name = match line.split(':').next() {
            Some(n) => n.trim().to_string(),
            None => continue,
        };
        if name == "lo" {
            continue;
        }

        let mac = read_sys_file(&format!("/sys/class/net/{name}/address"));
        let operstate = read_sys_file(&format!("/sys/class/net/{name}/operstate"));

        interfaces.push(json!({
            "name": name,
            "state": operstate.trim().to_uppercase(),
            "mac": mac.trim(),
            "addresses": Value::Array(vec![]),
        }));
    }

    if include_addresses {
        let addr_started = Instant::now();
        if let Some(addresses_by_name) = collect_interface_addresses(req_id) {
            #[allow(clippy::cast_possible_truncation)]
            let addr_ms = addr_started.elapsed().as_millis() as u64;
            debug!(
                req_id,
                addr_ms,
                address_interface_count = addresses_by_name.len(),
                "api.info: collect_interface_addresses complete"
            );
            for iface in &mut interfaces {
                let Some(name) = iface["name"].as_str() else {
                    continue;
                };
                if let Some(addresses) = addresses_by_name.get(name) {
                    iface["addresses"] = json!(addresses);
                }
            }
        } else {
            #[allow(clippy::cast_possible_truncation)]
            let addr_ms = addr_started.elapsed().as_millis() as u64;
            warn!(
                req_id,
                addr_ms, "api.info: collect_interface_addresses unavailable"
            );
        }
    } else {
        debug!(
            req_id,
            "api.info: interface address enumeration disabled by config"
        );
    }

    #[allow(clippy::cast_possible_truncation)]
    let total_ms = start.elapsed().as_millis() as u64;
    debug!(
        req_id,
        total_ms,
        interface_count = interfaces.len(),
        "api.info: collect_interfaces complete"
    );

    interfaces
}

/// Enumerate interface addresses without spawning external commands.
fn collect_interface_addresses(
    req_id: &str,
) -> Option<std::collections::HashMap<String, Vec<String>>> {
    let start = Instant::now();
    let mut addresses = std::collections::HashMap::<String, Vec<String>>::new();

    unsafe {
        let mut ifaddrs: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&raw mut ifaddrs) != 0 {
            warn!(
                "system info: getifaddrs failed, falling back to proc/sysfs only: {}",
                std::io::Error::last_os_error()
            );
            return None;
        }

        let mut current = ifaddrs;
        while !current.is_null() {
            let ifa = &*current;
            if !ifa.ifa_name.is_null() && !ifa.ifa_addr.is_null() {
                let name = std::ffi::CStr::from_ptr(ifa.ifa_name)
                    .to_string_lossy()
                    .into_owned();
                if name != "lo" {
                    if let Some(addr) = format_interface_address(ifa.ifa_addr, ifa.ifa_netmask) {
                        addresses.entry(name).or_default().push(addr);
                    }
                }
            }
            current = ifa.ifa_next;
        }

        libc::freeifaddrs(ifaddrs);
    }

    for values in addresses.values_mut() {
        values.sort();
        values.dedup();
    }

    #[allow(clippy::cast_possible_truncation)]
    let total_ms = start.elapsed().as_millis() as u64;
    let address_count: usize = addresses.values().map(std::vec::Vec::len).sum();
    debug!(
        req_id,
        total_ms,
        interface_count = addresses.len(),
        address_count,
        "api.info: getifaddrs complete"
    );

    Some(addresses)
}

/// Format an interface address as `ip/prefixlen`.
#[allow(clippy::cast_ptr_alignment)] // kernel guarantees sockaddr alignment
unsafe fn format_interface_address(
    addr: *const libc::sockaddr,
    netmask: *const libc::sockaddr,
) -> Option<String> {
    match i32::from((*addr).sa_family) {
        libc::AF_INET => {
            let addr_in = &*addr.cast::<libc::sockaddr_in>();
            let ip = std::net::Ipv4Addr::from(u32::from_be(addr_in.sin_addr.s_addr));
            let prefix = prefix_len_v4(netmask);
            Some(format!("{ip}/{prefix}"))
        }
        libc::AF_INET6 => {
            let addr_in6 = &*addr.cast::<libc::sockaddr_in6>();
            let ip = std::net::Ipv6Addr::from(addr_in6.sin6_addr.s6_addr);
            let prefix = prefix_len_v6(netmask);
            Some(format!("{ip}/{prefix}"))
        }
        _ => None,
    }
}

#[allow(clippy::cast_ptr_alignment)]
unsafe fn prefix_len_v4(netmask: *const libc::sockaddr) -> u32 {
    if netmask.is_null() || i32::from((*netmask).sa_family) != libc::AF_INET {
        return 0;
    }
    let mask = &*netmask.cast::<libc::sockaddr_in>();
    u32::from_be(mask.sin_addr.s_addr).count_ones()
}

#[allow(clippy::cast_ptr_alignment)]
unsafe fn prefix_len_v6(netmask: *const libc::sockaddr) -> u32 {
    if netmask.is_null() || i32::from((*netmask).sa_family) != libc::AF_INET6 {
        return 0;
    }
    let mask = &*netmask.cast::<libc::sockaddr_in6>();
    mask.sin6_addr.s6_addr.iter().map(|b| b.count_ones()).sum()
}

fn read_sys_file(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Get disk usage for a filesystem via the POSIX `statvfs` syscall.
///
/// Returns `null` on failure (e.g. path doesn't exist, or `statvfs` errors).
pub(crate) fn get_disk_usage(path: &str) -> Value {
    use std::ffi::CString;
    use std::mem::MaybeUninit;

    let Ok(c_path) = CString::new(path) else {
        return json!(null);
    };

    let mut stat = MaybeUninit::<libc::statvfs>::uninit();

    // SAFETY: statvfs is a standard POSIX call, we pass a valid C string
    // and a pointer to uninitialized but properly aligned memory.
    let ret = unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) };

    if ret != 0 {
        return json!(null);
    }

    // SAFETY: statvfs returned 0, so stat is fully initialized.
    let stat = unsafe { stat.assume_init() };

    #[allow(clippy::unnecessary_cast)]
    let block_size = stat.f_frsize as u64;
    let total = stat.f_blocks * block_size;
    let available = stat.f_bavail * block_size;
    let used = total - (stat.f_bfree * block_size);

    json!({
        "path": path,
        "total_bytes": total,
        "used_bytes": used,
        "available_bytes": available,
    })
}

/// Enumerate mounted filesystems and report per-mount usage for the real
/// (non-pseudo) storage volumes.
///
/// Devices have more than one storage: an OpenWrt/RUTOS unit exposes a
/// read-only squashfs root (always 100% — it's a compressed image), a writable
/// `jffs2`/`ubifs` overlay (the storage that actually fills up), a `tmpfs`
/// working area, and sometimes a separate log partition. Reporting only `/` was
/// both alarming (100%) and wrong (the overlay was invisible).
///
/// Pseudo/kernel filesystems are skipped, `overlay`-type merge mounts are
/// skipped (they mirror the underlying writable partition), and entries sharing
/// a source device + size are de-duplicated (e.g. squashfs `/` and `/rom/*`).
///
/// One mount point yields at most one entry, last one wins. The kernel lets a
/// later mount shadow an earlier one at the same path, so `/proc/mounts`
/// legitimately lists a target twice — an OpenWrt root shows `rootfs on /`
/// followed by `overlayfs:/overlay on /`. Those share neither source nor a
/// skipped fstype, so the two filters above both let them through, and a
/// consumer keying on the mount point has no way to tell which one is real.
/// Last wins because that is the filesystem actually mounted there.
pub(crate) fn collect_disks() -> Vec<Value> {
    let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    disks_from_mounts(&mounts, &get_disk_usage)
}

/// The rules above, applied to a `/proc/mounts` body with usage supplied by the
/// caller — so the selection can be tested against a real device's mount table
/// without that device, or a filesystem, being present.
fn disks_from_mounts(mounts: &str, usage_of: &dyn Fn(&str) -> Value) -> Vec<Value> {
    const SKIP_FSTYPE: &[&str] = &[
        "proc",
        "sysfs",
        "devtmpfs",
        "devpts",
        "cgroup",
        "cgroup2",
        "debugfs",
        "tracefs",
        "securityfs",
        "pstore",
        "bpf",
        "mqueue",
        "hugetlbfs",
        "fusectl",
        "configfs",
        "ramfs",
        "autofs",
        "nsfs",
        "rpc_pipefs",
        "binfmt_misc",
        "fuse.gvfsd-fuse",
        "overlay",
        // Same merge mount, older kernels' name for it. Without this the skip
        // above misses every OpenWrt build before the rename.
        "overlayfs",
    ];

    let mut out: Vec<Value> = Vec::new();
    let mut seen: Vec<(String, u64)> = Vec::new();

    for line in mounts.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 4 {
            continue;
        }
        let source = parts[0];
        let target = parts[1];
        let fstype = parts[2];
        let opts = parts[3];

        if SKIP_FSTYPE.contains(&fstype) {
            continue;
        }
        // Kernel mountpoints that slipped past the fstype filter.
        if target == "/dev" || target.starts_with("/proc") || target.starts_with("/sys") {
            continue;
        }

        let usage = usage_of(target);
        let Some(total) = usage.get("total_bytes").and_then(Value::as_u64) else {
            continue;
        };
        if total == 0 {
            continue;
        }
        // De-dup bind/remount views of the same device+size (squashfs / vs /rom).
        if seen.iter().any(|(s, t)| s == source && *t == total) {
            continue;
        }
        seen.push((source.to_string(), total));

        let read_only = opts.split(',').any(|o| o == "ro");
        let entry = json!({
            "mount": target,
            "fstype": fstype,
            "source": source,
            "read_only": read_only,
            "total_bytes": total,
            "used_bytes": usage.get("used_bytes").cloned().unwrap_or(Value::Null),
            "available_bytes": usage.get("available_bytes").cloned().unwrap_or(Value::Null),
        });
        // One row per mount point: replace in place rather than appending, so a
        // shadowed target keeps its position in the list and the surviving row
        // is the mount that is actually live there.
        match out
            .iter()
            .position(|e| e.get("mount").and_then(Value::as_str) == Some(target))
        {
            Some(i) => out[i] = entry,
            None => out.push(entry),
        }
    }

    out
}

#[cfg(test)]
mod disk_tests {
    use super::disks_from_mounts;
    use serde_json::{json, Value};

    /// Real `/proc/mounts` from WE826-F85E3CD01310, the device whose duplicate
    /// `/` took a fleet page down. Read off the unit, not composed by hand.
    const WE826_MOUNTS: &str = "\
rootfs / rootfs rw 0 0
/dev/root /rom squashfs ro,relatime,errors=continue 0 0
proc /proc proc rw,nosuid,nodev,noexec,relatime 0 0
sysfs /sys sysfs rw,nosuid,nodev,noexec,relatime 0 0
tmpfs /tmp tmpfs rw,nosuid,nodev,noatime 0 0
/dev/mtdblock3 /overlay jffs2 rw,noatime 0 0
overlayfs:/overlay / overlayfs rw,noatime,lowerdir=/,upperdir=/overlay/upper 0 0
tmpfs /dev tmpfs rw,nosuid,noexec,noatime,size=512k,mode=755 0 0
devpts /dev/pts devpts rw,nosuid,noexec,noatime,mode=600 0 0
";

    fn sized(total: u64) -> Value {
        json!({ "total_bytes": total, "used_bytes": total / 3, "available_bytes": total / 2 })
    }

    /// Distinct sizes per target, so the (source, size) de-dup cannot be the
    /// thing that happens to collapse the duplicate — only the mount rule can.
    fn usage(target: &str) -> Value {
        match target {
            "/" | "/overlay" => sized(851_968),
            "/rom" => sized(14_155_776),
            "/tmp" => sized(64_507_904),
            _ => sized(0),
        }
    }

    fn mounts_of(disks: &[Value]) -> Vec<&str> {
        disks
            .iter()
            .filter_map(|d| d.get("mount").and_then(Value::as_str))
            .collect()
    }

    #[test]
    fn a_target_reported_twice_yields_one_row() {
        let disks = disks_from_mounts(WE826_MOUNTS, &usage);
        assert_eq!(mounts_of(&disks), vec!["/", "/rom", "/tmp", "/overlay"]);
    }

    #[test]
    fn merge_mounts_are_skipped_under_either_kernel_name() {
        for fstype in ["overlay", "overlayfs"] {
            let table = format!("merged:/x /merged {fstype} rw 0 0\n");
            assert!(
                disks_from_mounts(&table, &|_| sized(1_000)).is_empty(),
                "{fstype} merge mount should not be reported"
            );
        }
    }

    #[test]
    fn the_live_mount_wins_when_one_shadows_another() {
        let table = "\
rootfs / rootfs rw 0 0
/dev/sda1 / ext4 rw 0 0
";
        let disks = disks_from_mounts(table, &|_| sized(4_096));
        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0]["fstype"], "ext4");
        assert_eq!(disks[0]["source"], "/dev/sda1");
    }

    #[test]
    fn pseudo_filesystems_and_zero_sized_volumes_are_left_out() {
        let disks = disks_from_mounts(WE826_MOUNTS, &usage);
        for skipped in ["/proc", "/sys", "/dev", "/dev/pts"] {
            assert!(!mounts_of(&disks).contains(&skipped), "{skipped} leaked in");
        }
    }
}
