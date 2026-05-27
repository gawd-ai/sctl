//! OpenWrt platform self-heal.
//!
//! sctl runs on OpenWrt BPIs where the default logd is RAM-only (24h ring,
//! ~64 KB), so any post-crash forensic trail vanishes on reboot or memory
//! pressure. This module makes sctl ensure on startup that:
//!
//! 1. `/root/log/` exists (persistent overlay).
//! 2. logd is configured to write to `/root/log/system.log` with a 2 MB ring.
//! 3. `/etc/crontabs/root` carries an hourly rotation line for that file.
//!
//! All three steps are idempotent — safe to run on every restart. On
//! non-OpenWrt hosts the entrypoint is a no-op.
//!
//! Failure mode: best-effort. Any individual sub-step that fails (UCI not
//! installed, cron missing, permission denied, ...) logs a `warn!` and the
//! caller continues. We never refuse to start sctl because a self-heal step
//! failed.
//!
//! Replaces the operator-applied overlay drift that was put on the LiveBarn
//! BPI on 2026-05-20. Codifying it here means a fresh reflash automatically
//! recovers the persistent-log capability.

use tokio::process::Command;
use tracing::{info, warn};

const LOG_DIR: &str = "/root/log";
const LOG_FILE: &str = "/root/log/system.log";
const LOG_SIZE_KB: &str = "2048";
const CRONTAB_PATH: &str = "/etc/crontabs/root";
/// Hourly rotation: if `system.log` > 2 MB, move it to `.1` and HUP logd so
/// it reopens. Single line — keep this format if you ever edit it manually
/// on a device so `ensure_rotation_cron`'s grep stays a match.
const ROTATION_CRON_LINE: &str =
    "0 * * * * f=/root/log/system.log; [ -s \"$f\" ] && [ \"$(wc -c < \"$f\")\" -gt 2097152 ] && { mv -f \"$f\" \"$f.1\"; kill -HUP $(pidof logd) 2>/dev/null; }";

/// Run all self-heal steps. No-op on non-OpenWrt hosts.
///
/// Best-effort: logs warnings and continues on any sub-step failure.
pub async fn ensure_persistent_logs() {
    if !is_openwrt() {
        return;
    }
    info!("platform/openwrt: ensuring persistent logs");

    if let Err(e) = ensure_log_dir().await {
        warn!("platform/openwrt: ensure_log_dir failed: {e}");
    }
    if let Err(e) = ensure_uci_logd_config().await {
        warn!("platform/openwrt: ensure_uci_logd_config failed: {e}");
    }
    if let Err(e) = ensure_rotation_cron().await {
        warn!("platform/openwrt: ensure_rotation_cron failed: {e}");
    }
}

fn is_openwrt() -> bool {
    std::path::Path::new("/etc/openwrt_release").exists()
}

async fn ensure_log_dir() -> Result<(), String> {
    tokio::fs::create_dir_all(LOG_DIR)
        .await
        .map_err(|e| format!("create_dir_all {LOG_DIR}: {e}"))
}

async fn ensure_uci_logd_config() -> Result<(), String> {
    let current_file = uci_get("system.@system[0].log_file").await;
    let current_size = uci_get("system.@system[0].log_size").await;

    let needs_file = current_file.as_deref() != Some(LOG_FILE);
    let needs_size = current_size.as_deref() != Some(LOG_SIZE_KB);

    if !needs_file && !needs_size {
        return Ok(());
    }

    info!(
        "platform/openwrt: updating logd config (file: {} → {LOG_FILE}, size: {} → {LOG_SIZE_KB})",
        current_file.as_deref().unwrap_or("(unset)"),
        current_size.as_deref().unwrap_or("(unset)"),
    );

    if needs_file {
        uci_set("system.@system[0].log_file", LOG_FILE).await?;
    }
    if needs_size {
        uci_set("system.@system[0].log_size", LOG_SIZE_KB).await?;
    }

    let commit = Command::new("uci")
        .args(["commit", "system"])
        .output()
        .await
        .map_err(|e| format!("uci commit system: {e}"))?;
    if !commit.status.success() {
        return Err(format!(
            "uci commit system failed: {}",
            String::from_utf8_lossy(&commit.stderr)
        ));
    }

    let restart = Command::new("/etc/init.d/log")
        .arg("restart")
        .output()
        .await
        .map_err(|e| format!("logd restart: {e}"))?;
    if !restart.status.success() {
        warn!(
            "platform/openwrt: logd restart returned non-zero: {}",
            String::from_utf8_lossy(&restart.stderr).trim()
        );
    }
    Ok(())
}

async fn ensure_rotation_cron() -> Result<(), String> {
    let existing = match tokio::fs::read_to_string(CRONTAB_PATH).await {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("read {CRONTAB_PATH}: {e}")),
    };

    // Idempotency: any operator-added variant that references the log file
    // is treated as "already configured" — don't double up.
    if existing.lines().any(|l| l.contains("/root/log/system.log")) {
        return Ok(());
    }

    info!("platform/openwrt: appending log-rotation cron line to {CRONTAB_PATH}");

    let mut next = existing;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(ROTATION_CRON_LINE);
    next.push('\n');

    tokio::fs::write(CRONTAB_PATH, next.as_bytes())
        .await
        .map_err(|e| format!("write {CRONTAB_PATH}: {e}"))?;

    let restart = Command::new("/etc/init.d/cron")
        .arg("restart")
        .output()
        .await
        .map_err(|e| format!("cron restart: {e}"))?;
    if !restart.status.success() {
        warn!(
            "platform/openwrt: cron restart returned non-zero: {}",
            String::from_utf8_lossy(&restart.stderr).trim()
        );
    }
    Ok(())
}

async fn uci_get(key: &str) -> Option<String> {
    let out = Command::new("uci").args(["get", key]).output().await.ok()?;
    if !out.status.success() {
        return None;
    }
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

async fn uci_set(key: &str, value: &str) -> Result<(), String> {
    let out = Command::new("uci")
        .args(["set", &format!("{key}={value}")])
        .output()
        .await
        .map_err(|e| format!("uci set {key}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "uci set {key} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(())
}
