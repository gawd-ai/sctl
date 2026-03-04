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

/// Maximum L3 (USB cycle) attempts before entering dormant mode.
const MAX_L3_ATTEMPTS: u32 = 3;

/// Maximum L3 cooldown after exponential backoff (30 min).
const MAX_L3_COOLDOWN: Duration = Duration::from_secs(1800);

/// Dormant mode tick interval (15 min).
const DORMANT_TICK_INTERVAL: Duration = Duration::from_secs(900);

/// How long the modem can be stuck in CEREG=Searching before allowing USB cycle (10 min).
const STUCK_SEARCHING_TIMEOUT: Duration = Duration::from_secs(600);

/// Number of consecutive AT failures before skipping to interface/USB reset.
const AT_FAILURE_SKIP_THRESHOLD: u32 = 3;

/// Internal watchdog state.
#[allow(clippy::struct_excessive_bools)]
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
    /// Whether we've already tried an interface restart for NOCONN this disconnect episode.
    tried_noconn_fix: bool,
    /// Number of L3 (USB cycle) attempts this disconnect episode.
    l3_attempts: u32,
    /// Whether watchdog has entered dormant mode (all escalation exhausted).
    dormant: bool,
    /// Whether internet was reachable on last check (relay-wait mode).
    internet_reachable: bool,
    /// When internet reachability was last confirmed.
    internet_reachable_since: Option<Instant>,
    /// When the modem first entered CEREG=Searching (for stuck-searching detection).
    searching_since: Option<Instant>,
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
            tried_noconn_fix: false,
            l3_attempts: 0,
            dormant: false,
            internet_reachable: false,
            internet_reachable_since: None,
            searching_since: None,
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
        self.tried_noconn_fix = false;
        self.l3_attempts = 0;
        self.dormant = false;
        self.internet_reachable = false;
        self.internet_reachable_since = None;
        self.searching_since = None;
    }

    fn cooldown_elapsed(&self) -> bool {
        let Some(last) = self.last_action_at else {
            return true;
        };
        if self.dormant {
            return last.elapsed() >= DORMANT_TICK_INTERVAL;
        }
        // L3 uses exponential backoff: 300s, 600s, 1200s, capped at 1800s
        if self.level >= 3 && self.l3_attempts > 0 {
            let backoff =
                Duration::from_secs(300 * 2u64.pow(self.l3_attempts.min(3))).min(MAX_L3_COOLDOWN);
            return last.elapsed() >= backoff;
        }
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

/// Apply a band config via AT commands with COPS re-registration and data path verification.
/// Returns true if bands were applied AND data connectivity (IPv4) was confirmed.
/// Rejects empty band lists to prevent accidentally disabling all LTE bands.
/// Skips destructive COPS commands if tunnel reconnects mid-apply.
async fn apply_band_config(
    modem: &Modem,
    bands: &[u16],
    priority: Option<u16>,
    interface: &str,
    tunnel_stats: &TunnelStats,
) -> bool {
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

    // Skip destructive COPS re-registration if tunnel reconnected
    if tunnel_stats.connected.load(Ordering::Relaxed) {
        info!("apply_band_config: tunnel connected, skipping re-registration (bands set)");
        return false;
    }

    // Force re-registration on new bands
    let _ = modem.command("AT+COPS=2").await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let _ = modem.command("AT+COPS=0").await;

    // Wait for registration (up to 30s), abort if tunnel reconnects
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        if tunnel_stats.connected.load(Ordering::Relaxed) {
            info!("apply_band_config: tunnel reconnected during registration wait");
            return false;
        }
        if let Ok(resp) = modem.command("AT+CEREG?").await {
            if let Ok(status) = parse_cereg(&resp) {
                if status.is_registered() {
                    // Registered — check for IPv4
                    for _ in 0..5 {
                        if interface_has_ipv4(interface) {
                            return true;
                        }
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                    // No IPv4 — nudge interface
                    action_restart_interface(interface, false, None).await;
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    return interface_has_ipv4(interface);
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    false
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
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub fn spawn_lte_watchdog(
    modem: Modem,
    modem_tx: watch::Sender<Modem>,
    lte_state: Arc<Mutex<LteState>>,
    tunnel_stats: Arc<TunnelStats>,
    session_events: broadcast::Sender<Value>,
    config: LteConfig,
    data_dir: String,
    tunnel_url: Option<String>,
) -> tokio::task::JoinHandle<()> {
    let interface = config.interface.clone();
    let device_path = config.device.clone();
    let reachability_host = config.reachability_host.clone();
    let interface_restart_cmd = config.interface_restart_cmd.clone();
    let openwrt = is_openwrt();

    tokio::spawn(async move {
        let mut modem = modem;
        // Determine reachability target: config override > relay host > 8.8.8.8
        let reachability_target = reachability_host
            .or_else(|| tunnel_url.as_deref().and_then(extract_relay_host))
            .unwrap_or_else(|| "8.8.8.8".to_string());
        info!(
            "LTE watchdog started (interface: {interface}, reachability: {reachability_target}, \
             openwrt: {openwrt})"
        );

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
                    let was_dormant = state.dormant;
                    let saved_l3_attempts = state.l3_attempts;
                    info!("LTE watchdog: tunnel reconnected, resetting escalation");
                    state.reset();
                    if was_dormant {
                        state.l3_attempts = saved_l3_attempts;
                    }

                    // Check for exhaustion report to emit on reconnect
                    if was_dormant || check_exhaustion_file(&data_dir) {
                        emit_watchdog_report(&data_dir, &session_events);
                    }
                }

                // Safe-bands promotion: if tunnel has been stable for 5+ min,
                // promote current bands to safe_bands.
                // Uses cached band config from the LTE poller's last reading
                // to avoid running AT commands while the tunnel is connected.
                {
                    let mut lte = lte_state.lock().await;
                    if lte.band_stable_since.is_none() {
                        lte.band_stable_since = Some(Instant::now());
                        // Snapshot bands from cached signal data (last poller reading)
                        if lte.bands_at_connect.is_none() {
                            if let Some(ref sig) = lte.signal {
                                if let Some(ref bc) = sig.band_config {
                                    lte.bands_at_connect = Some(bc.enabled_bands.clone());
                                }
                            }
                        }
                    }

                    if let Some(since) = lte.band_stable_since {
                        if since.elapsed() >= SAFE_PROMOTE_THRESHOLD {
                            let needs_promote = match (&lte.safe_bands, &lte.bands_at_connect) {
                                (Some(safe), Some(current)) => safe.bands != *current,
                                (None, Some(_)) => true,
                                _ => false,
                            };
                            if needs_promote {
                                if let Some(ref current_bands) = lte.bands_at_connect.clone() {
                                    // Use cached priority and RSRP from signal data
                                    let priority = lte
                                        .signal
                                        .as_ref()
                                        .and_then(|s| s.band_config.as_ref())
                                        .and_then(|bc| bc.priority_band);
                                    let rsrp = lte.signal.as_ref().and_then(|s| s.rsrp);
                                    info!(
                                        "LTE watchdog: promoting safe bands: {} (RSRP: {:?})",
                                        fmt_bands(current_bands),
                                        rsrp
                                    );
                                    lte.promote_safe_bands(
                                        &data_dir,
                                        current_bands,
                                        priority,
                                        rsrp,
                                    );
                                }
                            }
                            // Reset stability tracking — promotion done or not needed
                            lte.band_stable_since = None;
                            lte.bands_at_connect = None;
                        }
                    }
                }
                continue;
            }

            // ── Tunnel disconnected path ──

            // Clear stability tracking on disconnect
            {
                let mut lte = lte_state.lock().await;
                lte.band_stable_since = None;
                lte.bands_at_connect = None;
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

            // Check modem responsiveness with a lightweight AT command.
            // The tunnel is already down, so AT commands can't make things worse.
            // This replaces the old signal_stale check which depended on the
            // background poller (now disabled while tunnel is connected).
            let modem_responsive = modem.command("AT").await.is_ok();

            let disconnect_secs = disconnect_duration.as_secs();

            // ── Pre-change revert: if a band change happened within 3min, revert ──
            // Skip if the tunnel was already down before the band change — the user
            // likely changed bands trying to fix it, so reverting would undo their fix.
            if modem_responsive && !state.tried_prechange_revert {
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

                    if apply_band_config(
                        &modem,
                        &revert_bands,
                        revert_priority,
                        &interface,
                        &tunnel_stats,
                    )
                    .await
                    {
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
            if modem_responsive && !state.tried_safe_revert {
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

                        if apply_band_config(
                            &modem,
                            &safe_bands,
                            safe_priority,
                            &interface,
                            &tunnel_stats,
                        )
                        .await
                        {
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

            // ── NOCONN fix: registered but no data bearer ──
            // Interface restart (ifdown/ifup) re-establishes the QMI data session.
            // Don't wait for signal staleness — NOCONN means the radio is fine but
            // the data path is broken.
            if !state.tried_noconn_fix && disconnect_duration > Duration::from_secs(90) {
                let is_noconn = {
                    let lte = lte_state.lock().await;
                    lte.signal
                        .as_ref()
                        .and_then(|s| s.connection_state.as_deref())
                        == Some("NOCONN")
                };

                if is_noconn {
                    info!("LTE watchdog: NOCONN — restarting interface for QMI data session");
                    action_restart_interface(&interface, openwrt, interface_restart_cmd.as_deref())
                        .await;
                    state.tried_noconn_fix = true;

                    let detail = format!("action=noconn_fix disconnect={disconnect_secs}s");
                    info!("LTE watchdog: {detail}");
                    tunnel_stats
                        .push_event(TunnelEventType::WatchdogAction, detail)
                        .await;
                    let _ = session_events.send(serde_json::json!({
                        "type": "lte.watchdog",
                        "level": 0,
                        "action": "noconn_fix",
                        "disconnect_secs": disconnect_secs,
                    }));

                    // Verify recovery
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    if verify_recovery(&interface, &tunnel_stats, Duration::from_secs(30)).await {
                        info!("LTE watchdog: noconn_fix recovered tunnel");
                        state.reset();
                        continue;
                    }
                    // If interface restart didn't fix it, fall through to standard escalation
                }
            }

            // ── Reachability check: is the problem the modem or the relay? ──
            // Before escalating past L0, check if we can reach the internet.
            // If internet works but tunnel is down, the modem is fine — skip escalation.
            if modem_responsive && interface_has_ipv4(&interface) && state.level > 0 {
                let reachable = check_reachability(&reachability_target).await;
                if reachable {
                    if !state.internet_reachable {
                        info!(
                            "LTE watchdog: internet reachable ({reachability_target}), \
                             problem is relay/app — pausing modem escalation"
                        );
                        state.internet_reachable = true;
                        state.internet_reachable_since = Some(Instant::now());
                    }
                    // Internet works — don't cycle the modem, just wait
                    continue;
                }
                // Internet not reachable — clear the flag and proceed with escalation
                if state.internet_reachable {
                    info!("LTE watchdog: internet no longer reachable, resuming escalation");
                    state.internet_reachable = false;
                    state.internet_reachable_since = None;
                }
            }

            // ── Standard escalation (L0-L3) ──

            // Skip L2+ (interface/USB actions) while tunnel client is mid-reconnect
            // and we still have IP connectivity — disrupting the modem would kill
            // the reconnection attempt.
            if state.level >= 2
                && tunnel_stats.reconnecting.load(Ordering::Relaxed)
                && interface_has_ipv4(&interface)
            {
                info!(
                    "LTE watchdog: tunnel reconnecting with IP, skipping L{} action",
                    state.level
                );
                continue;
            }

            // Only escalate if signal is stale/missing — if signal is fresh,
            // the problem is likely not the modem (could be relay down, DNS, etc.)
            // Exception: allow one L0 re-register per disconnect episode after 2min
            if modem_responsive && state.level == 0 {
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

            // Skip L3 (USB cycle) when modem is actively searching for a cell,
            // unless it's been stuck searching for too long (hardware fault).
            if effective_level >= 3 {
                if let Some(RegistrationStatus::Searching) = reg_status {
                    let searching_elapsed = state
                        .searching_since
                        .get_or_insert(Instant::now())
                        .elapsed();
                    if searching_elapsed < STUCK_SEARCHING_TIMEOUT {
                        let detail = format!(
                            "level=3 action=skip_searching reg=searching \
                             disconnect={disconnect_secs}s modem_responsive={modem_responsive} \
                             searching_secs={}",
                            searching_elapsed.as_secs()
                        );
                        info!(
                            "LTE watchdog: CEREG=Searching, skipping USB cycle (modem is scanning)"
                        );
                        tunnel_stats
                            .push_event(TunnelEventType::WatchdogAction, detail)
                            .await;
                        let _ = session_events.send(serde_json::json!({
                            "type": "lte.watchdog",
                            "level": 3,
                            "action": "skip_searching",
                            "registration": "searching",
                            "disconnect_secs": disconnect_secs,
                            "modem_responsive": modem_responsive,
                            "searching_secs": searching_elapsed.as_secs(),
                        }));
                        state.last_action_at = Some(Instant::now());
                        continue;
                    }
                    info!(
                        "LTE watchdog: CEREG=Searching for {}s, allowing USB cycle",
                        searching_elapsed.as_secs()
                    );
                    state.searching_since = None;
                } else {
                    state.searching_since = None;
                }
            }

            // Execute recovery action
            let (action, new_modem): (&str, Option<Modem>) = match effective_level {
                0 => (action_reregister(&modem, &mut state).await, None),
                1 => (action_airplane_cycle(&modem, &mut state).await, None),
                2 => (
                    action_restart_interface(&interface, openwrt, interface_restart_cmd.as_deref())
                        .await,
                    None,
                ),
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
                 disconnect={disconnect_secs}s modem_responsive={modem_responsive}"
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
                "modem_responsive": modem_responsive,
            }));

            // Implicit interface nudge after radio recovery (L0/L1):
            // modem may be registered but QMI data session didn't restart
            if effective_level <= 1 {
                tokio::time::sleep(Duration::from_secs(15)).await;
                if !interface_has_ipv4(&interface) {
                    info!("LTE watchdog: registered but no IP, nudging {interface}");
                    action_restart_interface(&interface, openwrt, interface_restart_cmd.as_deref())
                        .await;
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

            // Track L3 attempts for exhaustion detection
            if effective_level >= 3 {
                state.l3_attempts += 1;
                if state.l3_attempts >= MAX_L3_ATTEMPTS {
                    warn!(
                        "LTE watchdog: EXHAUSTED — {} USB cycles failed, entering dormant mode \
                         (check every {}s)",
                        state.l3_attempts,
                        DORMANT_TICK_INTERVAL.as_secs()
                    );
                    state.dormant = true;

                    let detail = format!(
                        "WATCHDOG_EXHAUSTED l3_attempts={} disconnect={disconnect_secs}s \
                         modem_responsive={modem_responsive}",
                        state.l3_attempts
                    );
                    tunnel_stats
                        .push_event(TunnelEventType::WatchdogAction, detail)
                        .await;
                    let _ = session_events.send(serde_json::json!({
                        "type": "lte.watchdog_exhausted",
                        "l3_attempts": state.l3_attempts,
                        "disconnect_secs": disconnect_secs,
                        "modem_responsive": modem_responsive,
                    }));

                    // Persist exhaustion state for post-mortem
                    persist_exhaustion_state(
                        &data_dir,
                        state.l3_attempts,
                        disconnect_secs,
                        modem_responsive,
                        reg_str,
                    );
                }
            } else {
                state.level = (effective_level + 1).min(3);
            }
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

/// Level 2: Restart the network interface.
///
/// Uses the appropriate method based on the platform:
/// - Custom command: if `interface_restart_cmd` is configured
/// - OpenWrt: `ifdown {interface} && sleep 2 && ifup {interface}` (netifd manages QMI bearer)
/// - Generic Linux: `ip link set down/up` (only toggles kernel interface)
pub(crate) async fn action_restart_interface(
    interface: &str,
    openwrt: bool,
    custom_cmd: Option<&str>,
) -> &'static str {
    if let Some(cmd) = custom_cmd {
        info!("LTE watchdog: restarting interface via custom command");
        let result = tokio::process::Command::new("sh")
            .args(["-c", cmd])
            .output()
            .await;
        match result {
            Ok(output) if output.status.success() => "iface_restart_custom",
            Ok(output) => {
                warn!(
                    "LTE watchdog: custom restart command failed (exit {}): {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                );
                "iface_restart_custom_failed"
            }
            Err(e) => {
                warn!("LTE watchdog: custom restart command error: {e}");
                "iface_restart_custom_failed"
            }
        }
    } else if openwrt {
        // OpenWrt: ifdown/ifup properly tears down and re-establishes the QMI data bearer
        info!("LTE watchdog: restarting {interface} via netifd (ifdown/ifup)");
        let down = tokio::process::Command::new("ifdown")
            .arg(interface)
            .output()
            .await;
        if let Err(e) = &down {
            warn!("LTE watchdog: ifdown {interface} failed: {e}");
            return "iface_restart_failed";
        }

        tokio::time::sleep(Duration::from_secs(2)).await;

        let up = tokio::process::Command::new("ifup")
            .arg(interface)
            .output()
            .await;
        if let Err(e) = &up {
            warn!("LTE watchdog: ifup {interface} failed: {e}");
            return "iface_restart_partial";
        }
        "iface_restart_netifd"
    } else {
        // Generic Linux: ip link set (only toggles kernel interface, no QMI)
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

    // Wait for device to re-enumerate on USB (poll every 2s, up to 30s).
    // After power cycle, ttyUSB numbering may shift — auto-detect the port.
    let mut new_modem = None;
    for i in 0..15 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let actual_path = crate::modem::detect_quectel_at_port(device_path);
        if tokio::fs::metadata(&actual_path).await.is_ok() {
            info!(
                "LTE watchdog: {} appeared after {}s, settling...",
                actual_path,
                (i + 1) * 2
            );
            tokio::time::sleep(Duration::from_secs(3)).await;
            match Modem::open(&actual_path) {
                Ok(m) => {
                    info!("LTE watchdog: modem re-opened at {actual_path}");
                    new_modem = Some(m);
                }
                Err(e) => {
                    warn!("LTE watchdog: failed to re-open modem at {actual_path}: {e}");
                }
            }
            break;
        }
    }

    if new_modem.is_none() {
        warn!("LTE watchdog: modem did not re-appear after USB cycle");
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
pub(crate) fn interface_has_ipv4(iface: &str) -> bool {
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

/// Extract the host/IP from a tunnel relay URL for reachability checks.
/// E.g. `wss://relay.example.com/api/tunnel/register` → `relay.example.com`
fn extract_relay_host(url: &str) -> Option<String> {
    // Strip scheme
    let after_scheme = url
        .strip_prefix("wss://")
        .or_else(|| url.strip_prefix("ws://"))?;
    // Take host (before first '/' or ':')
    let host = after_scheme.split('/').next()?.split(':').next()?;
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

/// Check internet reachability by pinging a target host.
/// Uses ICMP ping with a 3s timeout.
async fn check_reachability(target: &str) -> bool {
    let result = tokio::process::Command::new("ping")
        .args(["-c", "1", "-W", "3", target])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await;
    matches!(result, Ok(status) if status.success())
}

/// Detect if running on OpenWrt (check for /etc/openwrt_release).
fn is_openwrt() -> bool {
    std::path::Path::new("/etc/openwrt_release").exists()
}

/// Persist watchdog exhaustion state to disk for post-mortem diagnostics.
fn persist_exhaustion_state(
    data_dir: &str,
    l3_attempts: u32,
    disconnect_secs: u64,
    modem_responsive: bool,
    registration: &str,
) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let report = serde_json::json!({
        "timestamp": now,
        "l3_attempts": l3_attempts,
        "disconnect_secs": disconnect_secs,
        "modem_responsive": modem_responsive,
        "registration": registration,
        "action": "dormant",
    });
    let path = std::path::Path::new(data_dir).join("watchdog_exhausted.json");
    let tmp = path.with_extension("json.tmp");
    if let Ok(json) = serde_json::to_string_pretty(&report) {
        if let Err(e) = std::fs::write(&tmp, &json) {
            warn!("Failed to write watchdog_exhausted.json: {e}");
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, &path) {
            warn!("Failed to rename watchdog_exhausted.json: {e}");
        }
    }
}

/// Check if a watchdog exhaustion file exists.
fn check_exhaustion_file(data_dir: &str) -> bool {
    std::path::Path::new(data_dir)
        .join("watchdog_exhausted.json")
        .exists()
}

/// Emit a watchdog report event on reconnect and clean up the exhaustion file.
fn emit_watchdog_report(data_dir: &str, session_events: &broadcast::Sender<Value>) {
    let path = std::path::Path::new(data_dir).join("watchdog_exhausted.json");
    match std::fs::read_to_string(&path) {
        Ok(contents) => {
            if let Ok(report) = serde_json::from_str::<Value>(&contents) {
                info!("LTE watchdog: emitting post-exhaustion report");
                let mut event = report;
                event["type"] = serde_json::json!("lte.watchdog_report");
                let _ = session_events.send(event);
            }
            if let Err(e) = std::fs::remove_file(&path) {
                warn!("Failed to remove watchdog_exhausted.json: {e}");
            }
        }
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            warn!("Failed to read watchdog_exhausted.json: {e}");
        }
        _ => {}
    }
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

    #[test]
    fn test_extract_relay_host_wss() {
        assert_eq!(
            extract_relay_host("wss://relay.example.com/api/tunnel/register"),
            Some("relay.example.com".to_string())
        );
    }

    #[test]
    fn test_extract_relay_host_ws() {
        assert_eq!(
            extract_relay_host("ws://10.0.0.1:8443/api/tunnel/register"),
            Some("10.0.0.1".to_string())
        );
    }

    #[test]
    fn test_extract_relay_host_invalid() {
        assert_eq!(extract_relay_host("http://example.com"), None);
        assert_eq!(extract_relay_host(""), None);
    }

    #[test]
    fn test_is_openwrt() {
        // Just verify it doesn't panic — result depends on host system
        let _ = is_openwrt();
    }

    #[test]
    fn test_watchdog_cooldown_dormant() {
        let mut state = WatchdogState::new();
        state.dormant = true;
        state.last_action_at = Some(Instant::now());
        // Just set, should not be elapsed yet
        assert!(!state.cooldown_elapsed());
    }

    #[test]
    fn test_watchdog_cooldown_l3_backoff() {
        let mut state = WatchdogState::new();
        state.level = 3;
        state.l3_attempts = 1;
        state.last_action_at = Some(Instant::now());
        // L3 backoff: 300 * 2^1 = 600s, should not be elapsed
        assert!(!state.cooldown_elapsed());
    }

    #[test]
    fn test_watchdog_reset_clears_all() {
        let mut state = WatchdogState::new();
        state.level = 3;
        state.l3_attempts = 5;
        state.dormant = true;
        state.internet_reachable = true;
        state.internet_reachable_since = Some(Instant::now());
        state.searching_since = Some(Instant::now());
        state.reset();
        assert_eq!(state.level, 0);
        assert_eq!(state.l3_attempts, 0);
        assert!(!state.dormant);
        assert!(!state.internet_reachable);
        assert!(state.internet_reachable_since.is_none());
        assert!(state.searching_since.is_none());
    }
}
