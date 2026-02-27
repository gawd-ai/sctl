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

use axum::{extract::State, http::StatusCode, Json};
use serde_json::{json, Value};

use crate::AppState;

/// `GET /api/info` — system information snapshot.
///
/// Returns a JSON object with device serial, hostname, kernel version, uptime,
/// CPU model, load averages, memory stats, disk usage, and network interfaces
/// with IP addresses.
pub async fn info(State(state): State<AppState>) -> Result<Json<Value>, StatusCode> {
    let hostname = read_proc_file("/proc/sys/kernel/hostname");
    let kernel = read_proc_file("/proc/version");
    let uptime_str = read_proc_file("/proc/uptime");
    let meminfo = read_proc_file("/proc/meminfo");
    let loadavg = read_proc_file("/proc/loadavg");
    let cpuinfo = read_proc_file("/proc/cpuinfo");

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let system_uptime = uptime_str
        .split_whitespace()
        .next()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0) as u64;

    let (mem_total, mem_available) = parse_meminfo(&meminfo);
    let load = parse_loadavg(&loadavg);
    let cpu_model = parse_cpu_model(&cpuinfo);

    // Collect network interfaces with IP addresses
    let interfaces = collect_interfaces().await;

    // Disk usage for root filesystem
    let disk = get_disk_usage("/");

    let mut response = json!({
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
        "disk": disk,
        "interfaces": interfaces,
    });

    // Include tunnel status if tunnel client mode is configured
    if let Some(ref tc) = state.config.tunnel {
        if tc.url.is_some() && !tc.relay {
            response["tunnel"] = json!({
                "connected": state.tunnel_stats.connected.load(std::sync::atomic::Ordering::Relaxed),
                "relay_url": tc.url,
                "reconnects": state.tunnel_stats.reconnects.load(std::sync::atomic::Ordering::Relaxed),
            });
        }
    }

    // Include GPS data if configured
    if let Some(ref gps_state) = state.gps_state {
        let gs = gps_state.lock().await;
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
    }

    // Include LTE signal + modem data if configured
    if let Some(ref lte_state) = state.lte_state {
        let ls = lte_state.lock().await;
        let mut lte = json!({});
        if let Some(ref sig) = ls.signal {
            lte = json!({
                "rssi_dbm": sig.rssi_dbm,
                "rsrp": sig.rsrp,
                "rsrq": sig.rsrq,
                "sinr": sig.sinr,
                "band": sig.band,
                "operator": sig.operator,
                "technology": sig.technology,
                "cell_id": sig.cell_id,
                "signal_bars": sig.signal_bars,
            });
        }
        if let Some(ref modem) = ls.modem {
            lte["modem"] = json!({
                "model": modem.model,
                "firmware": modem.firmware,
                "imei": modem.imei,
                "iccid": modem.iccid,
            });
        }
        if !lte.as_object().is_none_or(serde_json::Map::is_empty) {
            response["lte"] = lte;
        }
    }

    Ok(Json(response))
}

fn read_proc_file(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Parse `MemTotal` and `MemAvailable` from `/proc/meminfo` content.
fn parse_meminfo(meminfo: &str) -> (u64, u64) {
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
fn parse_loadavg(loadavg: &str) -> Vec<f64> {
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
/// Primary approach: parse `ip -j addr show` JSON output (reliable on `OpenWrt`).
/// Fallback: /proc/net/dev + /sys/class/net for names/MAC/state.
async fn collect_interfaces() -> Vec<Value> {
    // Try `ip -j addr show` first — reliable on modern Linux & OpenWrt
    if let Some(ifaces) = collect_interfaces_via_ip_json().await {
        return ifaces;
    }

    // Fallback: /proc + /sys only (no IP addresses, but names/MAC/state)
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

    interfaces
}

/// Parse `ip -j addr show` JSON output into interface objects.
///
/// Returns `None` if `ip` is unavailable or produces non-JSON output,
/// causing the caller to fall back to the proc/sysfs approach.
async fn collect_interfaces_via_ip_json() -> Option<Vec<Value>> {
    let output = tokio::process::Command::new("ip")
        .args(["-j", "addr", "show"])
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let json: Vec<Value> = serde_json::from_slice(&output.stdout).ok()?;
    let mut interfaces = Vec::new();

    for iface in &json {
        let name = iface["ifname"].as_str().unwrap_or("");
        if name == "lo" || name.is_empty() {
            continue;
        }

        let state = iface["operstate"]
            .as_str()
            .unwrap_or("UNKNOWN")
            .to_uppercase();
        let mac = iface["address"].as_str().unwrap_or("");

        let addresses: Vec<String> = iface["addr_info"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|a| {
                        let local = a["local"].as_str()?;
                        let prefixlen = a["prefixlen"].as_u64().unwrap_or(0);
                        Some(format!("{local}/{prefixlen}"))
                    })
                    .collect()
            })
            .unwrap_or_default();

        interfaces.push(json!({
            "name": name,
            "state": state,
            "mac": mac,
            "addresses": addresses,
        }));
    }

    Some(interfaces)
}

fn read_sys_file(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Get disk usage for a filesystem via the POSIX `statvfs` syscall.
///
/// Returns `null` on failure (e.g. path doesn't exist, or `statvfs` errors).
fn get_disk_usage(path: &str) -> Value {
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
