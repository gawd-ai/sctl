//! LTE watchdog — automatic modem recovery when tunnel is down.
//!
//! Correlates LTE signal state with tunnel health and takes escalating
//! recovery actions — from gentle re-registration to full USB power cycle —
//! without assuming anything about the radio environment.
//!
//! ## Safe-bands recovery
//!
//! The watchdog tracks which band config last sustained a stable tunnel
//! connection (5+ minutes). When the tunnel drops after a recent band change,
//! it can quickly revert to the known-working config before escalating to
//! heavier recovery actions.
//!
//! Recovery order when tunnel drops and signal is fresh:
//! 1. Pre-change revert (if band change within 3min)
//! 2. Safe-bands revert (if safe config differs from current)
//! 3. Standard escalation: L0 re-register → L1 airplane → L2 iface → L3 USB

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::{broadcast, watch, Mutex};
use tracing::{info, warn};

use crate::config::LteConfig;
use crate::lte::{BandChangeSource, LteState, ScanStatus};
use crate::modem::Modem;
use crate::state::{TunnelEventType, TunnelStats};

/// Watchdog tick interval.
const TICK_INTERVAL: Duration = Duration::from_secs(30);

/// How long the tunnel must be disconnected before the watchdog acts.
const DISCONNECT_GRACE: Duration = Duration::from_secs(60);

/// How long after a band change the pre-change revert is eligible (3 min).
const PRECHANGE_REVERT_WINDOW: Duration = Duration::from_secs(180);

/// How long the tunnel must be stable before promoting current bands to safe bands.
const SAFE_PROMOTE_THRESHOLD: Duration = Duration::from_secs(300);

/// Cooldowns per escalation level.
const COOLDOWNS: [Duration; 4] = [
    Duration::from_secs(60),  // Level 0: re-register (1 min)
    Duration::from_secs(120), // Level 1: airplane cycle (2 min)
    Duration::from_secs(120), // Level 2: iface restart (2 min)
    Duration::from_secs(300), // Level 3: USB power cycle (5 min)
];

/// Number of consecutive AT failures before skipping to interface/USB reset.
const AT_FAILURE_SKIP_THRESHOLD: u32 = 3;

/// Internal watchdog state.
struct WatchdogState {
    level: u8,
    last_action_at: Option<Instant>,
    tunnel_disconnect_since: Option<Instant>,
    consecutive_at_failures: u32,
    /// Whether we've already tried a fresh-signal re-register this disconnect episode.
    tried_fresh_reregister: bool,
    /// Whether we've already tried reverting to pre-change bands this disconnect episode.
    tried_prechange_revert: bool,
    /// Whether we've already tried reverting to safe bands this disconnect episode.
    tried_safe_revert: bool,
}

impl WatchdogState {
    fn new() -> Self {
        Self {
            level: 0,
            last_action_at: None,
            tunnel_disconnect_since: None,
            consecutive_at_failures: 0,
            tried_fresh_reregister: false,
            tried_prechange_revert: false,
            tried_safe_revert: false,
        }
    }

    fn reset(&mut self) {
        self.level = 0;
        self.last_action_at = None;
        self.tunnel_disconnect_since = None;
        self.consecutive_at_failures = 0;
        self.tried_fresh_reregister = false;
        self.tried_prechange_revert = false;
        self.tried_safe_revert = false;
    }

    fn cooldown_elapsed(&self) -> bool {
        let Some(last) = self.last_action_at else {
            return true;
        };
        let idx = (self.level as usize).min(COOLDOWNS.len() - 1);
        last.elapsed() >= COOLDOWNS[idx]
    }
}

/// EPS network registration status from AT+CEREG?.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationStatus {
    NotRegistered,  // 0
    RegisteredHome, // 1
    Searching,      // 2
    Denied,         // 3
    Unknown,        // 4
    RegisteredRoam, // 5
}

impl RegistrationStatus {
    pub fn is_registered(self) -> bool {
        matches!(self, Self::RegisteredHome | Self::RegisteredRoam)
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::NotRegistered => "not_registered",
            Self::RegisteredHome => "home",
            Self::Searching => "searching",
            Self::Denied => "denied",
            Self::Unknown => "unknown",
            Self::RegisteredRoam => "roaming",
        }
    }
}

/// Parse `AT+CEREG?` response into registration status.
///
/// Response format: `+CEREG: <n>,<stat>[,...]`
pub fn parse_cereg(response: &str) -> Result<RegistrationStatus, String> {
    let line = response
        .lines()
        .find(|l| l.contains("+CEREG:"))
        .ok_or_else(|| format!("no +CEREG in response: {}", response.trim()))?;

    let data = line
        .split(':')
        .nth(1)
        .ok_or("malformed +CEREG line")?
        .trim();

    // Format: <n>,<stat> — stat is the second field
    let stat_str = data
        .split(',')
        .nth(1)
        .ok_or("no stat field in +CEREG")?
        .trim();

    let stat: u8 = stat_str
        .parse()
        .map_err(|e| format!("bad CEREG stat: {e}"))?;

    Ok(match stat {
        0 => RegistrationStatus::NotRegistered,
        1 => RegistrationStatus::RegisteredHome,
        2 => RegistrationStatus::Searching,
        3 => RegistrationStatus::Denied,
        5 => RegistrationStatus::RegisteredRoam,
        _ => RegistrationStatus::Unknown,
    })
}

/// Read current band config from modem. Returns (bands, priority).
/// Only returns `None` if we can't read bands at all — bandpri is best-effort.
async fn read_current_bands(modem: &Modem) -> Option<(Vec<u16>, Option<u16>)> {
    let band_resp = modem.command("AT+QCFG=\"band\"").await.ok()?;
    let bands = crate::lte::parse_band_config(&band_resp);
    if bands.is_empty() {
        return None;
    }
    let priority = modem
        .command("AT+QCFG=\"bandpri\"")
        .await
        .ok()
        .and_then(|r| crate::lte::parse_bandpri(&r));
    Some((bands, priority))
}

/// Apply a band config via AT commands. Returns true on success.
/// Rejects empty band lists to prevent accidentally disabling all LTE bands.
async fn apply_band_config(modem: &Modem, bands: &[u16], priority: Option<u16>) -> bool {
    if bands.is_empty() {
        warn!("LTE watchdog: refusing to apply empty band config");
        return false;
    }
    let hex = crate::lte::bands_to_hex(bands);
    let cmd = format!("AT+QCFG=\"band\",0x260,{hex},0");
    if modem.command(&cmd).await.is_err() {
        return false;
    }
    if let Some(pri) = priority {
        let _ = modem.command(&format!("AT+QCFG=\"bandpri\",{pri}")).await;
    } else {
        let _ = modem.command("AT+QCFG=\"bandpri\",0").await;
    }
    true
}

/// Format bands as "B4,B12,B13" for logging.
fn fmt_bands(bands: &[u16]) -> String {
    bands
        .iter()
        .map(|b| format!("B{b}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Spawn the LTE watchdog task. Returns a `JoinHandle` for abort on shutdown.
#[allow(clippy::needless_pass_by_value)] // config is moved into spawned task
#[allow(clippy::too_many_lines)]
pub fn spawn_lte_watchdog(
    modem: Modem,
    modem_tx: watch::Sender<Modem>,
    lte_state: Arc<Mutex<LteState>>,
    tunnel_stats: Arc<TunnelStats>,
    session_events: broadcast::Sender<Value>,
    config: LteConfig,
    data_dir: String,
) -> tokio::task::JoinHandle<()> {
    let interface = config.interface.clone();
    let device_path = config.device.clone();

    tokio::spawn(async move {
        let mut modem = modem;
        info!("LTE watchdog started (interface: {interface})");

        let mut state = WatchdogState::new();
        let mut ticker = tokio::time::interval(TICK_INTERVAL);

        loop {
            ticker.tick().await;

            // Skip watchdog actions while a band scan is running or a manual
            // band change is in progress (band_action_until).
            // Safety valve: if a scan has been running > 30 min, force-clear it
            // so the watchdog can recover connectivity.
            {
                let mut lte = lte_state.lock().await;
                if let Some(ref scan) = lte.scan_status {
                    if scan.state == "running" {
                        let now_epoch = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs();
                        if now_epoch.saturating_sub(scan.started_at) > 30 * 60 {
                            warn!("LTE watchdog: scan stuck for >30min, force-clearing");
                            lte.scan_status = Some(ScanStatus {
                                state: "timeout".into(),
                                completed_at: Some(now_epoch),
                                ..scan.clone()
                            });
                        } else {
                            continue;
                        }
                    }
                }
                if let Some(until) = lte.band_action_until {
                    if Instant::now() < until {
                        continue;
                    }
                }
            }

            let tunnel_connected = tunnel_stats.connected.load(Ordering::Relaxed);

            // ── Tunnel connected path: track stability for safe-bands promotion ──
            if tunnel_connected {
                if state.tunnel_disconnect_since.is_some() {
                    info!("LTE watchdog: tunnel reconnected, resetting escalation");
                    state.reset();
                }

                // Safe-bands promotion: if tunnel has been stable for 5+ min,
                // promote current bands to safe_bands.
                //
                // Phase 1: Check if we need to snapshot bands (lock briefly)
                let needs_snapshot = {
                    let mut lte = lte_state.lock().await;
                    if lte.band_stable_since.is_none() {
                        lte.band_stable_since = Some(Instant::now());
                    }
                    lte.bands_at_connect.is_none()
                };

                // Phase 2: Read bands from modem WITHOUT holding the lock
                if needs_snapshot {
                    if let Some((bands, _)) = read_current_bands(&modem).await {
                        let mut lte = lte_state.lock().await;
                        if lte.bands_at_connect.is_none() {
                            lte.bands_at_connect = Some(bands);
                        }
                    }
                }

                // Phase 3: Check if promotion is needed (lock briefly)
                let promote_info = {
                    let lte = lte_state.lock().await;
                    match lte.band_stable_since {
                        Some(since) if since.elapsed() >= SAFE_PROMOTE_THRESHOLD => {
                            let needs_promote = match (&lte.safe_bands, &lte.bands_at_connect) {
                                (Some(safe), Some(current)) => safe.bands != *current,
                                (None, Some(_)) => true,
                                _ => false,
                            };
                            if needs_promote {
                                lte.bands_at_connect.clone()
                            } else {
                                None
                            }
                        }
                        _ => None,
                    }
                };

                // Phase 4: Do promotion (AT read outside lock, then lock briefly to write)
                if let Some(current_bands) = promote_info {
                    let priority = modem
                        .command("AT+QCFG=\"bandpri\"")
                        .await
                        .ok()
                        .and_then(|r| crate::lte::parse_bandpri(&r));
                    info!(
                        "LTE watchdog: promoting safe bands: {}",
                        fmt_bands(&current_bands)
                    );
                    let mut lte = lte_state.lock().await;
                    lte.promote_safe_bands(&data_dir, &current_bands, priority);
                    // Reset stability tracking — promotion done, start fresh
                    // so we re-snapshot if bands change later.
                    lte.band_stable_since = None;
                    lte.bands_at_connect = None;
                }
                continue;
            }

            // ── Tunnel disconnected path ──

            // Clear stability tracking on disconnect
            {
                let mut lte = lte_state.lock().await;
                if lte.band_stable_since.is_some() {
                    lte.band_stable_since = None;
                    lte.bands_at_connect = None;
                }
            }

            // Track disconnect start
            if state.tunnel_disconnect_since.is_none() {
                state.tunnel_disconnect_since = Some(Instant::now());
            }

            // Wait for grace period before acting
            let disconnect_duration = state
                .tunnel_disconnect_since
                .map_or(Duration::ZERO, |t| t.elapsed());
            if disconnect_duration < DISCONNECT_GRACE {
                continue;
            }

            // Check signal staleness — if no signal reading for >2min, modem may be stuck
            let signal_stale = {
                let lte = lte_state.lock().await;
                match &lte.signal {
                    Some(sig) => {
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs();
                        now.saturating_sub(sig.recorded_at) > 120
                    }
                    None => true,
                }
            };

            let disconnect_secs = disconnect_duration.as_secs();

            // ── Pre-change revert: if a band change happened within 3min, revert ──
            // Skip if the tunnel was already down before the band change — the user
            // likely changed bands trying to fix it, so reverting would undo their fix.
            if !signal_stale && !state.tried_prechange_revert {
                let revert_info = {
                    let lte = lte_state.lock().await;
                    match (&lte.last_band_change_at, &lte.pre_change_bands) {
                        (Some(changed_at), Some((bands, priority)))
                            if changed_at.elapsed() < PRECHANGE_REVERT_WINDOW
                                && state
                                    .tunnel_disconnect_since
                                    .is_some_and(|d| d > *changed_at) =>
                        {
                            Some((bands.clone(), *priority))
                        }
                        _ => None,
                    }
                };

                if let Some((revert_bands, revert_priority)) = revert_info {
                    state.tried_prechange_revert = true;

                    info!(
                        "LTE watchdog: reverting to pre-change bands: {}",
                        fmt_bands(&revert_bands)
                    );

                    if apply_band_config(&modem, &revert_bands, revert_priority).await {
                        // Record this as a watchdog change (doesn't overwrite pre_change_bands)
                        {
                            let mut lte = lte_state.lock().await;
                            lte.record_band_change(
                                BandChangeSource::Watchdog,
                                &[], // don't care about "from" for watchdog
                                None,
                                &revert_bands,
                            );
                        }

                        let detail = format!(
                            "level=0.5 action=prechange_revert bands={} disconnect={disconnect_secs}s",
                            fmt_bands(&revert_bands)
                        );
                        info!("LTE watchdog: {detail}");
                        tunnel_stats
                            .push_event(TunnelEventType::WatchdogAction, detail)
                            .await;
                        let _ = session_events.send(serde_json::json!({
                            "type": "lte.watchdog",
                            "level": 0,
                            "action": "prechange_revert",
                            "bands": revert_bands,
                            "disconnect_secs": disconnect_secs,
                        }));

                        // Wait for registration + tunnel recovery
                        tokio::time::sleep(Duration::from_secs(10)).await;
                        if verify_recovery(&interface, &tunnel_stats, Duration::from_secs(30)).await
                        {
                            info!("LTE watchdog: prechange_revert recovered tunnel");
                            state.reset();
                            continue;
                        }
                    }
                }
            }

            // ── Safe-bands revert: if safe bands exist and differ from current config ──
            if !signal_stale && !state.tried_safe_revert {
                let revert_info = {
                    let lte = lte_state.lock().await;
                    lte.safe_bands
                        .as_ref()
                        .map(|sb| (sb.bands.clone(), sb.priority_band))
                };

                if let Some((safe_bands, safe_priority)) = revert_info {
                    // Check if current config differs from safe config
                    let current = read_current_bands(&modem).await;
                    let differs = match &current {
                        Some((bands, _)) => *bands != safe_bands,
                        None => true,
                    };

                    if differs {
                        state.tried_safe_revert = true;

                        info!(
                            "LTE watchdog: reverting to safe bands: {}",
                            fmt_bands(&safe_bands)
                        );

                        if apply_band_config(&modem, &safe_bands, safe_priority).await {
                            {
                                let mut lte = lte_state.lock().await;
                                lte.record_band_change(
                                    BandChangeSource::Watchdog,
                                    &[],
                                    None,
                                    &safe_bands,
                                );
                            }

                            let detail = format!(
                                "level=0.5 action=safe_revert bands={} disconnect={disconnect_secs}s",
                                fmt_bands(&safe_bands)
                            );
                            info!("LTE watchdog: {detail}");
                            tunnel_stats
                                .push_event(TunnelEventType::WatchdogAction, detail)
                                .await;
                            let _ = session_events.send(serde_json::json!({
                                "type": "lte.watchdog",
                                "level": 0,
                                "action": "safe_revert",
                                "bands": safe_bands,
                                "disconnect_secs": disconnect_secs,
                            }));

                            tokio::time::sleep(Duration::from_secs(10)).await;
                            if verify_recovery(&interface, &tunnel_stats, Duration::from_secs(30))
                                .await
                            {
                                info!("LTE watchdog: safe_revert recovered tunnel");
                                state.reset();
                                continue;
                            }
                        }
                    }
                }
            }

            // ── Standard escalation (L0-L3) ──

            // Only escalate if signal is stale/missing — if signal is fresh,
            // the problem is likely not the modem (could be relay down, DNS, etc.)
            // Exception: allow one L0 re-register per disconnect episode after 2min
            if !signal_stale && state.level == 0 {
                if disconnect_duration < Duration::from_secs(120) || state.tried_fresh_reregister {
                    continue;
                }
                state.tried_fresh_reregister = true;
            }

            // Check cooldown
            if !state.cooldown_elapsed() {
                continue;
            }

            // If AT commands keep failing, skip directly to interface/USB reset
            let effective_level =
                if state.consecutive_at_failures >= AT_FAILURE_SKIP_THRESHOLD && state.level < 2 {
                    warn!(
                        "LTE watchdog: {} consecutive AT failures, skipping to level 2",
                        state.consecutive_at_failures
                    );
                    state.level = 2;
                    2
                } else {
                    state.level
                };

            // Check registration status for diagnostics
            let reg_status = match modem.command("AT+CEREG?").await {
                Ok(resp) => match parse_cereg(&resp) {
                    Ok(s) => {
                        state.consecutive_at_failures = 0;
                        Some(s)
                    }
                    Err(e) => {
                        warn!("LTE watchdog: CEREG parse error: {e}");
                        None
                    }
                },
                Err(e) => {
                    state.consecutive_at_failures += 1;
                    warn!(
                        "LTE watchdog: CEREG failed ({e}), AT failures: {}",
                        state.consecutive_at_failures
                    );
                    None
                }
            };

            let reg_str = reg_status.map_or("at_error", |s| s.as_str());

            // Execute recovery action
            let (action, new_modem): (&str, Option<Modem>) = match effective_level {
                0 => (action_reregister(&modem, &mut state).await, None),
                1 => (action_airplane_cycle(&modem, &mut state).await, None),
                2 => (action_restart_interface(&interface).await, None),
                _ => action_usb_power_cycle(&device_path).await,
            };

            // Handle modem replacement after USB cycle
            if let Some(new_m) = new_modem {
                let _ = modem_tx.send(new_m.clone());
                modem = new_m;
                info!("LTE watchdog: modem handle refreshed via watch channel");
            }

            let detail = format!(
                "level={effective_level} action={action} reg={reg_str} \
                 disconnect={disconnect_secs}s signal_stale={signal_stale}"
            );

            info!("LTE watchdog: {detail}");

            tunnel_stats
                .push_event(TunnelEventType::WatchdogAction, detail.clone())
                .await;

            let _ = session_events.send(serde_json::json!({
                "type": "lte.watchdog",
                "level": effective_level,
                "action": action,
                "registration": reg_str,
                "disconnect_secs": disconnect_secs,
                "signal_stale": signal_stale,
            }));

            // Implicit interface nudge after radio recovery (L0/L1):
            // modem may be registered but QMI data session didn't restart
            if effective_level <= 1 {
                tokio::time::sleep(Duration::from_secs(15)).await;
                if !interface_has_ipv4(&interface) {
                    info!("LTE watchdog: registered but no IP, nudging {interface}");
                    action_restart_interface(&interface).await;
                }
            }

            // Post-action verification: actively poll for recovery before escalating
            let verify_timeout = match effective_level {
                0 | 2 => Duration::from_secs(30),
                1 => Duration::from_secs(45),
                _ => Duration::from_secs(60),
            };

            if verify_recovery(&interface, &tunnel_stats, verify_timeout).await {
                info!("LTE watchdog: recovery verified after {action}");
                state.reset();
                continue;
            }

            state.last_action_at = Some(Instant::now());
            state.level = (effective_level + 1).min(3);
        }
    })
}

/// Level 0: Force network re-registration via AT+COPS=0.
async fn action_reregister(modem: &Modem, state: &mut WatchdogState) -> &'static str {
    match modem.command("AT+COPS=0").await {
        Ok(_) => {
            state.consecutive_at_failures = 0;
            "reregister"
        }
        Err(e) => {
            state.consecutive_at_failures += 1;
            warn!("LTE watchdog: AT+COPS=0 failed: {e}");
            "reregister_failed"
        }
    }
}

/// Level 1: Airplane mode cycle — AT+CFUN=0 → 5s → AT+CFUN=1.
async fn action_airplane_cycle(modem: &Modem, state: &mut WatchdogState) -> &'static str {
    match modem.command("AT+CFUN=0").await {
        Ok(_) => {
            state.consecutive_at_failures = 0;
        }
        Err(e) => {
            state.consecutive_at_failures += 1;
            warn!("LTE watchdog: AT+CFUN=0 failed: {e}");
            return "airplane_cycle_failed";
        }
    }

    tokio::time::sleep(Duration::from_secs(5)).await;

    match modem
        .command_with_timeout("AT+CFUN=1", Duration::from_secs(15))
        .await
    {
        Ok(_) => {
            state.consecutive_at_failures = 0;
            "airplane_cycle"
        }
        Err(e) => {
            state.consecutive_at_failures += 1;
            warn!("LTE watchdog: AT+CFUN=1 failed: {e}");
            "airplane_cycle_partial"
        }
    }
}

/// Level 2: Restart the network interface via `ip link set down/up`.
async fn action_restart_interface(interface: &str) -> &'static str {
    let down = tokio::process::Command::new("ip")
        .args(["link", "set", interface, "down"])
        .output()
        .await;

    if let Err(e) = &down {
        warn!("LTE watchdog: ip link set {interface} down failed: {e}");
        return "iface_restart_failed";
    }

    tokio::time::sleep(Duration::from_secs(2)).await;

    let up = tokio::process::Command::new("ip")
        .args(["link", "set", interface, "up"])
        .output()
        .await;

    if let Err(e) = &up {
        warn!("LTE watchdog: ip link set {interface} up failed: {e}");
        return "iface_restart_partial";
    }

    "iface_restart"
}

/// Level 3: USB modem power cycle — toggle sysfs `authorized` for the Quectel device.
/// Returns the action string and optionally a newly opened modem handle.
async fn action_usb_power_cycle(device_path: &str) -> (&'static str, Option<Modem>) {
    let Some(auth_path) = find_quectel_usb_auth().await else {
        warn!("LTE watchdog: Quectel USB device not found in sysfs");
        return ("usb_cycle_no_device", None);
    };

    info!("LTE watchdog: power cycling USB device at {auth_path}");

    // Deauthorize (power off)
    if let Err(e) = tokio::fs::write(&auth_path, "0").await {
        warn!("LTE watchdog: failed to write 0 to {auth_path}: {e}");
        return ("usb_cycle_failed", None);
    }

    tokio::time::sleep(Duration::from_secs(3)).await;

    // Reauthorize (power on)
    if let Err(e) = tokio::fs::write(&auth_path, "1").await {
        warn!("LTE watchdog: failed to write 1 to {auth_path}: {e}");
        return ("usb_cycle_partial", None);
    }

    // Wait for device to re-enumerate on USB (poll every 2s, up to 30s)
    let mut new_modem = None;
    for i in 0..15 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if tokio::fs::metadata(device_path).await.is_ok() {
            info!(
                "LTE watchdog: {device_path} appeared after {}s, settling...",
                (i + 1) * 2
            );
            tokio::time::sleep(Duration::from_secs(3)).await;
            match Modem::open(device_path) {
                Ok(m) => {
                    info!("LTE watchdog: modem re-opened at {device_path}");
                    new_modem = Some(m);
                }
                Err(e) => {
                    warn!("LTE watchdog: failed to re-open modem at {device_path}: {e}");
                }
            }
            break;
        }
    }

    if new_modem.is_none() {
        warn!("LTE watchdog: {device_path} did not re-appear after USB cycle");
    }

    ("usb_cycle", new_modem)
}

/// Find the sysfs `authorized` path for the Quectel USB device (vendor 2c7c).
async fn find_quectel_usb_auth() -> Option<String> {
    let output = tokio::process::Command::new("sh")
        .args([
            "-c",
            "for d in /sys/bus/usb/devices/*/idVendor; do \
                if [ \"$(cat \"$d\" 2>/dev/null)\" = \"2c7c\" ]; then \
                    echo \"$(dirname \"$d\")/authorized\"; \
                    break; \
                fi; \
            done",
        ])
        .output()
        .await
        .ok()?;

    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        None
    } else {
        Some(path)
    }
}

/// Check if a network interface has an IPv4 address assigned.
fn interface_has_ipv4(iface: &str) -> bool {
    // SAFETY: getifaddrs/freeifaddrs is a well-defined POSIX API.
    // We free the list before returning and don't hold pointers past that.
    unsafe {
        let mut ifaddrs: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&raw mut ifaddrs) != 0 {
            return false;
        }
        let mut current = ifaddrs;
        let mut found = false;
        while !current.is_null() {
            let ifa = &*current;
            if !ifa.ifa_name.is_null() && !ifa.ifa_addr.is_null() {
                let name = std::ffi::CStr::from_ptr(ifa.ifa_name);
                if let Ok(name_str) = name.to_str() {
                    if name_str == iface && i32::from((*ifa.ifa_addr).sa_family) == libc::AF_INET {
                        found = true;
                        break;
                    }
                }
            }
            current = ifa.ifa_next;
        }
        libc::freeifaddrs(ifaddrs);
        found
    }
}

/// Poll for recovery indicators after a watchdog action.
/// Returns `true` if interface has IPv4 AND tunnel reconnected within timeout.
async fn verify_recovery(interface: &str, tunnel_stats: &TunnelStats, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let has_ip = interface_has_ipv4(interface);
        let connected = tunnel_stats.connected.load(Ordering::Relaxed);
        if has_ip && connected {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_cereg_home() {
        let resp = "+CEREG: 0,1\r\nOK";
        assert_eq!(
            parse_cereg(resp).unwrap(),
            RegistrationStatus::RegisteredHome
        );
    }

    #[test]
    fn test_parse_cereg_roaming() {
        let resp = "+CEREG: 0,5\r\nOK";
        assert_eq!(
            parse_cereg(resp).unwrap(),
            RegistrationStatus::RegisteredRoam
        );
    }

    #[test]
    fn test_parse_cereg_searching() {
        let resp = "+CEREG: 0,2\r\nOK";
        assert_eq!(parse_cereg(resp).unwrap(), RegistrationStatus::Searching);
    }

    #[test]
    fn test_parse_cereg_denied() {
        let resp = "+CEREG: 0,3\r\nOK";
        assert_eq!(parse_cereg(resp).unwrap(), RegistrationStatus::Denied);
    }

    #[test]
    fn test_parse_cereg_not_registered() {
        let resp = "+CEREG: 0,0\r\nOK";
        assert_eq!(
            parse_cereg(resp).unwrap(),
            RegistrationStatus::NotRegistered
        );
    }

    #[test]
    fn test_parse_cereg_unknown_stat() {
        let resp = "+CEREG: 0,4\r\nOK";
        assert_eq!(parse_cereg(resp).unwrap(), RegistrationStatus::Unknown);
    }

    #[test]
    fn test_parse_cereg_extended_format() {
        // Some modems return extended info: +CEREG: <n>,<stat>,<tac>,<ci>,<AcT>
        let resp = "+CEREG: 2,1,\"A1B2\",\"0123ABCD\",7\r\nOK";
        assert_eq!(
            parse_cereg(resp).unwrap(),
            RegistrationStatus::RegisteredHome
        );
    }

    #[test]
    fn test_parse_cereg_no_response() {
        assert!(parse_cereg("ERROR\r\n").is_err());
    }

    #[test]
    fn test_parse_cereg_malformed() {
        assert!(parse_cereg("+CEREG: \r\nOK").is_err());
    }

    #[test]
    fn test_registration_status_is_registered() {
        assert!(RegistrationStatus::RegisteredHome.is_registered());
        assert!(RegistrationStatus::RegisteredRoam.is_registered());
        assert!(!RegistrationStatus::Searching.is_registered());
        assert!(!RegistrationStatus::Denied.is_registered());
        assert!(!RegistrationStatus::NotRegistered.is_registered());
        assert!(!RegistrationStatus::Unknown.is_registered());
    }

    #[test]
    fn test_fmt_bands() {
        assert_eq!(fmt_bands(&[4, 12, 13]), "B4,B12,B13");
        assert_eq!(fmt_bands(&[1]), "B1");
        assert_eq!(fmt_bands(&[]), "");
    }
}
