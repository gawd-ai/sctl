//! OpenWrt platform helpers.
//!
//! OpenWrt's default logd is usually RAM-only. Persistent logs can be useful for
//! post-crash forensics, but on small flash devices they compete with config and
//! the sctl payload itself. sctl therefore leaves logd alone by default and only
//! applies persistent log capture when explicitly enabled in config.
//!
//! When enabled, startup ensures that:
//!
//! 1. `/root/log/` exists on the persistent overlay.
//! 2. logd writes to `/root/log/system.log` with a bounded ring.
//! 3. `/etc/crontabs/root` carries an hourly rotation line for that file.
//!
//! The steps are idempotent and best-effort. On non-OpenWrt hosts the entrypoint
//! is a no-op.
//!
//! Failure mode: best-effort. Any individual sub-step that fails (UCI not
//! installed, cron missing, permission denied, ...) logs a `warn!` and the
//! caller continues. We never refuse to start sctl because a self-heal step
//! failed.

use tokio::process::Command;
use tracing::{debug, info, warn};

const LOG_DIR: &str = "/root/log";
const LOG_FILE: &str = "/root/log/system.log";
const CRONTAB_PATH: &str = "/etc/crontabs/root";
const ROTATION_CRON_MARKER: &str = "# sctl-openwrt-log-rotation";
const MIN_LOG_SIZE_KB: u32 = 16;
const MAX_LOG_SIZE_KB: u32 = 2048;

/// Run all self-heal steps. No-op on non-OpenWrt hosts.
///
/// Best-effort: logs warnings and continues on any sub-step failure.
pub async fn ensure_persistent_logs(enabled: bool, size_kb: u32) {
    if !is_openwrt() {
        return;
    }
    if !enabled {
        debug!("platform/openwrt: persistent logs disabled");
        return;
    }

    let size_kb = size_kb.clamp(MIN_LOG_SIZE_KB, MAX_LOG_SIZE_KB);
    info!("platform/openwrt: ensuring persistent logs");

    if let Err(e) = ensure_log_dir().await {
        warn!("platform/openwrt: ensure_log_dir failed: {e}");
    }
    if let Err(e) = ensure_uci_logd_config(size_kb).await {
        warn!("platform/openwrt: ensure_uci_logd_config failed: {e}");
    }
    if let Err(e) = ensure_rotation_cron(size_kb).await {
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

async fn ensure_uci_logd_config(size_kb: u32) -> Result<(), String> {
    let size = size_kb.to_string();
    let current_file = uci_get("system.@system[0].log_file").await;
    let current_size = uci_get("system.@system[0].log_size").await;

    let needs_file = current_file.as_deref() != Some(LOG_FILE);
    let needs_size = current_size.as_deref() != Some(size.as_str());

    if !needs_file && !needs_size {
        return Ok(());
    }

    info!(
        "platform/openwrt: updating logd config (file: {} → {LOG_FILE}, size: {} → {size})",
        current_file.as_deref().unwrap_or("(unset)"),
        current_size.as_deref().unwrap_or("(unset)"),
    );

    if needs_file {
        uci_set("system.@system[0].log_file", LOG_FILE).await?;
    }
    if needs_size {
        uci_set("system.@system[0].log_size", &size).await?;
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

async fn ensure_rotation_cron(size_kb: u32) -> Result<(), String> {
    let existing = match tokio::fs::read_to_string(CRONTAB_PATH).await {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("read {CRONTAB_PATH}: {e}")),
    };

    let rotation_line = rotation_cron_line(size_kb);
    if existing.lines().any(|l| l == rotation_line) {
        return Ok(());
    }

    let next = remove_managed_rotation_lines(&existing);

    info!("platform/openwrt: appending log-rotation cron line to {CRONTAB_PATH}");

    let mut next = next;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(&rotation_line);
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

fn remove_managed_rotation_lines(existing: &str) -> String {
    existing
        .lines()
        .filter(|line| !line.contains(ROTATION_CRON_MARKER) && !line.contains(LOG_FILE))
        .collect::<Vec<_>>()
        .join("\n")
}

fn rotation_cron_line(size_kb: u32) -> String {
    let max_bytes = u64::from(size_kb) * 1024;
    format!(
        "0 * * * * f={LOG_FILE}; [ -s \"$f\" ] && [ \"$(wc -c < \"$f\")\" -gt {max_bytes} ] && {{ mv -f \"$f\" \"$f.1\"; kill -HUP $(pidof logd) 2>/dev/null; }} {ROTATION_CRON_MARKER}"
    )
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
