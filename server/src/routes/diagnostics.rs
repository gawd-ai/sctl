//! Server diagnostics endpoint.
//!
//! `GET /api/diagnostics` returns process health, system stats, network state,
//! and recent service logs — designed for on-demand troubleshooting from the
//! frontend dashboard.
//!
//! ## Query parameters
//!
//! | Param       | Default | Description                          |
//! |-------------|---------|--------------------------------------|
//! | `log_lines` | 100     | Max log entries (capped at 500)      |
//! | `log_since` | `24h`   | Time range: `1h`, `6h`, `24h`       |

use axum::{extract::State, http::StatusCode, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use super::info::{get_disk_usage, parse_loadavg, parse_meminfo, read_proc_file};
use crate::AppState;

#[derive(Deserialize)]
pub struct DiagnosticsQuery {
    pub log_lines: Option<u32>,
    pub log_since: Option<String>,
}

/// `GET /api/diagnostics` — server diagnostics snapshot.
pub async fn diagnostics(
    State(state): State<AppState>,
    axum::extract::Query(query): axum::extract::Query<DiagnosticsQuery>,
) -> Result<Json<Value>, StatusCode> {
    let log_lines = query.log_lines.unwrap_or(200).min(1000);
    let log_since = query.log_since.as_deref().unwrap_or("24h");

    let process = collect_process_info(&state);
    let system = collect_system_info();
    let network = collect_network_info();
    let (logs, log_stats) = collect_logs(log_lines, log_since).await;

    Ok(Json(json!({
        "process": process,
        "system": system,
        "network": network,
        "logs": logs,
        "log_stats": log_stats,
    })))
}

/// Collect process-level diagnostics from /proc/self.
fn collect_process_info(state: &AppState) -> Value {
    let pid = std::process::id();
    let uptime_secs = state.start_time.elapsed().as_secs();

    // RSS from /proc/self/status
    let status = read_proc_file("/proc/self/status");
    let rss_bytes = parse_proc_status_field(&status, "VmRSS:");
    let threads = parse_proc_status_field_u32(&status, "Threads:");

    // Open file descriptors
    let open_fds = std::fs::read_dir("/proc/self/fd").map_or(0, |entries| entries.count());

    json!({
        "pid": pid,
        "rss_bytes": rss_bytes,
        "open_fds": open_fds,
        "threads": threads,
        "uptime_secs": uptime_secs,
    })
}

/// Collect system-level diagnostics.
fn collect_system_info() -> Value {
    let hostname = read_proc_file("/proc/sys/kernel/hostname");
    let uptime_str = read_proc_file("/proc/uptime");
    let meminfo = read_proc_file("/proc/meminfo");
    let loadavg_str = read_proc_file("/proc/loadavg");

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let os_uptime_secs = uptime_str
        .split_whitespace()
        .next()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0) as u64;

    let (mem_total_kb, mem_available_kb) = parse_meminfo(&meminfo);
    let load = parse_loadavg(&loadavg_str);
    let disk = get_disk_usage("/");

    let mem_total = mem_total_kb * 1024;
    let mem_available = mem_available_kb * 1024;
    let mem_used_pct = if mem_total > 0 {
        #[allow(clippy::cast_precision_loss)]
        let pct = ((mem_total - mem_available) as f64 / mem_total as f64) * 100.0;
        (pct * 10.0).round() / 10.0
    } else {
        0.0
    };

    json!({
        "hostname": hostname.trim(),
        "os_uptime_secs": os_uptime_secs,
        "load_avg": load,
        "memory": {
            "total_bytes": mem_total,
            "available_bytes": mem_available,
            "used_pct": mem_used_pct,
        },
        "disk": disk,
    })
}

/// Parse TCP connection states from /proc/net/tcp and /proc/net/tcp6.
fn collect_network_info() -> Value {
    let mut established = 0u32;
    let mut listen = 0u32;
    let mut time_wait = 0u32;
    let mut close_wait = 0u32;

    for path in &["/proc/net/tcp", "/proc/net/tcp6"] {
        let content = read_proc_file(path);
        for line in content.lines().skip(1) {
            // Column 4 (0-indexed: 3) is the connection state hex
            if let Some(state_hex) = line.split_whitespace().nth(3) {
                match state_hex {
                    "01" => established += 1, // TCP_ESTABLISHED
                    "0A" => listen += 1,      // TCP_LISTEN
                    "06" => time_wait += 1,   // TCP_TIME_WAIT
                    "08" => close_wait += 1,  // TCP_CLOSE_WAIT
                    _ => {}
                }
            }
        }
    }

    json!({
        "tcp": {
            "established": established,
            "listen": listen,
            "time_wait": time_wait,
            "close_wait": close_wait,
        }
    })
}

/// Collect service logs, trying journalctl first then logread.
async fn collect_logs(max_lines: u32, since: &str) -> (Vec<Value>, Value) {
    // Try journalctl (systemd)
    if let Some(logs) = try_journalctl(max_lines, since).await {
        let stats = compute_log_stats(&logs);
        return (logs, stats);
    }

    // Fallback: logread (OpenWrt)
    if let Some(logs) = try_logread(max_lines).await {
        let stats = compute_log_stats(&logs);
        return (logs, stats);
    }

    (vec![], json!({ "errors": 0, "warnings": 0, "total": 0 }))
}

const DIAGNOSTICS_LOG_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Try journalctl for log retrieval.
///
/// Prefers unit-based query (`-u sctl*`) which includes logs across restarts
/// (tunnel lifecycle: connects, disconnects, heartbeat timeouts from all PIDs).
/// Falls back to PID-based query if no systemd unit matches.
async fn try_journalctl(max_lines: u32, since: &str) -> Option<Vec<Value>> {
    let since_arg = match since {
        "1h" => "1 hour ago",
        "6h" => "6 hours ago",
        _ => "24 hours ago",
    };

    // Try unit name first — includes logs from previous PIDs (restarts)
    if let Some(logs) = try_journalctl_unit(max_lines, since_arg).await {
        return Some(logs);
    }

    // Fallback: current PID only (no systemd unit configured)
    let pid = std::process::id();
    let output = tokio::time::timeout(
        DIAGNOSTICS_LOG_TIMEOUT,
        tokio::process::Command::new("journalctl")
            .args([
                &format!("_PID={pid}"),
                "--output=json",
                &format!("--since={since_arg}"),
                "--no-pager",
            ])
            .output(),
    )
    .await
    .ok()?
    .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut logs: Vec<Value> = Vec::new();

    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<Value>(line) {
            let parsed = parse_journalctl_entry(&entry);
            if let Some(msg) = parsed["message"].as_str() {
                if is_lifecycle_noise(msg) {
                    continue;
                }
            }
            logs.push(parsed);
        }
    }

    // Keep only the last max_lines after filtering
    let start = logs.len().saturating_sub(max_lines as usize);
    let logs = logs.split_off(start);

    Some(logs)
}

/// Parse a single journalctl JSON entry into our log format.
fn parse_journalctl_entry(entry: &Value) -> Value {
    // __REALTIME_TIMESTAMP is microseconds since epoch — return as ISO-ish string
    let timestamp = entry["__REALTIME_TIMESTAMP"]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .map(format_unix_us)
        .unwrap_or_default();

    let priority = entry["PRIORITY"].as_str().unwrap_or("6");
    let level = match priority {
        "0" | "1" | "2" | "3" => "error",
        "4" => "warn",
        "5" => "notice",
        "7" => "debug",
        _ => "info",
    };

    // MESSAGE can be a string or a byte array (when it contains ANSI escape codes)
    let raw_message = if let Some(s) = entry["MESSAGE"].as_str() {
        s.to_string()
    } else if let Some(arr) = entry["MESSAGE"].as_array() {
        let bytes: Vec<u8> = arr
            .iter()
            .filter_map(|v| v.as_u64().map(|n| n as u8))
            .collect();
        strip_ansi_escapes(&String::from_utf8_lossy(&bytes))
    } else {
        String::new()
    };
    // Strip tracing prefix: "2026-03-02T01:25:14.123Z  INFO sctl: actual message"
    let message = strip_tracing_prefix(&raw_message);

    json!({
        "timestamp": timestamp,
        "level": level,
        "message": message,
    })
}

/// Format microseconds since Unix epoch as "YYYY-MM-DD HH:MM:SS" UTC.
#[allow(clippy::cast_possible_wrap)]
fn format_unix_us(us: u64) -> String {
    let secs = us / 1_000_000;
    let subsec = us % 1_000_000;

    // Manual UTC breakdown (avoids chrono dependency)
    let days_since_epoch = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    // Civil date from days since 1970-01-01 (algorithm from Howard Hinnant)
    let (year, month, day) = civil_from_days(days_since_epoch as i64);

    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}.{subsec:06}Z")
}

/// Convert days since 1970-01-01 to (year, month, day).
/// Algorithm from Howard Hinnant's `civil_from_days`.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = i64::from(yoe) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    #[allow(clippy::cast_possible_truncation)]
    (y as i32, m, d)
}

/// Try journalctl with `_COMM=sctl` filter.
///
/// Uses `_COMM=sctl` instead of `-u sctl*` to only get messages written by the
/// sctl binary itself, excluding systemd's own unit lifecycle messages (Started,
/// Stopped, Deactivated, Consumed CPU time, etc.) which create noise.
async fn try_journalctl_unit(max_lines: u32, since_arg: &str) -> Option<Vec<Value>> {
    // Don't use -n here: lifecycle noise filtering means we need all entries in
    // the time window, then truncate after filtering. --since bounds the volume.
    let output = tokio::time::timeout(
        DIAGNOSTICS_LOG_TIMEOUT,
        tokio::process::Command::new("journalctl")
            .args([
                "_COMM=sctl",
                "--output=json",
                &format!("--since={since_arg}"),
                "--no-pager",
            ])
            .output(),
    )
    .await
    .ok()?
    .ok()?;

    if !output.status.success() || output.stdout.is_empty() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut logs: Vec<Value> = Vec::new();

    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<Value>(line) {
            let parsed = parse_journalctl_entry(&entry);
            if let Some(msg) = parsed["message"].as_str() {
                if is_lifecycle_noise(msg) {
                    continue;
                }
            }
            logs.push(parsed);
        }
    }

    // Keep only the last max_lines after filtering
    let start = logs.len().saturating_sub(max_lines as usize);
    let logs = logs.split_off(start);

    if logs.is_empty() {
        None
    } else {
        Some(logs)
    }
}

/// Try logread (OpenWrt/BusyBox syslog).
async fn try_logread(max_lines: u32) -> Option<Vec<Value>> {
    let output = tokio::time::timeout(
        DIAGNOSTICS_LOG_TIMEOUT,
        tokio::process::Command::new("logread")
            .args(["-e", "sctl"])
            .output(),
    )
    .await
    .ok()?
    .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    let start = lines.len().saturating_sub(max_lines as usize);
    let mut logs = Vec::new();

    for line in &lines[start..] {
        let (timestamp, level, message) = parse_logread_line(line);
        logs.push(json!({
            "timestamp": timestamp,
            "level": level,
            "message": message,
        }));
    }

    if logs.is_empty() {
        None
    } else {
        Some(logs)
    }
}

/// Best-effort parse of a logread line.
/// Format: "Mon DD HH:MM:SS YYYY facility.level host prog[pid]: message"
fn parse_logread_line(line: &str) -> (String, &'static str, String) {
    let words: Vec<&str> = line.split_whitespace().collect();

    // Need at least: DOW Month Day Time Year Facility
    if words.len() < 6 {
        return (String::new(), "info", line.to_string());
    }

    // Convert timestamp to ISO 8601 (consistent with journalctl output)
    let month_num = match words[1] {
        "Feb" => "02",
        "Mar" => "03",
        "Apr" => "04",
        "May" => "05",
        "Jun" => "06",
        "Jul" => "07",
        "Aug" => "08",
        "Sep" => "09",
        "Oct" => "10",
        "Nov" => "11",
        "Dec" => "12",
        _ => "01",
    };
    let timestamp = format!("{}-{}-{:0>2}T{}Z", words[4], month_num, words[2], words[3]);

    // Extract level from facility.level field (words[5])
    // BusyBox crond logs all job executions at cron.err — not a real error
    let facility_level = words[5];
    let level = if let Some(dot_pos) = facility_level.rfind('.') {
        let facility = &facility_level[..dot_pos];
        match &facility_level[dot_pos + 1..] {
            "err" | "crit" | "alert" | "emerg" => {
                if facility == "cron" {
                    "info"
                } else {
                    "error"
                }
            }
            "warn" | "warning" => "warn",
            "debug" => "debug",
            _ => "info",
        }
    } else {
        "info"
    };

    // Message is everything after the colon following the program name
    let message = line
        .find("]: ")
        .map(|i| line[i + 3..].to_string())
        .or_else(|| {
            // Some logread formats don't have brackets
            line.splitn(6, ' ').nth(5).map(String::from)
        })
        .unwrap_or_else(|| line.to_string());

    (timestamp, level, message)
}

/// Compute error/warning/total counts from log entries.
fn compute_log_stats(logs: &[Value]) -> Value {
    let mut errors = 0u32;
    let mut warnings = 0u32;
    for log in logs {
        match log["level"].as_str() {
            Some("error") => errors += 1,
            Some("warn") => warnings += 1,
            _ => {}
        }
    }
    json!({
        "errors": errors,
        "warnings": warnings,
        "total": logs.len(),
    })
}

/// Returns `true` if the message is startup/shutdown boilerplate that should be
/// filtered from the diagnostics log — these are extremely repetitive on relays
/// that restart frequently and drown out the actually useful events.
fn is_lifecycle_noise(message: &str) -> bool {
    // Exact matches
    matches!(
        message,
        "Server ready"
            | "Goodbye"
            | "Shutting down..."
            | "Received SIGTERM"
            | "Received SIGINT"
            | "Tunnel relay mode enabled"
            | "Notifying tunnel devices of relay shutdown..."
    ) ||
    // Prefix matches
    message.starts_with("sctl v")          // "sctl v0.4.0 starting"
        || message.starts_with("Listening on ")    // "Listening on 0.0.0.0:8443"
        || message.starts_with("Device serial: ")  // "Device serial: RELAY-001"
        || message.starts_with("Journaling enabled") // "Journaling enabled, data_dir: ..."
        || message.starts_with("Seeded ") // "Seeded 8 connection sessions from journal"
}

/// Strip the tracing-subscriber prefix from a log message.
/// e.g. "2026-03-02T01:25:14.123Z  INFO sctl: Server ready" → "Server ready"
/// e.g. "2026-03-02T01:25:14.123Z  INFO sctl::tunnel::relay: device connected" → "device connected"
/// Also handles span prefixes: "...INFO tunnel_device{serial=X}: sctl::relay: msg" → "msg"
fn strip_tracing_prefix(s: &str) -> String {
    let mut result = s;

    // Step 1: strip "YYYY-MM-DDTHH:MM:SS.<frac>Z  LEVEL span{fields}: module: "
    if s.len() > 30 && s.as_bytes().get(4) == Some(&b'-') {
        if let Some(pos) = s[20..].find(": ") {
            result = &s[20 + pos + 2..];
        }
    }

    // Step 2: strip residual module path prefix (e.g. "sctl::tunnel::relay: ")
    // When a span is present, step 1 stops at the span's ": " leaving the module path.
    if let Some(pos) = result.find(": ") {
        if result[..pos].contains("::") {
            result = &result[pos + 2..];
        }
    }

    result.to_string()
}

/// Strip ANSI escape sequences from a string.
fn strip_ansi_escapes(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip ESC [ ... <letter> sequences
            if chars.peek() == Some(&'[') {
                chars.next(); // consume '['
                              // Consume until we hit a letter (the terminator)
                while let Some(&nc) = chars.peek() {
                    chars.next();
                    if nc.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// Parse a kB value field from /proc/self/status (e.g. VmRSS) and return bytes.
fn parse_proc_status_field(status: &str, field: &str) -> u64 {
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix(field) {
            let kb: u64 = rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            return kb * 1024;
        }
    }
    0
}

/// Parse a plain integer field from /proc/self/status (e.g. Threads).
fn parse_proc_status_field_u32(status: &str, field: &str) -> u32 {
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix(field) {
            return rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
        }
    }
    0
}
