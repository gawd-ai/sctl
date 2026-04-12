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
use tracing::{info, warn};

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
    let has_lte = state.lte_state.is_some();
    info!(
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
        info!(req_id, proc_ms, "api.info: phase proc complete");

        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let system_uptime = uptime_str
            .split_whitespace()
            .next()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0) as u64;

        let (mem_total, mem_available) = parse_meminfo(&meminfo);
        let load = parse_loadavg(&loadavg);
        let cpu_model = parse_cpu_model(&cpuinfo);
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
        info!(
            req_id,
            interfaces_ms,
            interface_count = interfaces.len(),
            "api.info: phase interfaces complete"
        );
        response["interfaces"] = json!(interfaces);
    }

    if groups.disk {
        let disk_started = Instant::now();
        let disk = get_disk_usage("/");
        #[allow(clippy::cast_possible_truncation)]
        let disk_ms = disk_started.elapsed().as_millis() as u64;
        info!(req_id, disk_ms, "api.info: phase disk complete");
        response["disk"] = disk;
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

    if groups.gps {
        if let Some(ref gps_state) = state.gps_state {
            let gps_lock_started = Instant::now();
            let gs = gps_state.lock().await;
            #[allow(clippy::cast_possible_truncation)]
            let gps_lock_wait_ms = gps_lock_started.elapsed().as_millis() as u64;
            let fix_age_secs = gs.last_fix_at.map(|t| t.elapsed().as_secs());
            if let Some(ref fix) = gs.last_fix {
                response["gps"] = json!({
                    "status": gs.status,
                    "latitude": fix.latitude,
                    "longitude": fix.longitude,
                    "altitude": fix.altitude,
                    "satellites": fix.satellites,
                    "speed_kmh": fix.speed_kmh,
                    "hdop": fix.hdop,
                    "fix_age_secs": fix_age_secs,
                });
            } else {
                response["gps"] = json!({
                    "status": gs.status,
                });
            }
            info!(req_id, gps_lock_wait_ms, "api.info: phase gps complete");
        }
    }

    let mut lte_lock_wait_ms = 0u64;
    if groups.lte {
        if let Some(ref lte_state) = state.lte_state {
            let lock_started = Instant::now();
            let ls = lte_state.lock().await;
            #[allow(clippy::cast_possible_truncation)]
            {
                lte_lock_wait_ms = lock_started.elapsed().as_millis() as u64;
            }
            let mut lte = if let Some(ref sig) = ls.signal {
                json!({
                    "rssi_dbm": sig.rssi_dbm,
                    "rsrp": sig.rsrp,
                    "rsrq": sig.rsrq,
                    "sinr": sig.sinr,
                    "band": sig.band,
                    "operator": sig.operator,
                    "technology": sig.technology,
                    "cell_id": sig.cell_id,
                    "signal_bars": sig.signal_bars,
                    "pci": sig.pci,
                    "earfcn": sig.earfcn,
                    "freq_band": sig.freq_band,
                    "tac": sig.tac,
                    "plmn": sig.plmn,
                    "enodeb_id": sig.enodeb_id,
                    "sector": sig.sector,
                    "ul_bw_mhz": sig.ul_bw_mhz,
                    "dl_bw_mhz": sig.dl_bw_mhz,
                    "connection_state": sig.connection_state,
                    "duplex": sig.duplex,
                    "neighbors": sig.neighbors,
                    "band_config": sig.band_config,
                })
            } else {
                json!({"status": "no_signal"})
            };
            if let Some(ref modem) = ls.modem {
                lte["modem"] = json!({
                    "model": modem.model,
                    "firmware": modem.firmware,
                    "imei": modem.imei,
                    "iccid": modem.iccid,
                });
            }
            response["lte"] = lte;
            info!(req_id, lte_lock_wait_ms, "api.info: phase lte complete");
        }
    }

    let serialize_started = Instant::now();
    let response_body_len = serde_json::to_string(&response)
        .map(|s| s.len())
        .unwrap_or(0);
    #[allow(clippy::cast_possible_truncation)]
    let serialize_ms = serialize_started.elapsed().as_millis() as u64;
    info!(
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
    info!(
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
            info!(
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
        info!(
            req_id,
            "api.info: interface address enumeration disabled by config"
        );
    }

    #[allow(clippy::cast_possible_truncation)]
    let total_ms = start.elapsed().as_millis() as u64;
    info!(
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
    info!(
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
