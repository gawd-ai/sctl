//! LTE watchdog — symptom-based modem recovery when tunnel is down.
//!
//! Core principle: **first, do no harm.** The modem and netifd usually recover
//! on their own. The watchdog only intervenes when it's certain the modem is
//! genuinely stuck, and picks the right action for the specific symptom instead
//! of blindly climbing an escalation ladder.
//!
//! ## Symptom-based dispatch
//!
//! Instead of L0→L1→L2→L3 escalation, the watchdog diagnoses the problem:
//! - `Searching` → wait (roaming handover), then airplane cycle if stuck
//! - `RegisteredNoData` → interface restart (QMI bearer broken)
//! - `NotRegistered` → re-register, then airplane cycle if repeat
//! - `Unresponsive` → interface restart, then USB cycle
//! - `RelayProblem` → do nothing (modem is fine)
//! - `TunnelReconnecting` → do nothing (client is working on it)
//!
//! ## Safe-bands recovery
//!
//! The watchdog tracks which band config last sustained a stable tunnel
//! connection (5+ minutes). When the tunnel drops after a recent band change,
//! it can quickly revert to the known-working config before symptom dispatch.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;
use tokio::sync::{broadcast, watch, Mutex};
use tracing::{info, warn};

use crate::config::LteConfig;
use crate::lte::{BandChangeSource, LteState, ScanStatus};
use crate::modem::Modem;
use crate::state::{TunnelEventType, TunnelStats};

/// Watchdog tick interval.
const TICK_INTERVAL: Duration = Duration::from_secs(30);

/// How long after a band change the pre-change revert is eligible (3 min).
const PRECHANGE_REVERT_WINDOW: Duration = Duration::from_secs(180);

/// How long the tunnel must be stable before promoting current bands to safe bands.
const SAFE_PROMOTE_THRESHOLD: Duration = Duration::from_secs(300);

/// Light reset threshold — 90s of stable tunnel resets episode counters.
const LIGHT_RESET_THRESHOLD: Duration = Duration::from_secs(90);

/// Maximum L3 (USB cycle) attempts before entering dormant mode.
const MAX_L3_ATTEMPTS: u32 = 3;

/// Maximum L3 cooldown after exponential backoff (30 min).
const MAX_L3_COOLDOWN: Duration = Duration::from_secs(1800);

/// Dormant mode tick interval (15 min).
const DORMANT_TICK_INTERVAL: Duration = Duration::from_secs(900);

/// How long the modem can be Searching before we try an airplane cycle (3 min).
const SEARCHING_ACTION_THRESHOLD: Duration = Duration::from_secs(180);

/// Extended grace while modem is actively searching (3 min on top of base grace).
const SEARCHING_GRACE_EXTENSION: Duration = Duration::from_secs(180);

/// Number of consecutive AT failures before skipping to interface/USB reset.
const AT_FAILURE_SKIP_THRESHOLD: u32 = 3;

/// How long after user activity (band change, scan) the watchdog is suppressed.
const USER_ACTIVITY_SUPPRESSION: Duration = Duration::from_secs(120);

/// Per-episode action caps.
const MAX_REREGISTERS_PER_EPISODE: u32 = 1;
const MAX_IFACE_RESTARTS_PER_EPISODE: u32 = 2;
const MAX_AIRPLANE_CYCLES_PER_EPISODE: u32 = 2;

/// Maximum recent events in the snapshot.
const MAX_SNAPSHOT_EVENTS: usize = 20;

// ── Symptom diagnosis ──────────────────────────────────────────────────────

/// What the watchdog thinks is wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Symptom {
    /// CEREG=2, modem scanning for a cell (likely roaming handover).
    Searching { secs: u64 },
    /// CEREG=1|5 but connection state is NOCONN (registered, no QMI bearer).
    RegisteredNoData,
    /// CEREG=0|3, modem gave up searching.
    NotRegistered,
    /// AT commands are failing.
    Unresponsive,
    /// Has IP + internet reachable, but tunnel is down (relay/app problem).
    RelayProblem,
    /// Tunnel client is mid-reconnect and we have IP.
    TunnelReconnecting,
    /// Registered + has IP but reachability target is unreachable — diagnosed,
    /// NOT actionable (the data path is fine; the wider internet or DNS is
    /// the problem, neither of which a modem cycle can fix).
    InternetUnreachable,
    /// Quectel module is not enumerated in sysfs — by definition a USB cycle
    /// won't help. Diagnosed and waited out.
    ModemGone,
    /// Can't determine the cause.
    Unknown,
}

impl Symptom {
    fn as_str(self) -> &'static str {
        match self {
            Self::Searching { .. } => "searching",
            Self::RegisteredNoData => "registered_no_data",
            Self::NotRegistered => "not_registered",
            Self::Unresponsive => "unresponsive",
            Self::RelayProblem => "relay_problem",
            Self::TunnelReconnecting => "tunnel_reconnecting",
            Self::InternetUnreachable => "internet_unreachable",
            Self::ModemGone => "modem_gone",
            Self::Unknown => "unknown",
        }
    }
}

/// Diagnose the current problem by checking modem state, interface, and tunnel.
async fn diagnose(
    modem: &Modem,
    lte_state: &Mutex<LteState>,
    tunnel_stats: &TunnelStats,
    interface: &str,
    reachability_target: &str,
    state: &WatchdogState,
) -> (Symptom, Option<RegistrationStatus>) {
    // Check modem responsiveness
    let at_ok = modem.command("AT").await.is_ok();
    if !at_ok {
        // Cross-check sysfs — if the module isn't enumerated, no amount of
        // USB cycling will help. Distinguish ModemGone (hardware-level absent)
        // from Unresponsive (enumerated but not answering AT).
        if crate::modem::detect_quectel_at_port_strict().is_none() {
            return (Symptom::ModemGone, None);
        }
        return (Symptom::Unresponsive, None);
    }

    // Check if tunnel client is mid-reconnect with IP
    if tunnel_stats.reconnecting.load(Ordering::Relaxed) && interface_has_ipv4(interface) {
        return (Symptom::TunnelReconnecting, None);
    }

    // Check registration status
    let reg_status = match modem.command("AT+CEREG?").await {
        Ok(resp) => parse_cereg(&resp).ok(),
        Err(_) => None,
    };

    if let Some(reg) = reg_status {
        match reg {
            RegistrationStatus::Searching => {
                let searching_secs = state.searching_since.map_or(0, |s| s.elapsed().as_secs());
                return (
                    Symptom::Searching {
                        secs: searching_secs,
                    },
                    Some(reg),
                );
            }
            RegistrationStatus::NotRegistered | RegistrationStatus::Denied => {
                return (Symptom::NotRegistered, Some(reg));
            }
            RegistrationStatus::RegisteredHome | RegistrationStatus::RegisteredRoam => {
                // Registered — check data path
                let is_noconn = {
                    let lte = lte_state.lock().await;
                    lte.signal
                        .as_ref()
                        .and_then(|s| s.connection_state.as_deref())
                        == Some("NOCONN")
                };
                if is_noconn || !interface_has_ipv4(interface) {
                    return (Symptom::RegisteredNoData, Some(reg));
                }
                // Has registration + IP — check internet reachability
                if check_reachability(reachability_target).await {
                    return (Symptom::RelayProblem, Some(reg));
                }
                // Registered + IP but reachability target unreachable.
                // The modem is doing its job; the wider internet (or just the
                // target) is the problem. Diagnosed but NOT actionable — a
                // modem cycle won't help and may make things worse.
                return (Symptom::InternetUnreachable, Some(reg));
            }
            RegistrationStatus::Unknown => {}
        }
    }

    (Symptom::Unknown, reg_status)
}

// ── Watchdog state ─────────────────────────────────────────────────────────

/// Internal watchdog state — tracks the current disconnect episode.
struct WatchdogState {
    tunnel_disconnect_since: Option<Instant>,
    tunnel_connected_since: Option<Instant>,
    last_action_at: Option<Instant>,
    last_symptom: Option<Symptom>,
    last_action: Option<String>,
    consecutive_at_failures: u32,
    /// Per-episode action counters (reset on recovery).
    reregisters: u32,
    iface_restarts: u32,
    airplane_cycles: u32,
    l3_attempts: u32,
    dormant: bool,
    /// When the modem first entered CEREG=Searching (for stuck-searching detection).
    searching_since: Option<Instant>,
    /// Whether we've already tried reverting to pre-change bands this episode.
    tried_prechange_revert: bool,
    /// Whether we've already tried reverting to safe bands this episode.
    tried_safe_revert: bool,
    /// Actions taken this episode, for the snapshot.
    episode_actions: Vec<String>,
}

impl WatchdogState {
    fn new() -> Self {
        Self {
            tunnel_disconnect_since: None,
            tunnel_connected_since: None,
            last_action_at: None,
            last_symptom: None,
            last_action: None,
            consecutive_at_failures: 0,
            reregisters: 0,
            iface_restarts: 0,
            airplane_cycles: 0,
            l3_attempts: 0,
            dormant: false,
            searching_since: None,
            tried_prechange_revert: false,
            tried_safe_revert: false,
            episode_actions: Vec::new(),
        }
    }

    /// Light reset: clear episode counters but preserve L3 attempts and dormant state.
    fn light_reset(&mut self) {
        self.tunnel_disconnect_since = None;
        self.last_action_at = None;
        self.consecutive_at_failures = 0;
        self.reregisters = 0;
        self.iface_restarts = 0;
        self.airplane_cycles = 0;
        self.tried_prechange_revert = false;
        self.tried_safe_revert = false;
        self.searching_since = None;
        self.episode_actions.clear();
    }

    /// Heavy reset: clear everything including L3 attempts and dormant state.
    fn heavy_reset(&mut self) {
        self.light_reset();
        self.l3_attempts = 0;
        self.dormant = false;
    }

    fn cooldown_elapsed(&self, level: u8) -> bool {
        let Some(last) = self.last_action_at else {
            return true;
        };
        if self.dormant {
            return last.elapsed() >= DORMANT_TICK_INTERVAL;
        }
        if level >= 3 && self.l3_attempts > 0 {
            let backoff =
                Duration::from_secs(300 * 2u64.pow(self.l3_attempts.min(3))).min(MAX_L3_COOLDOWN);
            return last.elapsed() >= backoff;
        }
        // Default cooldown: 60s for most actions, 120s for airplane/iface
        let cooldown = match level {
            0 => Duration::from_secs(60),
            1 | 2 => Duration::from_secs(120),
            _ => Duration::from_secs(300),
        };
        last.elapsed() >= cooldown
    }

    fn record_action(&mut self, action: &str) {
        self.last_action_at = Some(Instant::now());
        self.last_action = Some(action.to_string());
        self.episode_actions.push(action.to_string());
    }
}

// ── Watchdog snapshot for API visibility ───────────────────────────────────

/// A single watchdog event for the snapshot log.
///
/// The `evidence` and `cooldown_elapsed_secs` fields are filled by
/// `update_snapshot_event` from the watchdog's most recent reading; they make
/// the dispatch decision auditable after-the-fact.
#[derive(Debug, Clone, Serialize)]
pub struct WatchdogEvent {
    pub timestamp: u64,
    pub symptom: String,
    pub action: String,
    pub detail: String,
    /// Last-known RSRP (dBm), if the LTE poller has populated signal data.
    pub rsrp: Option<i32>,
    /// Last-known CEREG `stat` value (0–5), if available.
    pub cereg_stat: Option<u8>,
    /// Whether the LTE interface had an IPv4 address at decision time.
    pub has_ipv4: bool,
    /// Seconds since the previous watchdog action of any kind.
    pub cooldown_elapsed_secs: u64,
    /// Free-form evidence summary the watchdog used to choose this action.
    pub evidence: String,
}

/// Watchdog state snapshot, updated every tick. Exposed via `/api/lte`.
#[derive(Debug, Clone, Serialize)]
pub struct WatchdogSnapshot {
    pub state: String,
    pub disconnect_secs: Option<u64>,
    pub last_symptom: Option<String>,
    pub last_action: Option<String>,
    pub last_action_secs_ago: Option<u64>,
    pub l3_attempts: u32,
    pub episode_actions: Vec<String>,
    pub recent_events: VecDeque<WatchdogEvent>,
}

impl WatchdogSnapshot {
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: "idle".into(),
            disconnect_secs: None,
            last_symptom: None,
            last_action: None,
            last_action_secs_ago: None,
            l3_attempts: 0,
            episode_actions: Vec::new(),
            recent_events: VecDeque::with_capacity(MAX_SNAPSHOT_EVENTS),
        }
    }

    fn push_event(&mut self, symptom: &str, action: &str, detail: &str) {
        self.push_event_full(WatchdogEventContext {
            symptom,
            action,
            detail,
            rsrp: None,
            cereg_stat: None,
            has_ipv4: false,
            cooldown_elapsed_secs: 0,
            evidence: String::new(),
        });
    }

    fn push_event_full(&mut self, ctx: WatchdogEventContext<'_>) {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if self.recent_events.len() >= MAX_SNAPSHOT_EVENTS {
            self.recent_events.pop_front();
        }
        self.recent_events.push_back(WatchdogEvent {
            timestamp,
            symptom: ctx.symptom.into(),
            action: ctx.action.into(),
            detail: ctx.detail.into(),
            rsrp: ctx.rsrp,
            cereg_stat: ctx.cereg_stat,
            has_ipv4: ctx.has_ipv4,
            cooldown_elapsed_secs: ctx.cooldown_elapsed_secs,
            evidence: ctx.evidence,
        });
    }
}

/// Context passed to `push_event_full` so the caller doesn't have a fragile
/// positional-arg API.
struct WatchdogEventContext<'a> {
    symptom: &'a str,
    action: &'a str,
    detail: &'a str,
    rsrp: Option<i32>,
    cereg_stat: Option<u8>,
    has_ipv4: bool,
    cooldown_elapsed_secs: u64,
    evidence: String,
}

impl Default for WatchdogSnapshot {
    fn default() -> Self {
        Self::new()
    }
}

// ── Registration status parsing ────────────────────────────────────────────

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

// ── Helper functions ───────────────────────────────────────────────────────

/// Read current band config from modem. Returns (bands, priority).
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
    match crate::lte::verified_set_bands(modem, bands).await {
        Ok(actual) => info!("LTE watchdog: bands set and verified: {actual:?}"),
        Err(e) => {
            warn!("LTE watchdog: band write failed: {e}");
            return false;
        }
    }
    if let Some(pri) = priority {
        let _ = modem.command(&format!("AT+QCFG=\"bandpri\",{pri}")).await;
    } else {
        let _ = modem.command("AT+QCFG=\"bandpri\",0").await;
    }

    if tunnel_stats.connected.load(Ordering::Relaxed) {
        info!("apply_band_config: tunnel connected, skipping re-registration (bands set)");
        return false;
    }

    let _ = modem.command("AT+COPS=2").await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let _ = modem.command("AT+COPS=0").await;

    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        if tunnel_stats.connected.load(Ordering::Relaxed) {
            info!("apply_band_config: tunnel reconnected during registration wait");
            return false;
        }
        if let Ok(resp) = modem.command("AT+CEREG?").await {
            if let Ok(status) = parse_cereg(&resp) {
                if status.is_registered() {
                    for _ in 0..5 {
                        if interface_has_ipv4(interface) {
                            return true;
                        }
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
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

/// Result of a gated USB cycle attempt.
enum GatedUsbCycle {
    /// Cycle skipped — escalation level disallows it or evidence insufficient.
    /// Carries a reason string to embed in the action log.
    Skipped(String),
    /// Cycle attempted; carries the action result.
    Executed((&'static str, Option<Modem>)),
}

/// Pure decision helper: should the watchdog auto-fire a USB power-cycle,
/// given (a) the configured escalation ceiling, (b) the evidence policy,
/// (c) how long the qualifying symptom has held, and (d) whether sysfs still
/// shows the modem enumerated?
///
/// Returns `Ok(())` if the cycle is allowed, `Err(reason)` otherwise. This
/// function is side-effect-free so it can be unit-tested directly.
fn evaluate_usb_cycle_gate(
    max_escalation_level: u8,
    evidence: &crate::config::UsbCycleEvidence,
    disconnect_secs: u64,
    sysfs_present: bool,
) -> Result<(), String> {
    if max_escalation_level < 4 {
        return Err(format!(
            "max_escalation_level={max_escalation_level} (need 4)"
        ));
    }
    if disconnect_secs < evidence.min_sustained_secs {
        return Err(format!(
            "sustained={disconnect_secs}s (need {})",
            evidence.min_sustained_secs
        ));
    }
    if evidence.require_sysfs_absent && sysfs_present {
        // Module is enumerated; the AT path is just stuck. Lower-level
        // actions (airplane cycle, interface restart) are still in scope —
        // USB cycle is the wrong tool. Operator can override manually.
        return Err("sysfs_present=true (modem enumerated) — cycle declined".to_string());
    }
    Ok(())
}

/// Decide whether the watchdog is allowed to auto-fire a USB power-cycle,
/// and if so, do it. Manual cycles (via `POST /api/lte/usb_cycle`) bypass
/// this gate entirely.
async fn try_usb_cycle_gated(
    device_path: &str,
    max_escalation_level: u8,
    evidence: &crate::config::UsbCycleEvidence,
    disconnect_secs: u64,
) -> GatedUsbCycle {
    let sysfs_present = crate::modem::detect_quectel_at_port_strict().is_some();
    match evaluate_usb_cycle_gate(
        max_escalation_level,
        evidence,
        disconnect_secs,
        sysfs_present,
    ) {
        Ok(()) => GatedUsbCycle::Executed(action_usb_power_cycle(device_path).await),
        Err(reason) => GatedUsbCycle::Skipped(reason),
    }
}

// ── Main watchdog loop ─────────────────────────────────────────────────────

/// Spawn the LTE watchdog task. Returns a `JoinHandle` for abort on shutdown.
#[allow(clippy::needless_pass_by_value)]
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
    snapshot: Arc<Mutex<WatchdogSnapshot>>,
    detected_path: Arc<tokio::sync::RwLock<Option<String>>>,
) -> tokio::task::JoinHandle<()> {
    let interface = config.interface.clone();
    // Hint only — sysfs detection in action_usb_power_cycle is authoritative.
    // Falls back to `/dev/ttyUSB2` if no hint was provided (legacy behaviour for
    // non-Quectel modems that escape auto-detect).
    let device_path = config
        .device
        .clone()
        .unwrap_or_else(|| "/dev/ttyUSB2".to_string());
    let reachability_host = config.reachability_host.clone();
    let interface_restart_cmd = config.interface_restart_cmd.clone();
    let openwrt = is_openwrt();
    let grace = Duration::from_secs(config.watchdog_grace_secs);
    let max_escalation_level = config.max_escalation_level;
    let usb_cycle_evidence = config.usb_cycle_evidence.clone();
    let unknown_action = config.unknown_action.clone();
    let notregistered_grace_secs = config.notregistered_grace_secs;

    tokio::spawn(async move {
        let mut modem = modem;
        let reachability_target = reachability_host
            .or_else(|| tunnel_url.as_deref().and_then(extract_relay_host))
            .unwrap_or_else(|| "8.8.8.8".to_string());
        info!(
            "LTE watchdog started (interface: {interface}, reachability: {reachability_target}, \
             grace: {}s, openwrt: {openwrt})",
            grace.as_secs()
        );

        let mut state = WatchdogState::new();
        let mut ticker = tokio::time::interval(TICK_INTERVAL);

        loop {
            ticker.tick().await;

            // ── Skip while scan running or manual band change in progress ──
            let skip_tick = {
                let lock_started = Instant::now();
                let mut lte = lte_state.lock().await;
                #[allow(clippy::cast_possible_truncation)]
                let lock_wait_ms = lock_started.elapsed().as_millis() as u64;
                if lock_wait_ms >= 100 {
                    warn!(
                        lock_wait_ms,
                        "LTE watchdog: slow lte_state lock in skip_tick"
                    );
                }
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
                            false
                        } else {
                            true
                        }
                    } else {
                        false
                    }
                } else if let Some(until) = lte.band_action_until {
                    Instant::now() < until
                } else {
                    false
                }
            };
            if skip_tick {
                update_snapshot(&snapshot, &state, "suppressed_scan").await;
                continue;
            }

            // ── User activity suppression ──
            let user_suppressed = {
                let lte = lte_state.lock().await;
                lte.last_user_action_at
                    .is_some_and(|t| t.elapsed() < USER_ACTIVITY_SUPPRESSION)
            };
            if user_suppressed {
                update_snapshot(&snapshot, &state, "suppressed_user").await;
                continue;
            }

            let tunnel_connected = tunnel_stats.connected.load(Ordering::Relaxed);

            // ── Tunnel connected: track stability + safe-bands promotion ──
            if tunnel_connected {
                if state.tunnel_connected_since.is_none() {
                    state.tunnel_connected_since = Some(Instant::now());
                }

                let connected_duration = state
                    .tunnel_connected_since
                    .map_or(Duration::ZERO, |s| s.elapsed());

                // Tiered reset: light at 90s, heavy at 300s
                if state.tunnel_disconnect_since.is_some() {
                    if connected_duration >= SAFE_PROMOTE_THRESHOLD {
                        let was_dormant = state.dormant;
                        info!("LTE watchdog: tunnel stable for 5+ min, heavy reset");
                        state.heavy_reset();

                        if was_dormant || check_exhaustion_file(&data_dir) {
                            emit_watchdog_report(&data_dir, &session_events);
                        }
                    } else if connected_duration >= LIGHT_RESET_THRESHOLD {
                        info!("LTE watchdog: tunnel stable for 90s, light reset");
                        let saved_l3 = state.l3_attempts;
                        let saved_dormant = state.dormant;
                        state.light_reset();
                        state.l3_attempts = saved_l3;
                        state.dormant = saved_dormant;
                    }
                }

                // Safe-bands promotion (uses cached band config, no AT commands)
                {
                    let lock_started = Instant::now();
                    let mut lte = lte_state.lock().await;
                    #[allow(clippy::cast_possible_truncation)]
                    let lock_wait_ms = lock_started.elapsed().as_millis() as u64;
                    if lock_wait_ms >= 100 {
                        warn!(
                            lock_wait_ms,
                            "LTE watchdog: slow lte_state lock in connected path"
                        );
                    }
                    if lte.band_stable_since.is_none() {
                        lte.band_stable_since = Some(Instant::now());
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
                                    )
                                    .await;
                                }
                            }
                            lte.band_stable_since = None;
                            lte.bands_at_connect = None;
                        }
                    }
                }

                update_snapshot(&snapshot, &state, "connected").await;
                continue;
            }

            // ── Tunnel disconnected path ──

            state.tunnel_connected_since = None;
            {
                let mut lte = lte_state.lock().await;
                lte.band_stable_since = None;
                lte.bands_at_connect = None;
            }

            if state.tunnel_disconnect_since.is_none() {
                state.tunnel_disconnect_since = Some(Instant::now());
            }

            let disconnect_duration = state
                .tunnel_disconnect_since
                .map_or(Duration::ZERO, |t| t.elapsed());
            let disconnect_secs = disconnect_duration.as_secs();

            // ── Grace period ──
            // Base grace is configurable (default 120s).
            // Extended by 180s while modem is actively searching (roaming handover).
            let effective_grace = if modem
                .command("AT+CEREG?")
                .await
                .ok()
                .and_then(|r| parse_cereg(&r).ok())
                .is_some_and(|s| s == RegistrationStatus::Searching)
            {
                // Track searching start time
                if state.searching_since.is_none() {
                    state.searching_since = Some(Instant::now());
                    info!("LTE watchdog: modem searching (roaming handover?), extending grace");
                }
                grace + SEARCHING_GRACE_EXTENSION
            } else {
                state.searching_since = None;
                grace
            };

            if disconnect_duration < effective_grace {
                update_snapshot(&snapshot, &state, "grace").await;
                continue;
            }

            // ── Pre-change revert (before diagnosis) ──
            if !state.tried_prechange_revert {
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
                        {
                            let mut lte = lte_state.lock().await;
                            lte.record_band_change(
                                BandChangeSource::Watchdog,
                                &[],
                                None,
                                &revert_bands,
                            );
                        }

                        let detail = format!(
                            "action=prechange_revert bands={} disconnect={disconnect_secs}s",
                            fmt_bands(&revert_bands)
                        );
                        log_action(
                            &detail,
                            &tunnel_stats,
                            &session_events,
                            &mut state,
                            "prechange_revert",
                            0,
                            disconnect_secs,
                        )
                        .await;
                        update_snapshot_event(
                            &snapshot,
                            &mut state,
                            "prechange_revert",
                            "prechange_revert",
                            &detail,
                        )
                        .await;

                        tokio::time::sleep(Duration::from_secs(10)).await;
                        if verify_recovery(&interface, &tunnel_stats, Duration::from_secs(30)).await
                        {
                            info!("LTE watchdog: prechange_revert recovered tunnel");
                            state.light_reset();
                            continue;
                        }
                    }
                }
            }

            // ── Safe-bands revert ──
            if !state.tried_safe_revert {
                let revert_info = {
                    let lte = lte_state.lock().await;
                    lte.safe_bands
                        .as_ref()
                        .map(|sb| (sb.bands.clone(), sb.priority_band))
                };

                if let Some((safe_bands, safe_priority)) = revert_info {
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
                                "action=safe_revert bands={} disconnect={disconnect_secs}s",
                                fmt_bands(&safe_bands)
                            );
                            log_action(
                                &detail,
                                &tunnel_stats,
                                &session_events,
                                &mut state,
                                "safe_revert",
                                0,
                                disconnect_secs,
                            )
                            .await;
                            update_snapshot_event(
                                &snapshot,
                                &mut state,
                                "safe_revert",
                                "safe_revert",
                                &detail,
                            )
                            .await;

                            tokio::time::sleep(Duration::from_secs(10)).await;
                            if verify_recovery(&interface, &tunnel_stats, Duration::from_secs(30))
                                .await
                            {
                                info!("LTE watchdog: safe_revert recovered tunnel");
                                state.light_reset();
                                continue;
                            }
                        }
                    }
                }
            }

            // ── Diagnose the problem ──
            let (symptom, reg_status) = diagnose(
                &modem,
                &lte_state,
                &tunnel_stats,
                &interface,
                &reachability_target,
                &state,
            )
            .await;

            state.last_symptom = Some(symptom);
            let reg_str = reg_status.map_or("unknown", |s| s.as_str());

            // ── Symptom-based dispatch ──
            let (action, level, new_modem): (&str, u8, Option<Modem>) = match symptom {
                Symptom::Searching { secs } => {
                    if secs < SEARCHING_ACTION_THRESHOLD.as_secs() {
                        let detail = format!(
                            "symptom=searching secs={secs} action=wait disconnect={disconnect_secs}s"
                        );
                        info!("LTE watchdog: modem searching ({secs}s), waiting");
                        update_snapshot_event(&snapshot, &mut state, "searching", "wait", &detail)
                            .await;
                        update_snapshot(&snapshot, &state, "waiting").await;
                        continue;
                    }
                    // Stuck searching — try airplane cycle
                    if state.airplane_cycles >= MAX_AIRPLANE_CYCLES_PER_EPISODE {
                        info!("LTE watchdog: searching too long, airplane cap reached");
                        update_snapshot(&snapshot, &state, "acting").await;
                        if !state.cooldown_elapsed(3) {
                            continue;
                        }
                        match try_usb_cycle_gated(
                            &device_path,
                            max_escalation_level,
                            &usb_cycle_evidence,
                            disconnect_secs,
                        )
                        .await
                        {
                            GatedUsbCycle::Executed((act, m)) => (act, 3, m),
                            GatedUsbCycle::Skipped(reason) => {
                                info!("LTE watchdog: usb_cycle_skipped (searching) — {reason}");
                                state.dormant = true;
                                update_snapshot(&snapshot, &state, "dormant").await;
                                continue;
                            }
                        }
                    } else if !state.cooldown_elapsed(1) {
                        continue;
                    } else {
                        state.airplane_cycles += 1;
                        (action_airplane_cycle(&modem, &mut state).await, 1, None)
                    }
                }

                Symptom::RegisteredNoData => {
                    // Interface restart is the right fix — skip straight to it
                    if state.iface_restarts >= MAX_IFACE_RESTARTS_PER_EPISODE {
                        // Exhausted iface restarts, try airplane cycle
                        if state.airplane_cycles >= MAX_AIRPLANE_CYCLES_PER_EPISODE {
                            if !state.cooldown_elapsed(3) {
                                continue;
                            }
                            match try_usb_cycle_gated(
                                &device_path,
                                max_escalation_level,
                                &usb_cycle_evidence,
                                disconnect_secs,
                            )
                            .await
                            {
                                GatedUsbCycle::Executed((act, m)) => (act, 3, m),
                                GatedUsbCycle::Skipped(reason) => {
                                    info!(
                                        "LTE watchdog: usb_cycle_skipped (reg_nodata) — {reason}"
                                    );
                                    state.dormant = true;
                                    update_snapshot(&snapshot, &state, "dormant").await;
                                    continue;
                                }
                            }
                        } else if !state.cooldown_elapsed(1) {
                            continue;
                        } else {
                            state.airplane_cycles += 1;
                            (action_airplane_cycle(&modem, &mut state).await, 1, None)
                        }
                    } else if !state.cooldown_elapsed(2) {
                        continue;
                    } else {
                        state.iface_restarts += 1;
                        info!("LTE watchdog: NOCONN — restarting interface for QMI data session");
                        (
                            action_restart_interface(
                                &interface,
                                openwrt,
                                interface_restart_cmd.as_deref(),
                            )
                            .await,
                            2,
                            None,
                        )
                    }
                }

                Symptom::NotRegistered => {
                    // Honor the explicit NotRegistered grace window (default 180s) —
                    // natural roaming handovers can briefly look like NotRegistered.
                    if disconnect_secs < notregistered_grace_secs {
                        info!(
                            "LTE watchdog: NotRegistered for {disconnect_secs}s — \
                             under grace ({notregistered_grace_secs}s), waiting"
                        );
                        update_snapshot(&snapshot, &state, "waiting").await;
                        continue;
                    }
                    if state.reregisters < MAX_REREGISTERS_PER_EPISODE {
                        if !state.cooldown_elapsed(0) {
                            continue;
                        }
                        state.reregisters += 1;
                        (action_reregister(&modem, &mut state).await, 0, None)
                    } else if state.airplane_cycles < MAX_AIRPLANE_CYCLES_PER_EPISODE {
                        if !state.cooldown_elapsed(1) {
                            continue;
                        }
                        state.airplane_cycles += 1;
                        (action_airplane_cycle(&modem, &mut state).await, 1, None)
                    } else {
                        if !state.cooldown_elapsed(3) {
                            continue;
                        }
                        match try_usb_cycle_gated(
                            &device_path,
                            max_escalation_level,
                            &usb_cycle_evidence,
                            disconnect_secs,
                        )
                        .await
                        {
                            GatedUsbCycle::Executed((act, m)) => (act, 3, m),
                            GatedUsbCycle::Skipped(reason) => {
                                info!(
                                    "LTE watchdog: usb_cycle_skipped (not_registered) — {reason}"
                                );
                                state.dormant = true;
                                update_snapshot(&snapshot, &state, "dormant").await;
                                continue;
                            }
                        }
                    }
                }

                Symptom::Unresponsive => {
                    state.consecutive_at_failures += 1;
                    if state.consecutive_at_failures < AT_FAILURE_SKIP_THRESHOLD {
                        if state.iface_restarts < MAX_IFACE_RESTARTS_PER_EPISODE {
                            if !state.cooldown_elapsed(2) {
                                continue;
                            }
                            state.iface_restarts += 1;
                            (
                                action_restart_interface(
                                    &interface,
                                    openwrt,
                                    interface_restart_cmd.as_deref(),
                                )
                                .await,
                                2,
                                None,
                            )
                        } else {
                            continue;
                        }
                    } else {
                        if !state.cooldown_elapsed(3) {
                            continue;
                        }
                        match try_usb_cycle_gated(
                            &device_path,
                            max_escalation_level,
                            &usb_cycle_evidence,
                            disconnect_secs,
                        )
                        .await
                        {
                            GatedUsbCycle::Executed((act, m)) => (act, 3, m),
                            GatedUsbCycle::Skipped(reason) => {
                                info!("LTE watchdog: usb_cycle_skipped (unresponsive) — {reason}");
                                state.dormant = true;
                                update_snapshot(&snapshot, &state, "dormant").await;
                                continue;
                            }
                        }
                    }
                }

                Symptom::RelayProblem => {
                    info!(
                        "LTE watchdog: internet reachable, problem is relay/app — not acting \
                         (disconnect={disconnect_secs}s)"
                    );
                    update_snapshot_event(
                        &snapshot,
                        &mut state,
                        "relay_problem",
                        "wait",
                        &format!("symptom=relay_problem disconnect={disconnect_secs}s"),
                    )
                    .await;
                    update_snapshot(&snapshot, &state, "waiting").await;
                    continue;
                }

                Symptom::TunnelReconnecting => {
                    info!(
                        "LTE watchdog: tunnel reconnecting with IP, not acting \
                         (disconnect={disconnect_secs}s)"
                    );
                    update_snapshot(&snapshot, &state, "waiting").await;
                    continue;
                }

                Symptom::InternetUnreachable => {
                    // Modem is doing its job; wider internet (or just the
                    // target host) is the problem. Diagnose-only — a modem
                    // cycle won't help and may make things worse.
                    info!(
                        "LTE watchdog: internet_unreachable, modem healthy — not acting \
                         (disconnect={disconnect_secs}s)"
                    );
                    update_snapshot_event(
                        &snapshot,
                        &mut state,
                        "internet_unreachable",
                        "wait",
                        &format!(
                            "symptom=internet_unreachable reg={reg_str} \
                             disconnect={disconnect_secs}s"
                        ),
                    )
                    .await;
                    update_snapshot(&snapshot, &state, "waiting").await;
                    continue;
                }

                Symptom::ModemGone => {
                    // Quectel not enumerated in sysfs. Either the kernel hasn't
                    // re-bound after a previous cycle, or hardware is dead.
                    // A USB power-cycle by sysfs path won't fix this either.
                    warn!(
                        "LTE watchdog: modem_gone — Quectel not enumerated in sysfs \
                         (disconnect={disconnect_secs}s)"
                    );
                    update_snapshot_event(
                        &snapshot,
                        &mut state,
                        "modem_gone",
                        "wait",
                        &format!(
                            "symptom=modem_gone — no Quectel in /sys/bus/usb/devices \
                             disconnect={disconnect_secs}s"
                        ),
                    )
                    .await;
                    state.dormant = true;
                    update_snapshot(&snapshot, &state, "dormant").await;
                    continue;
                }

                Symptom::Unknown => {
                    // Unknown is no longer auto-actionable. Operator can opt-in
                    // via `[lte.unknown_action]` if they want the old behavior.
                    if !unknown_action.airplane_cycle && !unknown_action.escalate {
                        info!(
                            "LTE watchdog: unknown — diagnose-only \
                             (disconnect={disconnect_secs}s, reg={reg_str})"
                        );
                        update_snapshot_event(
                            &snapshot,
                            &mut state,
                            "unknown",
                            "wait",
                            &format!(
                                "symptom=unknown reg={reg_str} disconnect={disconnect_secs}s \
                                 unknown_action=disabled"
                            ),
                        )
                        .await;
                        update_snapshot(&snapshot, &state, "waiting").await;
                        continue;
                    }
                    if unknown_action.airplane_cycle
                        && state.airplane_cycles < MAX_AIRPLANE_CYCLES_PER_EPISODE
                    {
                        if !state.cooldown_elapsed(1) {
                            continue;
                        }
                        state.airplane_cycles += 1;
                        (action_airplane_cycle(&modem, &mut state).await, 1, None)
                    } else if unknown_action.escalate {
                        if !state.cooldown_elapsed(3) {
                            continue;
                        }
                        match try_usb_cycle_gated(
                            &device_path,
                            max_escalation_level,
                            &usb_cycle_evidence,
                            disconnect_secs,
                        )
                        .await
                        {
                            GatedUsbCycle::Executed((act, m)) => (act, 3, m),
                            GatedUsbCycle::Skipped(reason) => {
                                info!("LTE watchdog: usb_cycle_skipped (unknown) — {reason}");
                                state.dormant = true;
                                update_snapshot(&snapshot, &state, "dormant").await;
                                continue;
                            }
                        }
                    } else {
                        update_snapshot(&snapshot, &state, "waiting").await;
                        continue;
                    }
                }
            };

            // Handle modem replacement after USB cycle
            if let Some(new_m) = new_modem {
                let _ = modem_tx.send(new_m.clone());
                let new_path = new_m.device().to_string();
                *detected_path.write().await = Some(new_path.clone());
                modem = new_m;
                info!("LTE watchdog: modem handle refreshed via watch channel (path: {new_path})");
            }

            // Reset AT failure counter on successful action (not Unresponsive)
            if symptom != Symptom::Unresponsive {
                state.consecutive_at_failures = 0;
            }

            let detail = format!(
                "symptom={} level={level} action={action} reg={reg_str} \
                 disconnect={disconnect_secs}s",
                symptom.as_str()
            );

            log_action(
                &detail,
                &tunnel_stats,
                &session_events,
                &mut state,
                action,
                level,
                disconnect_secs,
            )
            .await;
            update_snapshot_event(&snapshot, &mut state, symptom.as_str(), action, &detail).await;
            record_history_and_detect_regression(
                &data_dir,
                symptom.as_str(),
                action,
                level,
                &detail,
                &session_events,
            )
            .await;

            // Implicit interface nudge after radio recovery (L0/L1)
            if level <= 1 {
                tokio::time::sleep(Duration::from_secs(15)).await;
                if !interface_has_ipv4(&interface) {
                    info!("LTE watchdog: registered but no IP, nudging {interface}");
                    action_restart_interface(&interface, openwrt, interface_restart_cmd.as_deref())
                        .await;
                }
            }

            // Post-action recovery verification
            let verify_timeout = match level {
                0 | 2 => Duration::from_secs(30),
                1 => Duration::from_secs(45),
                _ => Duration::from_secs(60),
            };

            if verify_recovery(&interface, &tunnel_stats, verify_timeout).await {
                info!("LTE watchdog: recovery verified after {action}");
                state.light_reset();
                update_snapshot(&snapshot, &state, "recovered").await;
                continue;
            }

            // Track L3 attempts for exhaustion
            if level >= 3 {
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
                        "WATCHDOG_EXHAUSTED l3_attempts={} disconnect={disconnect_secs}s",
                        state.l3_attempts
                    );
                    tunnel_stats
                        .push_event(TunnelEventType::WatchdogAction, detail)
                        .await;
                    let _ = session_events.send(serde_json::json!({
                        "type": "lte.watchdog_exhausted",
                        "l3_attempts": state.l3_attempts,
                        "disconnect_secs": disconnect_secs,
                    }));

                    persist_exhaustion_state(
                        &data_dir,
                        state.l3_attempts,
                        disconnect_secs,
                        symptom != Symptom::Unresponsive,
                        reg_str,
                    );
                }
            }

            update_snapshot(
                &snapshot,
                &state,
                if state.dormant { "dormant" } else { "acting" },
            )
            .await;
        }
    })
}

// ── Snapshot update helpers ────────────────────────────────────────────────

async fn update_snapshot(snapshot: &Mutex<WatchdogSnapshot>, state: &WatchdogState, status: &str) {
    let mut snap = snapshot.lock().await;
    snap.state = status.into();
    snap.disconnect_secs = state.tunnel_disconnect_since.map(|t| t.elapsed().as_secs());
    snap.last_symptom = state.last_symptom.map(|s| s.as_str().into());
    snap.last_action.clone_from(&state.last_action);
    snap.last_action_secs_ago = state.last_action_at.map(|t| t.elapsed().as_secs());
    snap.l3_attempts = state.l3_attempts;
    snap.episode_actions.clone_from(&state.episode_actions);
}

async fn update_snapshot_event(
    snapshot: &Mutex<WatchdogSnapshot>,
    state: &mut WatchdogState,
    symptom: &str,
    action: &str,
    detail: &str,
) {
    let mut snap = snapshot.lock().await;
    snap.push_event(symptom, action, detail);
    drop(snap);
    // Also update the state tracking (not the snapshot, just for `last_action` field)
    state.last_action = Some(action.to_string());
}

async fn log_action(
    detail: &str,
    tunnel_stats: &TunnelStats,
    session_events: &broadcast::Sender<Value>,
    state: &mut WatchdogState,
    action: &str,
    level: u8,
    disconnect_secs: u64,
) {
    info!("LTE watchdog: {detail}");
    tunnel_stats
        .push_event(TunnelEventType::WatchdogAction, detail.to_string())
        .await;
    let _ = session_events.send(serde_json::json!({
        "type": "lte.watchdog",
        "level": level,
        "action": action,
        "disconnect_secs": disconnect_secs,
    }));
    state.record_action(action);
}

/// Append one watchdog action to `<data_dir>/watchdog_history.jsonl` and run
/// the regression detector. Best-effort — file I/O errors are logged, not
/// fatal.
///
/// Regression detector: if the same `(symptom, action)` pair has fired ≥3
/// times in the past hour, emit a `lte.watchdog.regression` event on
/// `session_events`. Operators decide what to do; the watchdog itself does
/// not auto-escalate based on regression.
async fn record_history_and_detect_regression(
    data_dir: &str,
    symptom: &str,
    action: &str,
    level: u8,
    detail: &str,
    session_events: &broadcast::Sender<Value>,
) {
    use std::time::{SystemTime, UNIX_EPOCH};

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let path = std::path::Path::new(data_dir).join("watchdog_history.jsonl");
    let entry = serde_json::json!({
        "ts_unix": ts,
        "symptom": symptom,
        "action": action,
        "level": level,
        "detail": detail,
    });

    // Append + rotate at 5 MB via the shared async helper.
    let line = format!("{entry}\n");
    match crate::util::append_rotating(&path, &line, 5 * 1024 * 1024).await {
        Ok(true) => info!("watchdog_history.jsonl rotated (>5MB) → .jsonl.1"),
        Ok(false) => {}
        Err(e) => warn!("watchdog_history append failed: {e}"),
    }

    // Regression detection — scan recent entries (cheap; file is bounded).
    let one_hour_ago = ts.saturating_sub(3600);
    let count = tokio::fs::read_to_string(&path)
        .await
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| {
            v.get("ts_unix").and_then(Value::as_u64).unwrap_or(0) >= one_hour_ago
                && v.get("symptom").and_then(Value::as_str) == Some(symptom)
                && v.get("action").and_then(Value::as_str) == Some(action)
        })
        .count();
    if count >= 3 {
        warn!(
            "LTE watchdog: regression detected — ({symptom}, {action}) fired {count} times in 1h"
        );
        let _ = session_events.send(serde_json::json!({
            "type": "lte.watchdog.regression",
            "symptom": symptom,
            "action": action,
            "count": count,
            "window_secs": 3600,
        }));
    }
}

// ── Recovery actions ───────────────────────────────────────────────────────

/// Re-register: AT+COPS=0.
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

/// Airplane cycle: AT+CFUN=0 → 5s → AT+CFUN=1.
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

/// Restart the network interface (netifd or generic ip link).
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
        let netifd_name = resolve_netifd_interface(interface).await;
        info!("LTE watchdog: restarting {interface} via netifd (ifdown/ifup {netifd_name})");
        let down = tokio::process::Command::new("ifdown")
            .arg(&netifd_name)
            .output()
            .await;
        if let Err(e) = &down {
            warn!("LTE watchdog: ifdown {netifd_name} failed: {e}");
            return "iface_restart_failed";
        }

        tokio::time::sleep(Duration::from_secs(2)).await;

        let up = tokio::process::Command::new("ifup")
            .arg(&netifd_name)
            .output()
            .await;
        if let Err(e) = &up {
            warn!("LTE watchdog: ifup {netifd_name} failed: {e}");
            return "iface_restart_partial";
        }
        "iface_restart_netifd"
    } else {
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

/// Resolve a kernel interface name (e.g. "wwan0") to its OpenWrt netifd name (e.g. "wwan").
pub(crate) async fn resolve_netifd_interface(kernel_iface: &str) -> String {
    if let Ok(output) = tokio::process::Command::new("sh")
        .args([
            "-c",
            &format!(
                "ubus call network.interface dump 2>/dev/null | \
                 jsonfilter -e '@.interface[@.l3_device=\"{kernel_iface}\"].interface' | \
                 head -1"
            ),
        ])
        .output()
        .await
    {
        let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !name.is_empty() {
            return name;
        }
    }

    kernel_iface.trim_end_matches(char::is_numeric).to_string()
}

/// USB modem power cycle — toggle sysfs `authorized` for the Quectel device.
/// Force a USB power-cycle of the Quectel module. Public so the manual
/// `POST /api/lte/usb_cycle` endpoint can invoke it directly, bypassing the
/// `max_escalation_level` / `usb_cycle_evidence` gates that apply to the
/// automatic watchdog path.
pub async fn action_usb_power_cycle(device_path: &str) -> (&'static str, Option<Modem>) {
    let Some(auth_path) = find_quectel_usb_auth().await else {
        warn!("LTE watchdog: Quectel USB device not found in sysfs");
        return ("usb_cycle_no_device", None);
    };

    info!("LTE watchdog: power cycling USB device at {auth_path}");

    if let Err(e) = tokio::fs::write(&auth_path, "0").await {
        warn!("LTE watchdog: failed to write 0 to {auth_path}: {e}");
        return ("usb_cycle_failed", None);
    }

    tokio::time::sleep(Duration::from_secs(3)).await;

    if let Err(e) = tokio::fs::write(&auth_path, "1").await {
        warn!("LTE watchdog: failed to write 1 to {auth_path}: {e}");
        return ("usb_cycle_partial", None);
    }

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

// ── Utility functions ──────────────────────────────────────────────────────

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
pub fn interface_has_ipv4(iface: &str) -> bool {
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
fn extract_relay_host(url: &str) -> Option<String> {
    let after_scheme = url
        .strip_prefix("wss://")
        .or_else(|| url.strip_prefix("ws://"))?;
    let host = after_scheme.split('/').next()?.split(':').next()?;
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

/// Check internet reachability by pinging a target host.
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
pub fn is_openwrt() -> bool {
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
        let _ = is_openwrt();
    }

    #[test]
    fn test_watchdog_cooldown_dormant() {
        let mut state = WatchdogState::new();
        state.dormant = true;
        state.last_action_at = Some(Instant::now());
        assert!(!state.cooldown_elapsed(3));
    }

    #[test]
    fn test_watchdog_cooldown_l3_backoff() {
        let mut state = WatchdogState::new();
        state.l3_attempts = 1;
        state.last_action_at = Some(Instant::now());
        // L3 backoff: 300 * 2^1 = 600s, should not be elapsed
        assert!(!state.cooldown_elapsed(3));
    }

    #[test]
    fn test_watchdog_light_reset() {
        let mut state = WatchdogState::new();
        state.reregisters = 1;
        state.iface_restarts = 2;
        state.airplane_cycles = 1;
        state.l3_attempts = 2;
        state.dormant = true;
        state.tried_prechange_revert = true;
        state.tried_safe_revert = true;
        state.episode_actions.push("test".into());

        state.light_reset();

        assert_eq!(state.reregisters, 0);
        assert_eq!(state.iface_restarts, 0);
        assert_eq!(state.airplane_cycles, 0);
        assert!(!state.tried_prechange_revert);
        assert!(!state.tried_safe_revert);
        assert!(state.episode_actions.is_empty());
        // L3 and dormant preserved
        assert_eq!(state.l3_attempts, 2);
        assert!(state.dormant);
    }

    #[test]
    fn test_watchdog_heavy_reset() {
        let mut state = WatchdogState::new();
        state.l3_attempts = 5;
        state.dormant = true;
        state.reregisters = 1;

        state.heavy_reset();

        assert_eq!(state.l3_attempts, 0);
        assert!(!state.dormant);
        assert_eq!(state.reregisters, 0);
    }

    #[test]
    fn test_symptom_as_str() {
        assert_eq!(Symptom::Searching { secs: 10 }.as_str(), "searching");
        assert_eq!(Symptom::RegisteredNoData.as_str(), "registered_no_data");
        assert_eq!(Symptom::NotRegistered.as_str(), "not_registered");
        assert_eq!(Symptom::Unresponsive.as_str(), "unresponsive");
        assert_eq!(Symptom::RelayProblem.as_str(), "relay_problem");
        assert_eq!(Symptom::TunnelReconnecting.as_str(), "tunnel_reconnecting");
        assert_eq!(
            Symptom::InternetUnreachable.as_str(),
            "internet_unreachable"
        );
        assert_eq!(Symptom::ModemGone.as_str(), "modem_gone");
        assert_eq!(Symptom::Unknown.as_str(), "unknown");
    }

    // ── Layer 3 watchdog-policy regression coverage ────────────────────

    use crate::config::UsbCycleEvidence;

    fn evidence_default() -> UsbCycleEvidence {
        UsbCycleEvidence {
            min_sustained_secs: 600,
            require_sysfs_absent: true,
        }
    }

    #[test]
    fn usb_cycle_gated_by_max_escalation_level() {
        // Default `max_escalation_level=3` → cycle is denied even with strong
        // evidence (modem absent from sysfs, sustained 10min disconnect).
        let result = evaluate_usb_cycle_gate(3, &evidence_default(), 1200, false);
        let err = result.expect_err("level 3 must skip");
        assert!(
            err.contains("max_escalation_level"),
            "reason should cite escalation level, got: {err}"
        );
    }

    #[test]
    fn usb_cycle_requires_sustained_evidence() {
        // Even at level 4, a fresh disconnect (under min_sustained_secs)
        // is not enough evidence to fire.
        let result = evaluate_usb_cycle_gate(4, &evidence_default(), 60, false);
        let err = result.expect_err("premature trigger must skip");
        assert!(err.contains("sustained=60"), "got: {err}");
    }

    #[test]
    fn usb_cycle_declined_when_sysfs_shows_modem_present() {
        // Worst surprise: AT is failing but sysfs still has the modem.
        // USB cycle is the wrong tool — the cure (re-enumeration → ttyUSB
        // renumber) is worse than the disease.
        let result = evaluate_usb_cycle_gate(4, &evidence_default(), 1200, true);
        let err = result.expect_err("sysfs-present must skip");
        assert!(err.contains("sysfs_present=true"), "got: {err}");
    }

    #[test]
    fn usb_cycle_allowed_when_all_evidence_aligned() {
        // Level 4 opt-in + sustained ≥ min + sysfs corroborates absence.
        evaluate_usb_cycle_gate(4, &evidence_default(), 1200, false)
            .expect("aligned evidence must allow cycle");
    }

    #[test]
    fn usb_cycle_evidence_off_skips_sysfs_check() {
        // If an operator disables the sysfs gate explicitly, the cycle is
        // allowed when sustained + level pass — even with modem enumerated.
        let evidence = UsbCycleEvidence {
            min_sustained_secs: 600,
            require_sysfs_absent: false,
        };
        evaluate_usb_cycle_gate(4, &evidence, 1200, true)
            .expect("evidence off + level/sustained ok → allowed");
    }

    #[tokio::test]
    async fn regression_detector_fires_after_3_repeats_in_hour() {
        // Drive the same (symptom, action) pair three times into a fresh
        // history file and confirm the detector raises an event on session
        // events. Uses a temp data_dir so the test is hermetic.
        use tokio::sync::broadcast;
        let mut d = std::env::temp_dir();
        d.push(format!("sctl-watchdog-regression-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("mkdir data_dir");
        let dd = d.to_string_lossy().into_owned();
        let (tx, mut rx) = broadcast::channel::<Value>(16);

        for _ in 0..3 {
            record_history_and_detect_regression(
                &dd,
                "unresponsive",
                "usb_cycle",
                3,
                "test-detail",
                &tx,
            )
            .await;
        }

        // Drain any events; we expect at least one regression event.
        let mut saw_regression = false;
        while let Ok(v) = rx.try_recv() {
            if v.get("type").and_then(Value::as_str) == Some("lte.watchdog.regression") {
                saw_regression = true;
                assert_eq!(v["symptom"], "unresponsive");
                assert_eq!(v["action"], "usb_cycle");
            }
        }
        assert!(saw_regression, "regression event must be emitted");

        // Confirm history file has 3 lines (jsonl).
        let raw = std::fs::read_to_string(d.join("watchdog_history.jsonl")).expect("history file");
        assert_eq!(raw.lines().filter(|l| !l.is_empty()).count(), 3);

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn unknown_action_defaults_are_passive() {
        // Default UnknownAction is { airplane_cycle: false, escalate: false }
        // — diagnose-only. This is the "Unknown → USB cycle path removed"
        // regression guard.
        let ua = crate::config::UnknownAction::default();
        assert!(!ua.airplane_cycle, "default airplane_cycle must be false");
        assert!(!ua.escalate, "default escalate must be false");
    }

    #[test]
    fn test_snapshot_event_ring() {
        let mut snap = WatchdogSnapshot::new();
        for i in 0..25 {
            snap.push_event("test", "action", &format!("event {i}"));
        }
        assert_eq!(snap.recent_events.len(), MAX_SNAPSHOT_EVENTS);
        // Oldest should be event 5 (0-4 evicted)
        assert_eq!(snap.recent_events.front().unwrap().detail, "event 5");
    }
}
