//! Autonomous LTE modem recovery.
//!
//! **First, do no harm.** The modem and netifd usually recover on their own;
//! this watchdog only intervenes when it is certain the modem is genuinely
//! stuck, and it picks the action matching the *specific* fault instead of
//! climbing a blind escalation ladder. It acts ONLY when the tunnel is down,
//! the client is not mid-reconnect, and grace has elapsed.
//!
//! Three properties it guarantees:
//!
//! * **Never make things worse.** Every destructive action is gated by grace
//!   periods, per-episode caps, cooldowns, and — for the USB power-cycle —
//!   opt-in level, sustained evidence, sysfs presence, exponential backoff, and
//!   a dormant fuse after repeated failures. It never fights the operator and
//!   never touches band configuration.
//! * **Don't leave the device in a bad state.** The airplane-mode cycle always
//!   confirms `CFUN=1` before returning, and on startup the watchdog forces
//!   `CFUN=1` and re-authorizes the modem's USB port to heal an action that a
//!   crash/power-loss may have interrupted.
//! * **Recover from catastrophe.** As a last resort it can USB power-cycle a
//!   wedged-but-present modem, then backs off and goes dormant rather than
//!   crash-looping on dead hardware.
//!
//! **It does not mistake weather for a fault.** A modem that is searching or
//! unregistered *with no usable RF* (a storm, a dead zone, a disconnected
//! antenna) is diagnosed `Environmental` and the watchdog waits — cycling the
//! modem cannot conjure signal and only restarts its own scan. Escalation
//! happens only when the modem reports usable signal yet stays stuck.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::comms::{CommsClient, CommsState};
use crate::config::LteConfig;
use crate::state::{TunnelEventType, TunnelStats};

// --- cadence ---
const TICK: Duration = Duration::from_secs(30);
/// Re-probe interval when the diagnosis is non-actionable (e.g. a live bearer
/// with a relay/upstream problem) — keeps watchdog AT off a working bearer.
const CALM_TICK: Duration = Duration::from_secs(300);
const DORMANT_TICK: Duration = Duration::from_secs(900);

// --- tunnel-stability resets ---
/// Tunnel up this long → clear the current episode's action counters.
const STABLE_LIGHT_RESET: Duration = Duration::from_secs(90);
/// Tunnel up this long → also clear the USB-cycle count and dormant fuse.
const STABLE_HEAVY_RESET: Duration = Duration::from_secs(300);

// --- per-episode action caps (thrash protection) ---
const MAX_REREGISTERS: u32 = 1;
const MAX_IFACE_RESTARTS: u32 = 2;
const MAX_AIRPLANE_CYCLES: u32 = 2;
/// USB power-cycles before the watchdog gives up and goes dormant.
const MAX_USB_CYCLES: u32 = 3;

// --- cooldowns between actions (let each one work before the next) ---
const COOLDOWN_REREGISTER: Duration = Duration::from_secs(60);
const COOLDOWN_MEDIUM: Duration = Duration::from_secs(120);
const COOLDOWN_USB_BASE: Duration = Duration::from_secs(300);
const COOLDOWN_USB_MAX: Duration = Duration::from_secs(1800);

/// Consecutive AT failures after which the modem is treated as hung and the
/// watchdog skips interface restarts straight to the (gated) USB cycle.
const AT_FAIL_TO_USB: u32 = 3;

/// Below this CSQ RSSI index there is no usable signal — environmental, not a
/// modem fault. Index 99 = "unknown" (no service). Index 5 ≈ −103 dBm.
const RSSI_USABLE_MIN: u8 = 5;

const HISTORY_MAX_BYTES: u64 = 5 * 1024 * 1024;
const SNAPSHOT_EVENTS: usize = 20;
const REGRESSION_WINDOW: Duration = Duration::from_secs(3600);
const REGRESSION_THRESHOLD: usize = 3;

/// What the watchdog believes is wrong with the link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Symptom {
    /// Searching/unregistered with no usable RF — storm, dead zone, antenna.
    /// The modem is doing its job; cycling cannot help. Wait.
    Environmental,
    /// `CEREG`=searching with usable RF, sustained — modem stuck scanning.
    SearchingStuck,
    /// `CEREG`=not-registered/denied with usable RF, sustained.
    NotRegistered,
    /// Registered (home/roam) but the kernel has no IPv4 — QMI bearer wedged.
    RegisteredNoData,
    /// AT unresponsive but the modem is still present in sysfs — firmware hung.
    Unresponsive,
    /// AT unresponsive and the modem has vanished from sysfs — hardware gone.
    ModemGone,
    /// Bearer up + relay reachable over the link — modem fine, relay/app fault.
    RelayProblem,
    /// Bearer up but the wider internet/DNS is unreachable — not the modem.
    InternetUnreachable,
    /// Client is mid-reconnect with a working bearer — let it finish.
    TunnelReconnecting,
    /// Evidence is ambiguous — diagnose and wait, never mutate.
    Unknown,
}

impl Symptom {
    fn as_str(self) -> &'static str {
        match self {
            Self::Environmental => "environmental",
            Self::SearchingStuck => "searching_stuck",
            Self::NotRegistered => "not_registered",
            Self::RegisteredNoData => "registered_no_data",
            Self::Unresponsive => "unresponsive",
            Self::ModemGone => "modem_gone",
            Self::RelayProblem => "relay_problem",
            Self::InternetUnreachable => "internet_unreachable",
            Self::TunnelReconnecting => "tunnel_reconnecting",
            Self::Unknown => "unknown",
        }
    }

    /// Whether this symptom ever warrants a recovery action. Everything else is
    /// diagnose-and-wait — the heart of "first, do no harm".
    fn actionable(self) -> bool {
        matches!(
            self,
            Self::SearchingStuck
                | Self::NotRegistered
                | Self::RegisteredNoData
                | Self::Unresponsive
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Reregister,
    Airplane,
    IfaceRestart,
    UsbCycle,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Self::Reregister => "reregister",
            Self::Airplane => "airplane_cycle",
            Self::IfaceRestart => "interface_restart",
            Self::UsbCycle => "usb_cycle",
        }
    }
}

/// Inputs to the (pure) diagnosis decision. Gathered by `probe`, classified by
/// `classify` so the whole decision table — including the storm case — is unit
/// testable without hardware.
#[derive(Debug, Clone, Copy)]
#[allow(clippy::struct_excessive_bools)] // a diagnosis snapshot, not a state flag soup
struct Probe {
    at_ok: bool,
    sysfs_present: bool,
    has_ipv4: bool,
    reconnecting: bool,
    /// `CEREG` stat field (0,1,2,3,4,5) when AT answered, else `None`.
    cereg_stat: Option<u8>,
    /// CSQ RSSI index (0..=31, or 99 = unknown) when AT answered.
    rssi: Option<u8>,
    /// Whether a single interface-bound ping reached the reachability target.
    relay_reachable: bool,
}

impl Probe {
    fn usable_signal(self) -> bool {
        self.rssi.is_some_and(|r| r != 99 && r >= RSSI_USABLE_MIN)
    }
}

/// Pure classification — the entire decision table. `relay_reachable` is only
/// consulted on the registered-with-IPv4 branch.
fn classify(p: Probe) -> Symptom {
    if !p.at_ok {
        return if p.sysfs_present {
            Symptom::Unresponsive
        } else {
            Symptom::ModemGone
        };
    }
    if p.reconnecting && p.has_ipv4 {
        return Symptom::TunnelReconnecting;
    }
    match p.cereg_stat {
        // Registered (home / roaming): trust the kernel bearer, never the AT
        // serving-cell field (the EC25 reports NOCONN with a live bearer).
        Some(1 | 5) => {
            if !p.has_ipv4 {
                Symptom::RegisteredNoData
            } else if p.relay_reachable {
                Symptom::RelayProblem
            } else {
                Symptom::InternetUnreachable
            }
        }
        // Searching: stuck only if there is signal to lock onto.
        Some(2) => {
            if p.usable_signal() {
                Symptom::SearchingStuck
            } else {
                Symptom::Environmental
            }
        }
        // Not registered / registration denied.
        Some(0 | 3) => {
            if p.usable_signal() {
                Symptom::NotRegistered
            } else {
                Symptom::Environmental
            }
        }
        _ => Symptom::Unknown,
    }
}

#[derive(Debug, Default)]
struct WatchdogState {
    dormant: bool,
    last_action_at: Option<Instant>,
    consecutive_at_failures: u32,
    // per-episode counters
    reregisters: u32,
    iface_restarts: u32,
    airplane_cycles: u32,
    usb_cycles: u32,
    // sustained-symptom tracking
    current_symptom: Option<Symptom>,
    symptom_since: Option<Instant>,
    // observability
    episode_actions: Vec<String>,
    last_symptom: Option<Symptom>,
    last_action: Option<String>,
    state_label: &'static str,
    events: VecDeque<Value>,
    action_history: VecDeque<(Instant, Symptom, Action)>,
}

impl WatchdogState {
    /// Clears the current episode's action counters once the tunnel has held.
    /// Preserves the USB-cycle count and dormant fuse (heavy reset clears those).
    fn light_reset(&mut self) {
        self.reregisters = 0;
        self.iface_restarts = 0;
        self.airplane_cycles = 0;
        self.consecutive_at_failures = 0;
        self.current_symptom = None;
        self.symptom_since = None;
        if !self.episode_actions.is_empty() {
            self.episode_actions.clear();
        }
    }

    fn heavy_reset(&mut self) {
        self.light_reset();
        self.usb_cycles = 0;
        self.dormant = false;
    }

    /// Track how long the current symptom has been continuously observed —
    /// the evidence the USB-cycle gate requires.
    fn track_symptom(&mut self, symptom: Symptom, now: Instant) {
        if self.current_symptom != Some(symptom) {
            self.current_symptom = Some(symptom);
            self.symptom_since = Some(now);
        }
    }

    fn sustained_secs(&self, now: Instant) -> u64 {
        self.symptom_since
            .map_or(0, |t| now.duration_since(t).as_secs())
    }

    fn cooldown_for(&self, action: Action) -> Duration {
        match action {
            Action::Reregister => COOLDOWN_REREGISTER,
            Action::Airplane | Action::IfaceRestart => COOLDOWN_MEDIUM,
            Action::UsbCycle => usb_backoff(self.usb_cycles),
        }
    }

    fn cooldown_elapsed(&self, action: Action, now: Instant) -> bool {
        self.last_action_at
            .is_none_or(|t| now.duration_since(t) >= self.cooldown_for(action))
    }

    fn note_action(&mut self, action: Action, symptom: Symptom, now: Instant) {
        self.last_action_at = Some(now);
        self.last_action = Some(action.as_str().to_string());
        self.episode_actions.push(action.as_str().to_string());
        match action {
            Action::Reregister => self.reregisters += 1,
            Action::IfaceRestart => self.iface_restarts += 1,
            Action::Airplane => self.airplane_cycles += 1,
            Action::UsbCycle => self.usb_cycles += 1,
        }
        self.action_history.push_back((now, symptom, action));
        while self
            .action_history
            .front()
            .is_some_and(|(t, _, _)| now.duration_since(*t) > REGRESSION_WINDOW)
        {
            self.action_history.pop_front();
        }
    }

    /// `true` if the same (symptom, action) pair has fired enough times in the
    /// window to look like a regression. Alert-only — never auto-escalates.
    fn is_regression(&self, symptom: Symptom, action: Action, now: Instant) -> bool {
        self.action_history
            .iter()
            .filter(|(t, s, a)| {
                *s == symptom && *a == action && now.duration_since(*t) <= REGRESSION_WINDOW
            })
            .count()
            >= REGRESSION_THRESHOLD
    }
}

/// Exponential backoff for repeated USB cycles: 5m, 10m, 20m, capped at 30m.
fn usb_backoff(usb_cycles: u32) -> Duration {
    let mult = 1u32 << usb_cycles.min(3);
    (COOLDOWN_USB_BASE * mult).min(COOLDOWN_USB_MAX)
}

/// Outcome of the USB-cycle gate.
enum UsbGate {
    Allow,
    Deny(&'static str),
}

/// Pure gate for the most destructive action. `set_dormant` is returned so the
/// caller can flip the fuse without this fn touching state.
fn usb_gate(cfg: &LteConfig, st: &WatchdogState, now: Instant) -> (UsbGate, bool) {
    if cfg.max_escalation_level < 4 {
        return (
            UsbGate::Deny("usb_cycle opt-in required (max_escalation_level < 4)"),
            false,
        );
    }
    if st.usb_cycles >= MAX_USB_CYCLES {
        return (UsbGate::Deny("usb attempts exhausted -> dormant"), true);
    }
    if st.sustained_secs(now) < cfg.usb_cycle_evidence.min_sustained_secs {
        return (UsbGate::Deny("insufficient sustained evidence"), false);
    }
    (UsbGate::Allow, false)
}

/// Pick the recovery action for an actionable symptom given the episode's
/// caps, or `None` to keep waiting. Pure — drives the escalation order.
fn choose_action(
    symptom: Symptom,
    st: &WatchdogState,
    cfg: &LteConfig,
    down_secs: u64,
) -> Option<Action> {
    match symptom {
        Symptom::RegisteredNoData => {
            if st.iface_restarts < MAX_IFACE_RESTARTS {
                Some(Action::IfaceRestart)
            } else if st.airplane_cycles < MAX_AIRPLANE_CYCLES {
                Some(Action::Airplane)
            } else {
                Some(Action::UsbCycle)
            }
        }
        Symptom::Unresponsive => {
            if st.consecutive_at_failures >= AT_FAIL_TO_USB {
                Some(Action::UsbCycle)
            } else if st.iface_restarts < MAX_IFACE_RESTARTS {
                Some(Action::IfaceRestart)
            } else {
                Some(Action::UsbCycle)
            }
        }
        Symptom::SearchingStuck => {
            if st.airplane_cycles < MAX_AIRPLANE_CYCLES {
                Some(Action::Airplane)
            } else {
                Some(Action::UsbCycle)
            }
        }
        Symptom::NotRegistered => {
            // Extra grace: brief handovers can read as NotRegistered.
            if down_secs < cfg.notregistered_grace_secs {
                None
            } else if st.reregisters < MAX_REREGISTERS {
                Some(Action::Reregister)
            } else if st.airplane_cycles < MAX_AIRPLANE_CYCLES {
                Some(Action::Airplane)
            } else {
                Some(Action::UsbCycle)
            }
        }
        _ => None,
    }
}

/// Spawn the watchdog task. Returns immediately; the loop runs until shutdown.
#[allow(clippy::too_many_arguments)]
pub fn spawn_watchdog(
    client: CommsClient,
    comms_state: Arc<Mutex<CommsState>>,
    tunnel_stats: Arc<TunnelStats>,
    cfg: LteConfig,
    data_dir: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Idempotent startup recovery: heal a CFUN=0 / deauthorized USB left by
        // an action a prior crash or power-loss interrupted. Always safe to run.
        startup_recovery(&client).await;

        let mut st = WatchdogState::default();
        let mut tunnel_down_since: Option<Instant> = None;
        let mut tunnel_up_since: Option<Instant> = None;

        loop {
            let now = Instant::now();
            let connected = tunnel_stats.connected.load(Ordering::Relaxed);

            if connected {
                tunnel_down_since = None;
                let up = *tunnel_up_since.get_or_insert(now);
                let up_secs = now.duration_since(up);
                if up_secs >= STABLE_HEAVY_RESET {
                    st.heavy_reset();
                } else if up_secs >= STABLE_LIGHT_RESET {
                    st.light_reset();
                }
                st.state_label = "connected";
                publish(&comms_state, &st, 0).await;
                tokio::time::sleep(TICK).await;
                continue;
            }

            tunnel_up_since = None;
            let down = *tunnel_down_since.get_or_insert(now);
            let down_secs = now.duration_since(down).as_secs();

            // Don't fight an in-progress reconnect that already has a bearer.
            if tunnel_stats.reconnecting.load(Ordering::Relaxed)
                && interface_has_ipv4(&cfg.interface)
            {
                st.state_label = "reconnecting";
                publish(&comms_state, &st, down_secs).await;
                tokio::time::sleep(TICK).await;
                continue;
            }

            if down_secs < cfg.watchdog_grace_secs {
                st.state_label = "grace";
                publish(&comms_state, &st, down_secs).await;
                tokio::time::sleep(TICK).await;
                continue;
            }

            // Diagnose, then act (the act fn enforces every gate).
            let probe = probe(&client, &tunnel_stats, &cfg).await;
            let symptom = classify(probe);
            st.track_symptom(symptom, now);
            st.last_symptom = Some(symptom);

            act(
                &client,
                &cfg,
                &mut st,
                symptom,
                probe,
                down_secs,
                now,
                &data_dir,
                &tunnel_stats,
            )
            .await;

            publish(&comms_state, &st, down_secs).await;

            // Cadence: dormant hardware is checked rarely; a non-actionable
            // diagnosis (often a live bearer with a relay/upstream problem) is
            // re-probed calmly so we don't run AT every 30s on a working bearer;
            // an active fault is watched closely.
            let interval = if st.dormant {
                DORMANT_TICK
            } else if symptom.actionable() {
                TICK
            } else {
                CALM_TICK
            };
            tokio::time::sleep(interval).await;
        }
    })
}

/// Gather the diagnosis inputs with the fewest possible AT commands (AT, CSQ,
/// CEREG) — and only while the tunnel is down, so a live bearer is never
/// disturbed.
async fn probe(client: &CommsClient, tunnel_stats: &TunnelStats, cfg: &LteConfig) -> Probe {
    let at_ok = client
        .at_command("AT", Duration::from_secs(3))
        .await
        .is_ok_and(|r| r.contains("OK"));

    if !at_ok {
        return Probe {
            at_ok: false,
            sysfs_present: quectel_present_in_sysfs(),
            has_ipv4: interface_has_ipv4(&cfg.interface),
            reconnecting: tunnel_stats.reconnecting.load(Ordering::Relaxed),
            cereg_stat: None,
            rssi: None,
            relay_reachable: false,
        };
    }

    let rssi = client
        .at_command("AT+CSQ", Duration::from_secs(3))
        .await
        .ok()
        .and_then(|r| parse_csq_rssi(&r));
    let cereg_stat = client
        .at_command("AT+CEREG?", Duration::from_secs(3))
        .await
        .ok()
        .and_then(|r| parse_cereg_stat(&r));

    let has_ipv4 = interface_has_ipv4(&cfg.interface);
    // The reachability ping only matters on the registered+IPv4 branch; skip it
    // otherwise to avoid needless probes.
    let relay_reachable = if has_ipv4 && matches!(cereg_stat, Some(1 | 5)) {
        let target = cfg.reachability_host.as_deref().unwrap_or("8.8.8.8");
        reachable(&cfg.interface, target)
    } else {
        false
    };

    Probe {
        at_ok: true,
        sysfs_present: true,
        has_ipv4,
        reconnecting: tunnel_stats.reconnecting.load(Ordering::Relaxed),
        cereg_stat,
        rssi,
        relay_reachable,
    }
}

#[allow(clippy::too_many_arguments)]
async fn act(
    client: &CommsClient,
    cfg: &LteConfig,
    st: &mut WatchdogState,
    symptom: Symptom,
    probe: Probe,
    down_secs: u64,
    now: Instant,
    data_dir: &str,
    tunnel_stats: &TunnelStats,
) {
    if !symptom.actionable() {
        if symptom == Symptom::ModemGone && !st.dormant {
            st.dormant = true;
            warn!("lte watchdog: modem absent from sysfs — going dormant");
            record(st, data_dir, symptom, None, "modem_gone_dormant", probe).await;
        }
        st.state_label = match symptom {
            Symptom::Environmental => "waiting_no_signal",
            Symptom::ModemGone => "dormant",
            Symptom::TunnelReconnecting => "reconnecting",
            _ => "waiting_not_actionable",
        };
        return;
    }

    let Some(action) = choose_action(symptom, st, cfg, down_secs) else {
        st.state_label = "grace";
        return;
    };

    if !st.cooldown_elapsed(action, now) {
        st.state_label = "cooldown";
        return;
    }

    if action == Action::UsbCycle {
        let (gate, set_dormant) = usb_gate(cfg, st, now);
        if set_dormant {
            st.dormant = true;
        }
        if let UsbGate::Deny(reason) = gate {
            warn!(
                "lte watchdog: USB cycle declined for {}: {reason}",
                symptom.as_str()
            );
            record(st, data_dir, symptom, None, reason, probe).await;
            st.state_label = if st.dormant {
                "dormant"
            } else {
                "waiting_gated"
            };
            return;
        }
    }

    st.state_label = "acting";
    info!(
        "lte watchdog: {} for {} (down {}s, sustained {}s)",
        action.as_str(),
        symptom.as_str(),
        down_secs,
        st.sustained_secs(now)
    );

    let detail = execute(client, cfg, action).await;
    st.note_action(action, symptom, now);

    if st.is_regression(symptom, action, now) {
        warn!(
            "lte watchdog REGRESSION: {} -> {} fired >= {} times in the last hour",
            symptom.as_str(),
            action.as_str(),
            REGRESSION_THRESHOLD
        );
    }

    record(st, data_dir, symptom, Some(action), &detail, probe).await;
    tunnel_stats
        .push_event(
            TunnelEventType::WatchdogAction,
            format!("{}: {} ({detail})", symptom.as_str(), action.as_str()),
        )
        .await;
}

/// Execute one recovery action. Returns a short human-readable outcome.
async fn execute(client: &CommsClient, cfg: &LteConfig, action: Action) -> String {
    match action {
        Action::Reregister => match client
            .at_command("AT+COPS=0", Duration::from_secs(10))
            .await
        {
            Ok(_) => "AT+COPS=0 ok".to_string(),
            Err(e) => format!("AT+COPS=0 failed: {e}"),
        },
        Action::Airplane => airplane_cycle(client).await,
        Action::IfaceRestart => restart_interface(cfg).await,
        Action::UsbCycle => match client
            .call(
                sctl_comms_abi::methods::RECOVERY_USB_CYCLE,
                serde_json::Value::Null,
            )
            .await
        {
            // The plugin reopens the AT port after re-enumeration.
            Ok(_) => "usb power-cycle ok".to_string(),
            Err(e) => format!("usb power-cycle failed: {e}"),
        },
    }
}

/// Airplane-mode cycle that NEVER leaves the modem in airplane mode: after
/// `CFUN=0`/`CFUN=1` it confirms `CFUN=1` and retries the restore on timeout.
async fn airplane_cycle(client: &CommsClient) -> String {
    let _ = client
        .at_command("AT+CFUN=0", Duration::from_secs(10))
        .await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    for attempt in 0..3 {
        let _ = client
            .at_command("AT+CFUN=1", Duration::from_secs(15))
            .await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        if modem_full_functionality(client).await {
            return if attempt == 0 {
                "airplane cycle ok".to_string()
            } else {
                format!("airplane cycle ok (CFUN=1 restored on retry {attempt})")
            };
        }
    }
    "airplane cycle: CFUN=1 not confirmed".to_string()
}

/// Query `AT+CFUN?` and report whether the modem is at full functionality (1).
async fn modem_full_functionality(client: &CommsClient) -> bool {
    client
        .at_command("AT+CFUN?", Duration::from_secs(5))
        .await
        .ok()
        .and_then(|r| parse_cfun(&r))
        .is_some_and(|v| v == 1)
}

/// On startup, restore any state an interrupted action could have left bad.
/// Both steps are idempotent and safe to run on a healthy modem.
async fn startup_recovery(client: &CommsClient) {
    // 1. A crash between CFUN=0 and CFUN=1 leaves the modem in airplane mode.
    if let Ok(resp) = client.at_command("AT+CFUN?", Duration::from_secs(5)).await {
        if parse_cfun(&resp).is_some_and(|v| v != 1) {
            warn!("lte watchdog: modem at reduced functionality on startup — forcing CFUN=1");
            let _ = client
                .at_command("AT+CFUN=1", Duration::from_secs(15))
                .await;
        }
    }
    // 2. A crash between deauthorize and reauthorize leaves the USB port off.
    reauthorize_usb();
}

// --- kernel-truth helpers (server-side, no AT) ---

fn interface_has_ipv4(iface: &str) -> bool {
    std::process::Command::new("ip")
        .args(["-4", "addr", "show", "dev", iface])
        .output()
        .is_ok_and(|o| o.status.success() && !o.stdout.is_empty())
}

/// Interface-bound single ping — measures *this* link, not whatever WAN the
/// tunnel might currently ride.
fn reachable(iface: &str, target: &str) -> bool {
    std::process::Command::new("ping")
        .args(["-c", "1", "-W", "3", "-I", iface, target])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Whether a Quectel modem (USB vendor `2c7c`) is still enumerated.
fn quectel_present_in_sysfs() -> bool {
    let Ok(entries) = std::fs::read_dir("/sys/bus/usb/devices") else {
        return false;
    };
    for entry in entries.flatten() {
        let vid = entry.path().join("idVendor");
        if let Ok(contents) = std::fs::read_to_string(&vid) {
            if contents.trim().eq_ignore_ascii_case("2c7c") {
                return true;
            }
        }
    }
    false
}

/// Best-effort: write `1` to the `authorized` node of any deauthorized Quectel
/// USB device, healing an interrupted USB cycle.
fn reauthorize_usb() {
    let Ok(entries) = std::fs::read_dir("/sys/bus/usb/devices") else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_quectel = std::fs::read_to_string(path.join("idVendor"))
            .is_ok_and(|s| s.trim().eq_ignore_ascii_case("2c7c"));
        if !is_quectel {
            continue;
        }
        let auth = path.join("authorized");
        if std::fs::read_to_string(&auth).is_ok_and(|s| s.trim() == "0") {
            warn!("lte watchdog: reauthorizing {} on startup", auth.display());
            let _ = std::fs::write(&auth, b"1");
        }
    }
}

/// Restart the LTE interface: custom command, then OpenWrt ifdown/ifup, then a
/// generic `ip link` bounce.
async fn restart_interface(cfg: &LteConfig) -> String {
    let iface = cfg.interface.clone();
    let custom = cfg.interface_restart_cmd.clone();
    tokio::task::spawn_blocking(move || {
        if let Some(cmd) = custom {
            let ok = std::process::Command::new("sh")
                .args(["-c", &cmd])
                .status()
                .is_ok_and(|s| s.success());
            return format!(
                "custom interface_restart_cmd: {}",
                if ok { "ok" } else { "failed" }
            );
        }
        // OpenWrt logical interface (strip a trailing index, e.g. wwan0 -> wwan).
        let logical = iface.trim_end_matches(|c: char| c.is_ascii_digit());
        let ifdown = std::process::Command::new("ifdown").arg(logical).status();
        if ifdown.is_ok_and(|s| s.success()) {
            std::thread::sleep(Duration::from_secs(2));
            let ok = std::process::Command::new("ifup")
                .arg(logical)
                .status()
                .is_ok_and(|s| s.success());
            return format!(
                "ifdown/ifup {logical}: {}",
                if ok { "ok" } else { "ifup failed" }
            );
        }
        // Generic fallback.
        let _ = std::process::Command::new("ip")
            .args(["link", "set", &iface, "down"])
            .status();
        std::thread::sleep(Duration::from_secs(2));
        let ok = std::process::Command::new("ip")
            .args(["link", "set", &iface, "up"])
            .status()
            .is_ok_and(|s| s.success());
        format!(
            "ip link bounce {iface}: {}",
            if ok { "ok" } else { "failed" }
        )
    })
    .await
    .unwrap_or_else(|e| format!("interface restart join error: {e}"))
}

// --- AT response parsing (pure) ---

/// `+CEREG: <n>,<stat>[,...]` → stat. Handles short and extended forms.
fn parse_cereg_stat(resp: &str) -> Option<u8> {
    let line = resp.lines().find(|l| l.contains("+CEREG:"))?;
    let after = line.split(':').nth(1)?;
    let mut fields = after.split(',').map(str::trim);
    let _n = fields.next()?;
    fields.next()?.trim_matches('"').parse::<u8>().ok()
}

/// `+CSQ: <rssi>,<ber>` → rssi index (0..=31 or 99).
fn parse_csq_rssi(resp: &str) -> Option<u8> {
    let line = resp.lines().find(|l| l.contains("+CSQ:"))?;
    let after = line.split(':').nth(1)?;
    after.split(',').next()?.trim().parse::<u8>().ok()
}

/// `+CFUN: <fun>` → functionality level.
fn parse_cfun(resp: &str) -> Option<u8> {
    let line = resp.lines().find(|l| l.contains("+CFUN:"))?;
    let after = line.split(':').nth(1)?;
    after.trim().split(',').next()?.trim().parse::<u8>().ok()
}

// --- observability ---

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Append to `watchdog_history.jsonl` (rotating) and push a snapshot event.
async fn record(
    st: &mut WatchdogState,
    data_dir: &str,
    symptom: Symptom,
    action: Option<Action>,
    detail: &str,
    probe: Probe,
) {
    let event = json!({
        "ts": now_unix(),
        "symptom": symptom.as_str(),
        "action": action.map(Action::as_str),
        "detail": detail,
        "rssi": probe.rssi,
        "cereg_stat": probe.cereg_stat,
        "has_ipv4": probe.has_ipv4,
    });
    st.events.push_back(event.clone());
    while st.events.len() > SNAPSHOT_EVENTS {
        st.events.pop_front();
    }

    let path = PathBuf::from(data_dir).join("watchdog_history.jsonl");
    if let Ok(line) = serde_json::to_string(&event) {
        if let Err(e) = crate::util::append_rotating(&path, &line, HISTORY_MAX_BYTES).await {
            warn!("lte watchdog: history write failed: {e}");
        }
    }
}

/// Write the live snapshot into `CommsState.watchdog` for `/api/lte`.
async fn publish(comms_state: &Arc<Mutex<CommsState>>, st: &WatchdogState, disconnect_secs: u64) {
    let snapshot = json!({
        "state": st.state_label,
        "dormant": st.dormant,
        "disconnect_secs": disconnect_secs,
        "last_symptom": st.last_symptom.map(Symptom::as_str),
        "last_action": st.last_action,
        "usb_cycles": st.usb_cycles,
        "episode_actions": st.episode_actions,
        "events": st.events.iter().cloned().collect::<Vec<_>>(),
    });
    comms_state.lock().await.watchdog = Some(snapshot);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_probe() -> Probe {
        Probe {
            at_ok: true,
            sysfs_present: true,
            has_ipv4: false,
            reconnecting: false,
            cereg_stat: Some(2),
            rssi: Some(20),
            relay_reachable: false,
        }
    }

    fn cfg() -> LteConfig {
        // Deserialize an empty table to get all defaults.
        toml::from_str("").unwrap()
    }

    #[test]
    fn searching_with_signal_is_stuck_but_without_signal_is_environmental() {
        let stuck = Probe {
            cereg_stat: Some(2),
            rssi: Some(18),
            ..base_probe()
        };
        assert_eq!(classify(stuck), Symptom::SearchingStuck);

        // Storm / dead zone: searching, no usable RF -> never act.
        let storm = Probe {
            cereg_stat: Some(2),
            rssi: Some(99),
            ..base_probe()
        };
        assert_eq!(classify(storm), Symptom::Environmental);
        assert!(!classify(storm).actionable());

        let weak = Probe {
            cereg_stat: Some(2),
            rssi: Some(2),
            ..base_probe()
        };
        assert_eq!(classify(weak), Symptom::Environmental);
    }

    #[test]
    fn not_registered_needs_signal_to_be_actionable() {
        let no_sig = Probe {
            cereg_stat: Some(0),
            rssi: None,
            ..base_probe()
        };
        assert_eq!(classify(no_sig), Symptom::Environmental);

        let with_sig = Probe {
            cereg_stat: Some(0),
            rssi: Some(15),
            ..base_probe()
        };
        assert_eq!(classify(with_sig), Symptom::NotRegistered);
    }

    #[test]
    fn registered_uses_kernel_truth_not_at_field() {
        // Registered, no IPv4 -> bearer wedged.
        let no_data = Probe {
            cereg_stat: Some(1),
            has_ipv4: false,
            ..base_probe()
        };
        assert_eq!(classify(no_data), Symptom::RegisteredNoData);

        // Registered, IPv4, relay reachable -> not the modem's problem.
        let relay = Probe {
            cereg_stat: Some(5),
            has_ipv4: true,
            relay_reachable: true,
            ..base_probe()
        };
        assert_eq!(classify(relay), Symptom::RelayProblem);
        assert!(!classify(relay).actionable());

        // Registered, IPv4, target unreachable -> upstream/DNS, not the modem.
        let inet = Probe {
            cereg_stat: Some(1),
            has_ipv4: true,
            relay_reachable: false,
            ..base_probe()
        };
        assert_eq!(classify(inet), Symptom::InternetUnreachable);
        assert!(!classify(inet).actionable());
    }

    #[test]
    fn at_silence_splits_on_sysfs_presence() {
        let hung = Probe {
            at_ok: false,
            sysfs_present: true,
            ..base_probe()
        };
        assert_eq!(classify(hung), Symptom::Unresponsive);

        let gone = Probe {
            at_ok: false,
            sysfs_present: false,
            ..base_probe()
        };
        assert_eq!(classify(gone), Symptom::ModemGone);
        assert!(!classify(gone).actionable());
    }

    #[test]
    fn reconnecting_with_bearer_is_left_alone() {
        let p = Probe {
            reconnecting: true,
            has_ipv4: true,
            cereg_stat: Some(1),
            ..base_probe()
        };
        assert_eq!(classify(p), Symptom::TunnelReconnecting);
        assert!(!classify(p).actionable());
    }

    #[test]
    fn unknown_cereg_is_diagnose_only() {
        let p = Probe {
            cereg_stat: Some(4),
            ..base_probe()
        };
        assert_eq!(classify(p), Symptom::Unknown);
        assert!(!classify(p).actionable());
    }

    #[test]
    fn usb_cycle_is_opt_in_and_evidence_gated() {
        let now = Instant::now();
        let mut c = cfg();

        // Default max_escalation_level = 3 -> USB never auto.
        let st = WatchdogState::default();
        assert!(matches!(usb_gate(&c, &st, now).0, UsbGate::Deny(_)));

        // Opt-in but no sustained evidence yet.
        c.max_escalation_level = 4;
        let mut st = WatchdogState {
            symptom_since: Some(now),
            ..WatchdogState::default()
        };
        assert!(matches!(usb_gate(&c, &st, now).0, UsbGate::Deny(_)));

        // Opt-in + sustained past the evidence window -> allowed.
        st.symptom_since = now.checked_sub(Duration::from_secs(
            c.usb_cycle_evidence.min_sustained_secs + 1,
        ));
        assert!(matches!(usb_gate(&c, &st, now).0, UsbGate::Allow));

        // Exhausted attempts -> deny + dormant fuse.
        st.usb_cycles = MAX_USB_CYCLES;
        let (gate, dormant) = usb_gate(&c, &st, now);
        assert!(matches!(gate, UsbGate::Deny(_)));
        assert!(dormant);
    }

    #[test]
    fn escalation_respects_episode_caps() {
        let c = cfg();
        let mut st = WatchdogState::default();
        // RegisteredNoData: iface restart until cap, then airplane, then USB.
        assert_eq!(
            choose_action(Symptom::RegisteredNoData, &st, &c, 9999),
            Some(Action::IfaceRestart)
        );
        st.iface_restarts = MAX_IFACE_RESTARTS;
        assert_eq!(
            choose_action(Symptom::RegisteredNoData, &st, &c, 9999),
            Some(Action::Airplane)
        );
        st.airplane_cycles = MAX_AIRPLANE_CYCLES;
        assert_eq!(
            choose_action(Symptom::RegisteredNoData, &st, &c, 9999),
            Some(Action::UsbCycle)
        );
    }

    #[test]
    fn not_registered_honors_extra_grace() {
        let c = cfg();
        let st = WatchdogState::default();
        // Within notregistered grace: wait.
        assert_eq!(
            choose_action(
                Symptom::NotRegistered,
                &st,
                &c,
                c.notregistered_grace_secs - 1
            ),
            None
        );
        // Past it: re-register first.
        assert_eq!(
            choose_action(
                Symptom::NotRegistered,
                &st,
                &c,
                c.notregistered_grace_secs + 1
            ),
            Some(Action::Reregister)
        );
    }

    #[test]
    fn unresponsive_jumps_to_usb_after_repeated_at_failures() {
        let c = cfg();
        let st = WatchdogState {
            consecutive_at_failures: AT_FAIL_TO_USB,
            ..WatchdogState::default()
        };
        assert_eq!(
            choose_action(Symptom::Unresponsive, &st, &c, 9999),
            Some(Action::UsbCycle)
        );
    }

    #[test]
    fn non_actionable_symptoms_choose_nothing() {
        let c = cfg();
        let st = WatchdogState::default();
        for s in [
            Symptom::Environmental,
            Symptom::RelayProblem,
            Symptom::InternetUnreachable,
            Symptom::TunnelReconnecting,
            Symptom::ModemGone,
            Symptom::Unknown,
        ] {
            assert_eq!(choose_action(s, &st, &c, 9999), None);
        }
    }

    #[test]
    fn usb_backoff_grows_then_caps() {
        assert_eq!(usb_backoff(0), COOLDOWN_USB_BASE);
        assert_eq!(usb_backoff(1), COOLDOWN_USB_BASE * 2);
        assert_eq!(usb_backoff(2), COOLDOWN_USB_BASE * 4);
        assert_eq!(usb_backoff(10), COOLDOWN_USB_MAX);
    }

    #[test]
    fn parsers_handle_short_and_extended_forms() {
        assert_eq!(parse_cereg_stat("+CEREG: 0,1\r\nOK"), Some(1));
        assert_eq!(
            parse_cereg_stat("+CEREG: 2,5,\"A1B2\",\"0123ABCD\",7\r\nOK"),
            Some(5)
        );
        assert_eq!(parse_cereg_stat("OK"), None);
        assert_eq!(parse_csq_rssi("+CSQ: 20,99\r\nOK"), Some(20));
        assert_eq!(parse_csq_rssi("+CSQ: 99,99"), Some(99));
        assert_eq!(parse_cfun("+CFUN: 1\r\nOK"), Some(1));
        assert_eq!(parse_cfun("+CFUN: 0"), Some(0));
    }

    #[test]
    fn regression_detector_alerts_after_threshold() {
        let now = Instant::now();
        let mut st = WatchdogState::default();
        for _ in 0..(REGRESSION_THRESHOLD - 1) {
            st.note_action(Action::UsbCycle, Symptom::Unresponsive, now);
        }
        assert!(!st.is_regression(Symptom::Unresponsive, Action::UsbCycle, now));
        st.note_action(Action::UsbCycle, Symptom::Unresponsive, now);
        assert!(st.is_regression(Symptom::Unresponsive, Action::UsbCycle, now));
        // A different pair is unaffected.
        assert!(!st.is_regression(Symptom::RegisteredNoData, Action::IfaceRestart, now));
    }

    #[test]
    fn heavy_reset_clears_usb_and_dormant_light_reset_does_not() {
        let mut st = WatchdogState {
            usb_cycles: 2,
            dormant: true,
            iface_restarts: 2,
            ..WatchdogState::default()
        };
        st.light_reset();
        assert_eq!(st.iface_restarts, 0);
        assert_eq!(st.usb_cycles, 2);
        assert!(st.dormant);
        st.heavy_reset();
        assert_eq!(st.usb_cycles, 0);
        assert!(!st.dormant);
    }
}
