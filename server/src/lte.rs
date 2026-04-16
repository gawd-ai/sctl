//! LTE signal quality monitoring via Quectel modem AT commands.
//!
//! When `[lte]` is present in the config, a background poller queries the modem
//! for signal strength, serving cell info, network info, and operator name
//! at the configured interval, storing results in [`LteState`].
//!
//! Static modem identity (IMEI, model, firmware, ICCID) is read once at startup.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, watch, Mutex, Notify};
use tracing::{debug, info, warn};

use crate::config::LteConfig;
use crate::lte_watchdog::parse_cereg;
use crate::modem::Modem;
use crate::state::TunnelStats;

/// Static modem identity — read once at startup.
#[derive(Debug, Clone, Serialize)]
pub struct ModemInfo {
    /// Modem model (e.g. "EC25", from AT+CGMM).
    pub model: Option<String>,
    /// Firmware version (e.g. "EC25AFFAR07A14M4G", from AT+CGMR).
    pub firmware: Option<String>,
    /// IMEI (from AT+GSN).
    pub imei: Option<String>,
    /// SIM ICCID (from AT+QCCID).
    pub iccid: Option<String>,
    /// IMSI (from AT+CIMI) — used for APN auto-detection via MCC+MNC prefix.
    pub imsi: Option<String>,
}

/// A neighbor cell detected by the modem.
#[derive(Debug, Clone, Serialize)]
pub struct NeighborCell {
    pub earfcn: u32,
    pub pci: u16,
    pub rsrp: Option<i32>,
    pub rsrq: Option<i32>,
    pub rssi: Option<i32>,
    pub sinr: Option<f64>,
    pub cell_type: String,
}

/// Modem band configuration (from AT+QCFG).
#[derive(Debug, Clone, Serialize)]
pub struct BandConfig {
    /// Enabled LTE bands (e.g. [4, 12, 13]).
    pub enabled_bands: Vec<u16>,
    /// Priority band number (from AT+QCFG="bandpri").
    pub priority_band: Option<u16>,
}

/// A single signal observation on a specific band.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BandObservation {
    pub rsrp: i32,
    pub rsrq: Option<i32>,
    pub sinr: Option<f64>,
    pub pci: u16,
    pub recorded_at: u64,
    pub serving: bool,
}

/// Accumulated per-band signal history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BandHistoryEntry {
    pub band: u16,
    pub best_rsrp: i32,
    pub latest_rsrp: i32,
    pub observation_count: u64,
    pub last_seen: u64,
    pub recent: VecDeque<BandObservation>,
}

/// Maximum recent observations per band.
const MAX_BAND_OBSERVATIONS: usize = 20;

/// Status of a running or completed band scan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanStatus {
    pub state: String,
    pub started_at: u64,
    pub completed_at: Option<u64>,
    pub bands_to_scan: Vec<u16>,
    pub bands_scanned: Vec<u16>,
    pub current_band: Option<u16>,
    pub results: Vec<ScanBandResult>,
    pub original_bands: Vec<u16>,
    pub original_priority: Option<u16>,
}

/// Per-band result from a band scan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanBandResult {
    pub band: u16,
    pub registered: bool,
    pub registration_time_ms: u64,
    pub rsrp: Option<i32>,
    pub rsrq: Option<i32>,
    pub sinr: Option<f64>,
    pub download_bps: Option<u64>,
    pub upload_bps: Option<u64>,
}

/// LTE signal quality snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct LteSignal {
    /// RSSI in dBm (from AT+CSQ).
    pub rssi_dbm: i32,
    /// Reference Signal Received Power (from AT+QENG).
    pub rsrp: Option<i32>,
    /// Reference Signal Received Quality (from AT+QENG).
    pub rsrq: Option<i32>,
    /// Signal-to-Interference-plus-Noise Ratio (from AT+QENG).
    pub sinr: Option<f64>,
    /// LTE band (e.g. "B4", from AT+QNWINFO).
    pub band: Option<String>,
    /// Network operator name (from AT+COPS?).
    pub operator: Option<String>,
    /// Access technology (e.g. "LTE", from AT+QNWINFO).
    pub technology: Option<String>,
    /// Cell ID (hex, from AT+QENG).
    pub cell_id: Option<String>,
    /// Physical Cell ID (0-503, from AT+QENG).
    pub pci: Option<u16>,
    /// E-UTRA Absolute Radio Frequency Channel Number (from AT+QENG).
    pub earfcn: Option<u32>,
    /// LTE frequency band number (from AT+QENG).
    pub freq_band: Option<u16>,
    /// Tracking Area Code (hex, from AT+QENG).
    pub tac: Option<String>,
    /// PLMN (MCC+MNC, e.g. "302720", from AT+QENG).
    pub plmn: Option<String>,
    /// eNodeB ID (cell_id >> 8).
    pub enodeb_id: Option<u32>,
    /// Sector ID (cell_id & 0xFF).
    pub sector: Option<u8>,
    /// Uplink bandwidth in MHz (from AT+QENG).
    pub ul_bw_mhz: Option<String>,
    /// Downlink bandwidth in MHz (from AT+QENG).
    pub dl_bw_mhz: Option<String>,
    /// Connection state (NOCONN/CONNECT/SEARCH/LIMSRV, from AT+QENG).
    pub connection_state: Option<String>,
    /// Duplex mode (FDD/TDD, from AT+QENG).
    pub duplex: Option<String>,
    /// Neighbor cells (from AT+QENG="neighbourcell").
    pub neighbors: Vec<NeighborCell>,
    /// Band configuration (enabled bands + priority).
    pub band_config: Option<BandConfig>,
    /// Signal quality as 1-5 bars, derived from RSRP (or RSSI fallback).
    pub signal_bars: u8,
    /// When this reading was recorded (epoch seconds).
    pub recorded_at: u64,
}

/// Minimum RSRP (dBm) required for safe-bands promotion.
/// Configs with signal below this are "marginal" and won't overwrite a better safe config.
const SAFE_BANDS_MIN_RSRP: i32 = -110;

/// Band config that last sustained tunnel connectivity — persisted to `{data_dir}/safe_bands.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafeBandConfig {
    pub bands: Vec<u16>,
    pub priority_band: Option<u16>,
    /// Epoch seconds when tunnel was confirmed stable on this config.
    pub confirmed_at: u64,
    /// RSRP at promotion time (dBm), for comparing signal quality between configs.
    #[serde(default)]
    pub signal_rsrp: Option<i32>,
}

/// Distinguishes user-initiated band changes from watchdog reverts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BandChangeSource {
    /// From `/api/lte/bands` endpoint.
    User,
    /// From watchdog safe-bands revert.
    Watchdog,
}

/// Persisted LTE data — scan results + band history.
/// Written to `{data_dir}/lte_data.json` on scan completion and poller updates.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedLteData {
    scan_status: Option<ScanStatus>,
    band_history: Vec<BandHistoryEntry>,
}

/// Shared LTE state updated by the background poller.
#[derive(Default)]
pub struct LteState {
    pub modem: Option<ModemInfo>,
    pub signal: Option<LteSignal>,
    pub errors_total: u64,
    pub last_error: Option<String>,
    pub band_history: HashMap<u16, BandHistoryEntry>,
    pub scan_status: Option<ScanStatus>,
    /// Suppresses watchdog actions until this instant (set during manual band changes).
    pub band_action_until: Option<Instant>,
    /// Band config that last sustained tunnel connectivity for 5+ min.
    pub safe_bands: Option<SafeBandConfig>,
    /// Bands that were active when tunnel last connected (for post-change detection).
    pub bands_at_connect: Option<Vec<u16>>,
    /// When the current band config first had a stable tunnel (for promotion timing).
    pub band_stable_since: Option<Instant>,
    /// When the last band change occurred (user or auto, NOT watchdog revert).
    pub last_band_change_at: Option<Instant>,
    /// Bands before the most recent user-initiated change (for quick revert).
    /// Tuple: (bands, priority_band).
    pub pre_change_bands: Option<(Vec<u16>, Option<u16>)>,
    /// True while a background registration monitor is running after a fast band change.
    pub registration_pending: bool,
    /// Last time a user performed an LTE action (band change, scan, etc.).
    /// Watchdog suppresses all actions for 120s after user activity.
    pub last_user_action_at: Option<Instant>,
}

impl LteState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Load safe bands from `{data_dir}/safe_bands.json`. Silent on missing file.
    pub fn load_safe_bands(&mut self, data_dir: &str) {
        let path = std::path::Path::new(data_dir).join("safe_bands.json");
        match std::fs::read_to_string(&path) {
            Ok(contents) => match serde_json::from_str::<SafeBandConfig>(&contents) {
                Ok(config) => {
                    info!(
                        "Loaded safe bands: B{} (confirmed {}s ago)",
                        config
                            .bands
                            .iter()
                            .map(|b| b.to_string())
                            .collect::<Vec<_>>()
                            .join(",B"),
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs()
                            .saturating_sub(config.confirmed_at)
                    );
                    self.safe_bands = Some(config);
                }
                Err(e) => warn!("Failed to parse safe_bands.json: {e}"),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!("Failed to read safe_bands.json: {e}"),
        }
    }

    /// Persist current safe bands to `{data_dir}/safe_bands.json` (atomic: tmp + rename).
    pub fn save_safe_bands(&self, data_dir: &str) {
        let Some(ref config) = self.safe_bands else {
            return;
        };
        let path = std::path::Path::new(data_dir).join("safe_bands.json");
        let tmp = path.with_extension("json.tmp");
        let Ok(json) = serde_json::to_string_pretty(config) else {
            warn!("Failed to serialize safe bands");
            return;
        };
        if let Err(e) = std::fs::write(&tmp, &json) {
            warn!("Failed to write safe_bands.json.tmp: {e}");
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, &path) {
            warn!("Failed to rename safe_bands.json.tmp: {e}");
        }
    }

    /// Load persisted LTE data (scan results, band history) from `{data_dir}/lte_data.json`.
    pub fn load_lte_data(&mut self, data_dir: &str) {
        let path = std::path::Path::new(data_dir).join("lte_data.json");
        match std::fs::read_to_string(&path) {
            Ok(contents) => match serde_json::from_str::<PersistedLteData>(&contents) {
                Ok(data) => {
                    if let Some(scan) = data.scan_status {
                        info!(
                            "Loaded scan results: {} bands, state={}",
                            scan.results.len(),
                            scan.state
                        );
                        self.scan_status = Some(scan);
                    }
                    if !data.band_history.is_empty() {
                        info!("Loaded band history: {} bands", data.band_history.len());
                        for entry in data.band_history {
                            self.band_history.insert(entry.band, entry);
                        }
                    }
                }
                Err(e) => warn!("Failed to parse lte_data.json: {e}"),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!("Failed to read lte_data.json: {e}"),
        }
    }

    /// Persist scan results and band history to `{data_dir}/lte_data.json` (atomic: tmp + rename).
    pub fn save_lte_data(&self, data_dir: &str) {
        let data = PersistedLteData {
            scan_status: self.scan_status.clone(),
            band_history: self.band_history.values().cloned().collect(),
        };
        let path = std::path::Path::new(data_dir).join("lte_data.json");
        let tmp = path.with_extension("json.tmp");
        let Ok(json) = serde_json::to_string_pretty(&data) else {
            warn!("Failed to serialize LTE data");
            return;
        };
        if let Err(e) = std::fs::write(&tmp, &json) {
            warn!("Failed to write lte_data.json.tmp: {e}");
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, &path) {
            warn!("Failed to rename lte_data.json.tmp: {e}");
        }
    }

    /// Promote current band config to safe bands. Called when tunnel has been stable for 5+ min.
    ///
    /// Signal quality gate: configs with RSRP below -110 dBm are "marginal" and
    /// won't overwrite an existing safe config that has better signal quality.
    pub fn promote_safe_bands(
        &mut self,
        data_dir: &str,
        bands: &[u16],
        priority: Option<u16>,
        rsrp: Option<i32>,
    ) {
        // Guard: reject suspiciously large band lists — these are the un-clamped
        // all-128 "auto" config, not actual hardware bands. Promoting them would
        // cause verified_set_bands to log errors on every restore.
        if bands.len() > 64 {
            warn!(
                "LTE: skipping safe-bands promotion ({} bands — likely un-clamped auto config)",
                bands.len()
            );
            return;
        }

        // Quality gate: if below threshold, only promote if no existing safe config
        // or existing config has worse signal
        if let Some(current_rsrp) = rsrp {
            if current_rsrp < SAFE_BANDS_MIN_RSRP {
                if let Some(ref existing) = self.safe_bands {
                    let existing_rsrp = existing.signal_rsrp.unwrap_or(i32::MIN);
                    if existing_rsrp >= current_rsrp {
                        info!(
                            "LTE: skipping safe-bands promotion (RSRP {current_rsrp} dBm < \
                             threshold {SAFE_BANDS_MIN_RSRP}, existing has {existing_rsrp} dBm)"
                        );
                        return;
                    }
                }
                info!(
                    "LTE: promoting marginal safe bands (RSRP {current_rsrp} dBm < threshold, \
                     but no better config exists)"
                );
            }
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        self.safe_bands = Some(SafeBandConfig {
            bands: bands.to_vec(),
            priority_band: priority,
            confirmed_at: now,
            signal_rsrp: rsrp,
        });
        self.save_safe_bands(data_dir);
    }

    /// Record a band change. Only `User` source saves `pre_change_bands` and
    /// sets `last_band_change_at` (watchdog reverts must not contaminate the
    /// pre-change detection window).
    pub fn record_band_change(
        &mut self,
        source: BandChangeSource,
        current_bands: &[u16],
        current_priority: Option<u16>,
        new_bands: &[u16],
    ) {
        // Reset stability tracking — new config needs to prove itself
        self.band_stable_since = None;
        self.bands_at_connect = None;

        if source == BandChangeSource::User {
            self.last_band_change_at = Some(Instant::now());
            // Save pre-change state so watchdog can revert.
            // Guard: only save if we got a valid snapshot — empty bands would cause
            // a revert to AT+QCFG="band",0x260,0,0 which disables all LTE bands.
            if !current_bands.is_empty() {
                self.pre_change_bands = Some((current_bands.to_vec(), current_priority));
            }
            info!(
                "Recorded user band change: B{} → B{}",
                current_bands
                    .iter()
                    .map(|b| b.to_string())
                    .collect::<Vec<_>>()
                    .join(",B"),
                new_bands
                    .iter()
                    .map(|b| b.to_string())
                    .collect::<Vec<_>>()
                    .join(",B"),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// SIM state persistence + APN auto-configuration
// ---------------------------------------------------------------------------

/// Persisted SIM identity — tracks which SIM the current band/history data belongs to.
/// Written to `{data_dir}/sim_state.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimState {
    /// ICCID of the SIM that was active when bands/history were last saved.
    pub iccid: String,
    /// IMSI (from AT+CIMI) — MCC+MNC prefix identifies the home carrier.
    pub imsi: Option<String>,
    /// Epoch seconds when this SIM was first seen.
    pub first_seen: u64,
    /// Epoch seconds when SIM state was last updated.
    pub last_seen: u64,
}

/// Load SIM state from `{data_dir}/sim_state.json`. Returns `None` on missing file.
fn load_sim_state(data_dir: &str) -> Option<SimState> {
    let path = std::path::Path::new(data_dir).join("sim_state.json");
    match std::fs::read_to_string(&path) {
        Ok(contents) => match serde_json::from_str::<SimState>(&contents) {
            Ok(state) => Some(state),
            Err(e) => {
                warn!("Failed to parse sim_state.json: {e}");
                None
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            warn!("Failed to read sim_state.json: {e}");
            None
        }
    }
}

/// Persist SIM state to `{data_dir}/sim_state.json` (atomic: tmp + rename).
fn save_sim_state(data_dir: &str, state: &SimState) {
    let path = std::path::Path::new(data_dir).join("sim_state.json");
    let tmp = path.with_extension("json.tmp");
    let Ok(json) = serde_json::to_string_pretty(state) else {
        warn!("Failed to serialize SIM state");
        return;
    };
    if let Err(e) = std::fs::write(&tmp, &json) {
        warn!("Failed to write sim_state.json.tmp: {e}");
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        warn!("Failed to rename sim_state.json.tmp: {e}");
    }
}

/// Delete SIM-specific persisted data on SIM change.
fn clear_sim_data(data_dir: &str) {
    for name in ["safe_bands.json", "lte_data.json"] {
        let path = std::path::Path::new(data_dir).join(name);
        if path.exists() {
            match std::fs::remove_file(&path) {
                Ok(()) => info!("SIM change: removed {name}"),
                Err(e) => warn!("SIM change: failed to remove {name}: {e}"),
            }
        }
    }
}

/// APN fallback database: (IMSI prefix [MCC+MNC], APN).
/// Looked up by longest-prefix match. Config override and modem PDP context take priority.
const APN_DATABASE: &[(&str, &str)] = &[
    // IoT MVNOs
    ("29505", "em"),           // EMnify (Liechtenstein)
    ("90143", "em"),           // EMnify (international)
    ("20404", "em"),           // EMnify (Netherlands)
    ("23450", "hologram"),     // Hologram (UK)
    ("29510", "iot.1nce.net"), // 1NCE
    ("44010", "soracom.io"),   // Soracom
    // Canada
    ("302720", "ltemobile.apn"), // Rogers
    ("302370", "sp.fido.ca"),    // Fido (Rogers MVNO)
    ("302610", "pda.bell.ca"),   // Bell
    ("302220", "sp.telus.com"),  // Telus
    // US
    ("310260", "fast.t-mobile.com"), // T-Mobile
    ("310410", "broadband"),         // AT&T
    ("311480", "vzwinternet"),       // Verizon
];

/// Look up APN by IMSI prefix (longest match wins).
fn lookup_apn_by_imsi(imsi: &str) -> Option<&'static str> {
    let mut best: Option<(&str, usize)> = None;
    for &(prefix, apn) in APN_DATABASE {
        if imsi.starts_with(prefix) {
            let len = prefix.len();
            if best.is_none() || len > best.unwrap().1 {
                best = Some((apn, len));
            }
        }
    }
    best.map(|(apn, _)| apn)
}

/// Resolve the APN for the current SIM using a three-tier priority chain:
/// 1. Manual override from `[lte] apn` in config
/// 2. Modem's own PDP context (SIM-provisioned APN via AT+CGDCONT?)
/// 3. Built-in IMSI prefix database
async fn resolve_apn(
    modem: &Modem,
    config_apn: Option<&str>,
    imsi: Option<&str>,
) -> Option<(String, &'static str)> {
    // Tier 1: config override
    if let Some(apn) = config_apn {
        return Some((apn.to_string(), "config"));
    }

    // Tier 2: modem's PDP context (SIM-provisioned)
    if let Ok(resp) = modem.command("AT+CGDCONT?").await {
        if let Some(apn) = parse_cgdcont(&resp) {
            return Some((apn, "modem PDP context"));
        }
    }

    // Tier 3: IMSI database lookup
    if let Some(imsi) = imsi {
        if let Some(apn) = lookup_apn_by_imsi(imsi) {
            return Some((apn.to_string(), "IMSI database"));
        }
    }

    None
}

/// Read a uci config value. Returns `None` on error or missing key.
async fn uci_get(key: &str) -> Option<String> {
    tokio::process::Command::new("uci")
        .args(["get", key])
        .output()
        .await
        .ok()
        .and_then(|o| {
            if o.status.success() {
                let v = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if v.is_empty() {
                    None
                } else {
                    Some(v)
                }
            } else {
                None
            }
        })
}

/// Set a uci config value.
async fn uci_set(key: &str, value: &str) -> Result<(), String> {
    let out = tokio::process::Command::new("uci")
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

/// Configure APN and pdptype on OpenWrt via uci. Skips if already correct.
async fn configure_apn_openwrt(modem: &Modem, interface: &str, apn: &str) -> Result<(), String> {
    // Write modem PDP context directly — this is critical because netifd's QMI
    // proto passes APN through QMI at connect time, but the modem's internal
    // AT+CGDCONT context can conflict and prevent registration. After SIM swaps
    // the stale PDP context from the old SIM blocks the new SIM's data session.
    let pdp_cmd = format!("AT+CGDCONT=1,\"IP\",\"{apn}\"");
    match modem.command(&pdp_cmd).await {
        Ok(_) => info!("APN: modem PDP context set to '{apn}' (IP)"),
        Err(e) => warn!("APN: failed to set modem PDP context: {e}"),
    }

    if !crate::lte_watchdog::is_openwrt() {
        info!("APN: not OpenWrt — set APN to '{apn}' manually in your network config");
        return Ok(());
    }

    let netifd_name = crate::lte_watchdog::resolve_netifd_interface(interface).await;

    // Read current config
    let current_apn = uci_get(&format!("network.{netifd_name}.apn")).await;
    let current_pdptype = uci_get(&format!("network.{netifd_name}.pdptype")).await;

    let needs_apn = current_apn.as_deref() != Some(apn);
    let needs_pdptype = current_pdptype.as_deref() != Some("ip");

    if !needs_apn && !needs_pdptype {
        info!("APN: uci already correct ('{apn}', pdptype=ip)");
        return Ok(());
    }

    info!(
        "APN: configuring {netifd_name} — apn: '{}' → '{apn}', pdptype: '{}' → 'ip'",
        current_apn.as_deref().unwrap_or("(none)"),
        current_pdptype.as_deref().unwrap_or("(none)")
    );

    if needs_apn {
        uci_set(&format!("network.{netifd_name}.apn"), apn).await?;
    }

    // uci set pdptype to IPv4-only — many IoT MVNOs (e.g. EMnify) fail on
    // IPv6, and OpenWrt's QMI proto tears down the entire session when ipv4v6
    // is set and IPv6 fails.
    if needs_pdptype {
        uci_set(&format!("network.{netifd_name}.pdptype"), "ip").await?;
    }

    let commit = tokio::process::Command::new("uci")
        .args(["commit", "network"])
        .output()
        .await
        .map_err(|e| format!("uci commit: {e}"))?;
    if !commit.status.success() {
        return Err(format!(
            "uci commit failed: {}",
            String::from_utf8_lossy(&commit.stderr)
        ));
    }

    // Restart interface
    let _ = tokio::process::Command::new("ifdown")
        .arg(&netifd_name)
        .output()
        .await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let _ = tokio::process::Command::new("ifup")
        .arg(&netifd_name)
        .output()
        .await;

    info!("APN: applied '{apn}' and restarted {netifd_name}");
    Ok(())
}

/// Detect SIM change at startup and take recovery actions.
///
/// Called after modem is opened but BEFORE safe-bands restore.
/// Returns `true` if a SIM change was detected (bands reset to auto, APN reconfigured).
pub async fn detect_sim_change(
    modem: &Modem,
    lte_state: &Arc<Mutex<LteState>>,
    data_dir: &str,
    lte_config: &LteConfig,
) -> bool {
    // Read current ICCID
    let iccid = match modem.command("AT+QCCID").await {
        Ok(resp) => {
            let Some(id) = parse_qccid(&resp) else {
                warn!("SIM detect: AT+QCCID returned empty, skipping");
                return false;
            };
            id
        }
        Err(e) => {
            warn!("SIM detect: AT+QCCID failed ({e}), skipping");
            return false;
        }
    };

    // Read IMSI for APN lookup
    let imsi = modem
        .command("AT+CIMI")
        .await
        .ok()
        .and_then(|r| parse_cimi(&r));

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let previous = load_sim_state(data_dir);
    let sim_changed = matches!(&previous, Some(prev) if prev.iccid != iccid);

    if sim_changed {
        let prev_iccid = previous.as_ref().map_or("?", |p| p.iccid.as_str());
        info!(
            "SIM CHANGE DETECTED: {} -> {} — resetting bands and clearing history",
            prev_iccid, iccid
        );

        // 1. Clear persisted SIM-specific data
        clear_sim_data(data_dir);

        // 2. Clear in-memory state
        {
            let mut ls = lte_state.lock().await;
            ls.safe_bands = None;
            ls.band_history.clear();
            ls.scan_status = None;
            ls.band_stable_since = None;
            ls.bands_at_connect = None;
            ls.pre_change_bands = None;
            ls.last_band_change_at = None;
        }

        // 3. Set modem to all bands (auto mode)
        let all_bands: Vec<u16> = (1..=128).collect();
        let hex = bands_to_hex(&all_bands);
        match modem
            .command(&format!("AT+QCFG=\"band\",260,{hex},0"))
            .await
        {
            Ok(_) => info!("SIM change: bands set to auto (all open)"),
            Err(e) => warn!("SIM change: failed to set bands to auto: {e}"),
        }
        // Clear priority band
        let _ = modem.command("AT+QCFG=\"bandpri\",0").await;
        // Re-register
        let _ = modem.command("AT+COPS=0").await;

        // 4. Resolve and configure APN
        let apn_result = resolve_apn(modem, lte_config.apn.as_deref(), imsi.as_deref()).await;

        if let Some((apn, source)) = &apn_result {
            info!("SIM change: APN '{apn}' (from {source})");
            if let Err(e) = configure_apn_openwrt(modem, &lte_config.interface, apn).await {
                warn!("SIM change: APN configuration failed: {e}");
            }
        } else {
            warn!(
                "SIM change: no APN found for IMSI {} — configure via [lte] apn in sctl.toml",
                imsi.as_deref().unwrap_or("unknown")
            );
        }
    } else if previous.is_none() {
        // First boot — resolve APN from config or IMSI database, same as SIM change.
        // This ensures the modem PDP context matches the uci config even on initial
        // deployment (the modem may have a stale PDP context from factory or prior use).
        let apn_result = resolve_apn(modem, lte_config.apn.as_deref(), imsi.as_deref()).await;

        if let Some((apn, source)) = &apn_result {
            info!("First boot: APN '{apn}' (from {source}), recording ICCID {iccid}");
            if let Err(e) = configure_apn_openwrt(modem, &lte_config.interface, apn).await {
                warn!("First boot: APN configuration failed: {e}");
            }
        } else {
            info!("SIM state: first boot, recording ICCID {iccid}");
        }
    } else {
        // Same SIM — but sync modem PDP context if config APN is set.
        // This catches the case where uci was set but AT+CGDCONT wasn't synced
        // (e.g. after manual APN fix).
        if let Some(ref manual_apn) = lte_config.apn {
            let pdp_cmd = format!("AT+CGDCONT=1,\"IP\",\"{manual_apn}\"");
            match modem.command(&pdp_cmd).await {
                Ok(_) => info!("Startup: synced modem PDP context to config APN '{manual_apn}'"),
                Err(e) => warn!("Startup: failed to sync PDP context: {e}"),
            }
        }
    }

    // Persist current SIM state
    let new_state = SimState {
        iccid,
        imsi,
        first_seen: previous
            .as_ref()
            .map_or(now, |p| if sim_changed { now } else { p.first_seen }),
        last_seen: now,
    };
    save_sim_state(data_dir, &new_state);

    sim_changed
}

/// Compute signal bars (1-5) from RSRP, falling back to RSSI.
fn compute_signal_bars(rsrp: Option<i32>, rssi_dbm: i32) -> u8 {
    if let Some(rsrp) = rsrp {
        return match rsrp {
            _ if rsrp >= -80 => 5,
            _ if rsrp >= -90 => 4,
            _ if rsrp >= -100 => 3,
            _ if rsrp >= -110 => 2,
            _ => 1,
        };
    }
    match rssi_dbm {
        _ if rssi_dbm >= -65 => 5,
        _ if rssi_dbm >= -75 => 4,
        _ if rssi_dbm >= -85 => 3,
        _ if rssi_dbm >= -95 => 2,
        _ => 1,
    }
}

// ── EARFCN / band utilities ──────────────────────────────────────────

/// 3GPP TS 36.101 EARFCN → LTE band lookup table.
/// Each entry: (band, dl_offset, dl_high).
const EARFCN_TABLE: &[(u16, u32, u32)] = &[
    (1, 0, 599),
    (2, 600, 1199),
    (3, 1200, 1949),
    (4, 1950, 2399),
    (5, 2400, 2649),
    (7, 2750, 3449),
    (8, 3450, 3799),
    (12, 5010, 5179),
    (13, 5180, 5279),
    (14, 5280, 5379),
    (17, 5730, 5849),
    (20, 6150, 6449),
    (25, 8040, 8689),
    (26, 8690, 9039),
    (28, 9210, 9659),
    (29, 9660, 9769),
    (30, 9770, 9869),
    (66, 66436, 67335),
    (71, 68586, 68935),
];

/// Convert an EARFCN to an LTE band number. Returns `None` if not in the table.
#[must_use]
pub fn earfcn_to_band(earfcn: u32) -> Option<u16> {
    for &(band, dl_offset, dl_high) in EARFCN_TABLE {
        if earfcn >= dl_offset && earfcn <= dl_high {
            return Some(band);
        }
    }
    None
}

/// Convert a list of LTE band numbers to a hex bitmask string (no `0x` prefix).
/// Bit (N-1) represents band N. Uses u128 to support bands up to 128 (e.g. B66, B71).
#[must_use]
pub fn bands_to_hex(bands: &[u16]) -> String {
    let mut mask: u128 = 0;
    for &b in bands {
        if (1..=128).contains(&b) {
            mask |= 1u128 << (b - 1);
        }
    }
    format!("{mask:X}")
}

/// Send `AT+QCFG="band"` to set LTE bands, then read back and verify the modem applied the change.
/// Result from `verified_set_bands`: actual bands + whether modem was deregistered.
#[derive(Debug)]
pub struct VerifiedSetResult {
    /// Actual bands read back from modem after write.
    pub bands: Vec<u16>,
    /// True if `AT+COPS=2` was needed (modem was deregistered to apply the change).
    /// When true, caller must re-register (`AT+COPS=0`) and restart the data interface.
    pub did_deregister: bool,
}

/// First tries a direct write; if the modem silently rejects it (QMI data session active),
/// deregisters (`AT+COPS=2`) and retries. Returns the actual bands and whether deregistration
/// was needed.
pub async fn verified_set_bands(
    modem: &crate::modem::Modem,
    bands: &[u16],
) -> Result<VerifiedSetResult, String> {
    let hex = bands_to_hex(bands);
    let cmd = format!("AT+QCFG=\"band\",260,{hex},0");

    // First attempt: direct write (works if no active QMI session)
    modem
        .command(&cmd)
        .await
        .map_err(|e| format!("AT+QCFG band write failed: {e}"))?;

    // Delay before read-back — give modem time to apply and reduces QMI disruption
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    // Read back and verify
    let resp = modem
        .command("AT+QCFG=\"band\"")
        .await
        .map_err(|e| format!("AT+QCFG band read-back failed: {e}"))?;
    let actual = parse_band_config(&resp);
    let actual_hex = bands_to_hex(&actual);
    if actual_hex == hex {
        return Ok(VerifiedSetResult {
            bands: actual,
            did_deregister: false,
        });
    }

    // Modem ignored the write — deregister and retry
    tracing::info!(
        "Band write ignored (wrote {hex}, got {actual_hex}), deregistering and retrying"
    );
    let _ = modem.command("AT+COPS=2").await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    modem
        .command(&cmd)
        .await
        .map_err(|e| format!("AT+QCFG band retry write failed: {e}"))?;

    // Read back again
    let resp2 = modem
        .command("AT+QCFG=\"band\"")
        .await
        .map_err(|e| format!("AT+QCFG band retry read-back failed: {e}"))?;
    let retry_bands = parse_band_config(&resp2);
    let retry_hex = bands_to_hex(&retry_bands);
    if retry_hex != hex {
        // Re-register so modem isn't left deregistered
        let _ = modem.command("AT+COPS=0").await;
        return Err(format!(
            "modem rejected band change after deregister: wrote {hex}, read back {retry_hex}"
        ));
    }
    Ok(VerifiedSetResult {
        bands: retry_bands,
        did_deregister: true,
    })
}

/// Update band history from a freshly-polled signal reading.
fn update_band_history(history: &mut HashMap<u16, BandHistoryEntry>, signal: &LteSignal) {
    let now = signal.recorded_at;

    // Record serving cell
    if let (Some(band), Some(rsrp), Some(pci)) = (signal.freq_band, signal.rsrp, signal.pci) {
        let obs = BandObservation {
            rsrp,
            rsrq: signal.rsrq,
            sinr: signal.sinr,
            pci,
            recorded_at: now,
            serving: true,
        };
        let entry = history.entry(band).or_insert_with(|| BandHistoryEntry {
            band,
            best_rsrp: rsrp,
            latest_rsrp: rsrp,
            observation_count: 0,
            last_seen: now,
            recent: VecDeque::with_capacity(MAX_BAND_OBSERVATIONS),
        });
        entry.latest_rsrp = rsrp;
        if rsrp > entry.best_rsrp {
            entry.best_rsrp = rsrp;
        }
        entry.observation_count += 1;
        entry.last_seen = now;
        if entry.recent.len() >= MAX_BAND_OBSERVATIONS {
            entry.recent.pop_front();
        }
        entry.recent.push_back(obs);
    }

    // Record neighbor cells
    for neighbor in &signal.neighbors {
        let Some(band) = earfcn_to_band(neighbor.earfcn) else {
            continue;
        };
        let Some(rsrp) = neighbor.rsrp else {
            continue;
        };
        let obs = BandObservation {
            rsrp,
            rsrq: neighbor.rsrq,
            sinr: neighbor.sinr,
            pci: neighbor.pci,
            recorded_at: now,
            serving: false,
        };
        let entry = history.entry(band).or_insert_with(|| BandHistoryEntry {
            band,
            best_rsrp: rsrp,
            latest_rsrp: rsrp,
            observation_count: 0,
            last_seen: now,
            recent: VecDeque::with_capacity(MAX_BAND_OBSERVATIONS),
        });
        entry.latest_rsrp = rsrp;
        if rsrp > entry.best_rsrp {
            entry.best_rsrp = rsrp;
        }
        entry.observation_count += 1;
        entry.last_seen = now;
        if entry.recent.len() >= MAX_BAND_OBSERVATIONS {
            entry.recent.pop_front();
        }
        entry.recent.push_back(obs);
    }
}

// ── Band control ────────────────────────────────────────────────────

/// Apply bands immediately (verified write + priority set) without waiting
/// for registration. Returns the verified `BandConfig` plus old bands/priority
/// for rollback by a background `monitor_registration` task.
///
/// Typically completes in <3s. The caller should spawn `monitor_registration`
/// afterward if registration confirmation is needed.
pub async fn apply_bands_fast(
    modem: &Modem,
    lte_bands: &[u16],
    priority_band: Option<u16>,
) -> Result<(BandConfig, Vec<u16>, Option<u16>, bool), String> {
    // Space AT commands to minimize QMI data path disruption.
    let cmd_delay = Duration::from_secs(1);

    // 1. Read current config for rollback reference
    let old_bands_resp = modem
        .command("AT+QCFG=\"band\"")
        .await
        .map_err(|e| format!("failed to read current bands: {e}"))?;
    let old_bands = parse_band_config(&old_bands_resp);

    tokio::time::sleep(cmd_delay).await;

    let old_pri_resp = modem
        .command("AT+QCFG=\"bandpri\"")
        .await
        .map_err(|e| format!("failed to read current bandpri: {e}"))?;
    let old_priority = parse_bandpri(&old_pri_resp);

    tokio::time::sleep(cmd_delay).await;

    // Detect if we're only adding bands (new is superset of old).
    // Adding bands doesn't change the serving cell — the modem just updates its
    // band scan list. No deregistration or interface restart needed.
    let old_set: std::collections::HashSet<u16> = old_bands.iter().copied().collect();
    let new_set: std::collections::HashSet<u16> = lte_bands.iter().copied().collect();
    let is_additive = old_set.is_subset(&new_set) && old_set != new_set;

    // 2. Write bands
    let result = if is_additive {
        // Additive change: simple write, no deregister fallback.
        // If the modem silently rejects, we just report what it accepted.
        let hex = bands_to_hex(lte_bands);
        let cmd = format!("AT+QCFG=\"band\",260,{hex},0");
        modem
            .command(&cmd)
            .await
            .map_err(|e| format!("AT+QCFG band write failed: {e}"))?;

        tokio::time::sleep(cmd_delay).await;

        let resp = modem
            .command("AT+QCFG=\"band\"")
            .await
            .map_err(|e| format!("AT+QCFG band read-back failed: {e}"))?;
        let actual = parse_band_config(&resp);
        let actual_hex = bands_to_hex(&actual);
        if actual_hex != hex {
            info!("Additive band write: wrote {hex}, got {actual_hex} (modem may need deregister for full change)");
        }
        VerifiedSetResult {
            bands: actual,
            did_deregister: false,
        }
    } else {
        // Non-additive (removing bands or switching): use verified write with
        // deregister fallback since the serving band may be removed.
        verified_set_bands(modem, lte_bands).await?
    };

    // 3. Re-register only if deregistration was needed
    if result.did_deregister {
        let _ = modem.command("AT+COPS=0").await;
    }

    tokio::time::sleep(cmd_delay).await;

    // 4. Set priority
    if let Some(pri) = priority_band {
        let pri_cmd = format!("AT+QCFG=\"bandpri\",{pri}");
        if let Err(e) = modem.command(&pri_cmd).await {
            warn!("Failed to set bandpri={pri}: {e}");
        }
    } else {
        let _ = modem.command("AT+QCFG=\"bandpri\",0").await;
    }

    let config = BandConfig {
        enabled_bands: result.bands,
        priority_band,
    };
    Ok((config, old_bands, old_priority, result.did_deregister))
}

/// Background task: polls CEREG for registration after a fast band change.
/// On timeout, rolls back to old bands (unless another change superseded ours).
#[allow(clippy::too_many_arguments)]
pub async fn monitor_registration(
    modem: Modem,
    lte_state: Arc<Mutex<LteState>>,
    expected_bands: Vec<u16>,
    old_bands: Vec<u16>,
    old_priority: Option<u16>,
    timeout: Duration,
    interface: String,
    interface_restart_cmd: Option<String>,
) {
    let start = tokio::time::Instant::now();
    let poll_interval = Duration::from_secs(3);
    let openwrt = crate::lte_watchdog::is_openwrt();

    loop {
        if start.elapsed() >= timeout {
            break;
        }
        tokio::time::sleep(poll_interval).await;

        match modem.command("AT+CEREG?").await {
            Ok(resp) => {
                if let Ok(status) = parse_cereg(&resp) {
                    if status.is_registered() {
                        info!("Background registration complete, checking data path");
                        recover_data_path_pub(
                            &modem,
                            &lte_state,
                            &expected_bands,
                            &interface,
                            openwrt,
                            interface_restart_cmd.as_deref(),
                        )
                        .await;

                        let mut ls = lte_state.lock().await;
                        ls.band_action_until = None;
                        ls.registration_pending = false;
                        return;
                    }
                }
            }
            Err(e) => {
                warn!("CEREG poll failed during background registration: {e}");
            }
        }
    }

    // Timeout — check if bands still match before rolling back
    // (another band change may have superseded ours)
    let current_resp = modem.command("AT+QCFG=\"band\"").await.unwrap_or_default();
    let current_bands = parse_band_config(&current_resp);
    let current_hex = bands_to_hex(&current_bands);
    let expected_hex = bands_to_hex(&expected_bands);

    if current_hex == expected_hex {
        warn!("Background registration timed out, rolling back to previous config");
        match verified_set_bands(&modem, &old_bands).await {
            Ok(_) => info!("Rollback verified: bands restored"),
            Err(e) => warn!("Rollback verification failed: {e}"),
        }
        if let Some(pri) = old_priority {
            let _ = modem.command(&format!("AT+QCFG=\"bandpri\",{pri}")).await;
        }
        let _ = modem.command("AT+COPS=0").await;

        // Recover data path after rollback re-registration
        recover_data_path_pub(
            &modem,
            &lte_state,
            &old_bands,
            &interface,
            openwrt,
            interface_restart_cmd.as_deref(),
        )
        .await;
    } else {
        info!("Bands changed during registration wait, skipping rollback");
    }

    let mut ls = lte_state.lock().await;
    ls.band_action_until = None;
    ls.registration_pending = false;
}

/// After modem registration, restart the LTE data interface to re-establish
/// the QMI data bearer and obtain a fresh DHCP lease.
///
/// Band changes invalidate the QMI bearer even if the old IP is still bound
/// to the interface (stale lease), so we always restart rather than checking
/// IPv4 first.
pub async fn recover_data_path_pub(
    modem: &Modem,
    lte_state: &Arc<Mutex<LteState>>,
    expected_bands: &[u16],
    interface: &str,
    openwrt: bool,
    interface_restart_cmd: Option<&str>,
) {
    info!("Restarting {interface} to re-establish QMI data bearer");

    // Extend watchdog suppression to cover interface restart + band re-apply
    {
        let mut ls = lte_state.lock().await;
        ls.band_action_until = Some(Instant::now() + Duration::from_secs(20));
    }

    crate::lte_watchdog::action_restart_interface(interface, openwrt, interface_restart_cmd).await;

    // Wait for DHCP
    tokio::time::sleep(Duration::from_secs(5)).await;

    // OpenWrt's QMI proto may reset band config during ifup — re-apply
    let resp = modem.command("AT+QCFG=\"band\"").await.unwrap_or_default();
    let current = parse_band_config(&resp);
    let current_hex = bands_to_hex(&current);
    let expected_hex = bands_to_hex(expected_bands);
    if current_hex != expected_hex {
        info!("Bands reset by ifup ({current_hex} != {expected_hex}), re-applying");
        match verified_set_bands(modem, expected_bands).await {
            Ok(_) => info!("Bands re-applied after ifup"),
            Err(e) => warn!("Failed to re-apply bands after ifup: {e}"),
        }
    }

    if crate::lte_watchdog::interface_has_ipv4(interface) {
        info!("Data path recovered on {interface}");
    } else {
        warn!("Data path not recovered on {interface}, watchdog will handle");
    }
}

/// Set LTE bands on the modem with rollback on registration failure.
///
/// 1. Reads current config for rollback
/// 2. Writes new band bitmask
/// 3. Optionally sets priority band
/// 4. Polls AT+CEREG? for registration within timeout
/// 5. If timeout → restores previous config, returns error
pub async fn safe_set_bands(
    modem: &Modem,
    lte_bands: &[u16],
    priority_band: Option<u16>,
    timeout: Duration,
    lte_state: Option<&Arc<Mutex<LteState>>>,
) -> Result<BandConfig, String> {
    // Set band_action_until to suppress watchdog during band change
    if let Some(state) = lte_state {
        state.lock().await.band_action_until =
            Some(Instant::now() + timeout + Duration::from_secs(5));
    }

    let result = do_safe_set_bands(modem, lte_bands, priority_band, timeout, lte_state).await;

    // Clear suppression on failure (success path clears it inside do_safe_set_bands)
    if result.is_err() {
        if let Some(state) = lte_state {
            state.lock().await.band_action_until = None;
        }
    }

    result
}

async fn do_safe_set_bands(
    modem: &Modem,
    lte_bands: &[u16],
    priority_band: Option<u16>,
    timeout: Duration,
    lte_state: Option<&Arc<Mutex<LteState>>>,
) -> Result<BandConfig, String> {
    // 1. Read current config for rollback
    let old_bands_resp = modem
        .command("AT+QCFG=\"band\"")
        .await
        .map_err(|e| format!("failed to read current bands: {e}"))?;
    let old_bands = parse_band_config(&old_bands_resp);

    let old_pri_resp = modem
        .command("AT+QCFG=\"bandpri\"")
        .await
        .map_err(|e| format!("failed to read current bandpri: {e}"))?;
    let old_priority = parse_bandpri(&old_pri_resp);

    // 2. Write new bands (with read-back verification — deregisters first)
    verified_set_bands(modem, lte_bands).await?;

    // Re-register on new bands
    let _ = modem.command("AT+COPS=0").await;

    // 3. Set priority if requested
    if let Some(pri) = priority_band {
        let pri_cmd = format!("AT+QCFG=\"bandpri\",{pri}");
        if let Err(e) = modem.command(&pri_cmd).await {
            warn!("Failed to set bandpri={pri}: {e}");
        }
    } else {
        // Clear priority by setting to 0
        let _ = modem.command("AT+QCFG=\"bandpri\",0").await;
    }

    // 4. Poll for registration
    let poll_interval = Duration::from_secs(3);
    let start = tokio::time::Instant::now();
    loop {
        if start.elapsed() >= timeout {
            break;
        }
        tokio::time::sleep(poll_interval).await;

        match modem.command("AT+CEREG?").await {
            Ok(resp) => {
                if let Ok(status) = parse_cereg(&resp) {
                    if status.is_registered() {
                        // Clear band action suppression
                        if let Some(state) = lte_state {
                            state.lock().await.band_action_until = None;
                        }
                        // Read back actual bands from modem (don't trust our write)
                        let actual_bands = match modem.command("AT+QCFG=\"band\"").await {
                            Ok(resp) => parse_band_config(&resp),
                            Err(_) => lte_bands.to_vec(), // fallback to requested
                        };
                        return Ok(BandConfig {
                            enabled_bands: actual_bands,
                            priority_band,
                        });
                    }
                }
            }
            Err(e) => {
                warn!("CEREG poll failed during band change: {e}");
            }
        }
    }

    // 5. Timeout — rollback (with verification)
    warn!("Band change timed out, rolling back to previous config");
    match verified_set_bands(modem, &old_bands).await {
        Ok(_) => info!("Rollback verified: bands restored"),
        Err(e) => warn!("Rollback verification failed: {e}"),
    }
    if let Some(pri) = old_priority {
        let _ = modem.command(&format!("AT+QCFG=\"bandpri\",{pri}")).await;
    }

    // Clear band action suppression
    if let Some(state) = lte_state {
        state.lock().await.band_action_until = None;
    }

    Err("modem did not register on new bands within timeout, rolled back".into())
}

/// Spawn a background band scan task. Locks to each band, measures signal,
/// optionally runs speed test, then restores original config.
#[allow(clippy::too_many_arguments)]
pub fn spawn_band_scan(
    modem: Modem,
    lte_state: Arc<Mutex<LteState>>,
    bands_to_scan: Vec<u16>,
    include_speed_test: bool,
    speed_test_url: Option<String>,
    speed_test_upload_url: Option<String>,
    data_dir: String,
    interface: String,
    tunnel_stats: Arc<TunnelStats>,
    force: bool,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Read current config for restoration
        let original_bands = match modem.command("AT+QCFG=\"band\"").await {
            Ok(resp) => parse_band_config(&resp),
            Err(_) => vec![],
        };
        let original_priority = match modem.command("AT+QCFG=\"bandpri\"").await {
            Ok(resp) => parse_bandpri(&resp),
            Err(_) => None,
        };

        // Set scan status
        {
            let mut state = lte_state.lock().await;
            state.scan_status = Some(ScanStatus {
                state: "running".into(),
                started_at: now,
                completed_at: None,
                bands_to_scan: bands_to_scan.clone(),
                bands_scanned: vec![],
                current_band: None,
                results: vec![],
                original_bands: original_bands.clone(),
                original_priority,
            });
        }

        let mut results = Vec::new();

        for &band in &bands_to_scan {
            // Abort if tunnel reconnected mid-scan (AT commands would kill it) — unless forced
            if !force && tunnel_stats.connected.load(Ordering::Relaxed) {
                warn!("Band scan: aborting — tunnel reconnected");
                let mut state = lte_state.lock().await;
                if let Some(ref mut scan) = state.scan_status {
                    scan.state = "aborted".into();
                    scan.completed_at = Some(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                    );
                }
                break; // Falls through to restoration code below
            }

            // Update current band
            {
                let mut state = lte_state.lock().await;
                if let Some(ref mut scan) = state.scan_status {
                    scan.current_band = Some(band);
                }
            }

            if let Err(e) = verified_set_bands(&modem, &[band]).await {
                warn!("Band scan: failed to lock to B{band}: {e}");
                results.push(ScanBandResult {
                    band,
                    registered: false,
                    registration_time_ms: 0,
                    rsrp: None,
                    rsrq: None,
                    sinr: None,
                    download_bps: None,
                    upload_bps: None,
                });
                continue;
            }

            // Re-check tunnel before destructive COPS (could reconnect between band lock and here)
            if !force && tunnel_stats.connected.load(Ordering::Relaxed) {
                warn!("Band scan: tunnel reconnected mid-band, aborting");
                let mut state = lte_state.lock().await;
                if let Some(ref mut scan) = state.scan_status {
                    scan.state = "aborted".into();
                    scan.completed_at = Some(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                    );
                }
                break;
            }

            // Force network re-registration so the modem actually tries the new band.
            // Without this, CEREG reports "registered" from the old band's stale state.
            let _ = modem.command("AT+COPS=2").await; // deregister
            tokio::time::sleep(Duration::from_secs(1)).await;
            let _ = modem.command("AT+COPS=0").await; // auto-register on new band

            // Poll for registration (up to 45s)
            let reg_start = tokio::time::Instant::now();
            let mut registered = false;
            for _ in 0..15 {
                tokio::time::sleep(Duration::from_secs(3)).await;
                if let Ok(resp) = modem.command("AT+CEREG?").await {
                    if let Ok(status) = parse_cereg(&resp) {
                        if status.is_registered() {
                            registered = true;
                            break;
                        }
                    }
                }
            }

            let registration_time_ms = reg_start.elapsed().as_millis() as u64;
            let mut result = ScanBandResult {
                band,
                registered,
                registration_time_ms,
                rsrp: None,
                rsrq: None,
                sinr: None,
                download_bps: None,
                upload_bps: None,
            };

            if registered {
                // Wait a beat for signal to settle
                tokio::time::sleep(Duration::from_secs(2)).await;

                // Read signal
                if let Ok(resp) = modem.command("AT+QENG=\"servingcell\"").await {
                    let qeng = parse_qeng(&resp);
                    result.rsrp = qeng.rsrp;
                    result.rsrq = qeng.rsrq;
                    result.sinr = qeng.sinr;
                }

                // Optional speed test
                if include_speed_test {
                    if let Some(ref url) = speed_test_url {
                        result.download_bps = run_download_speed_test(url, &interface).await;
                    }
                    if let Some(ref url) = speed_test_upload_url {
                        result.upload_bps = run_upload_speed_test(url, &interface).await;
                    }
                }
            }

            results.push(result);

            // Update scan status
            {
                let mut state = lte_state.lock().await;
                if let Some(ref mut scan) = state.scan_status {
                    scan.bands_scanned.push(band);
                    scan.results.clone_from(&results);
                }
            }
        }

        // Always restore original config and force clean re-registration.
        // The scan leaves the modem deregistered (AT+COPS=2) after the last band,
        // so we must explicitly re-register or the modem stays offline.
        match verified_set_bands(&modem, &original_bands).await {
            Ok(actual) => info!("Scan: bands restored and verified: {actual:?}"),
            Err(e) => tracing::error!("Scan: band restore verification failed: {e}"),
        }
        if let Some(pri) = original_priority {
            if let Err(e) = modem.command(&format!("AT+QCFG=\"bandpri\",{pri}")).await {
                tracing::error!("Scan: failed to restore priority: {e}");
            }
        }

        // Force clean re-registration — but skip if tunnel reconnected (COPS kills QMI)
        if tunnel_stats.connected.load(Ordering::Relaxed) {
            info!("Scan: tunnel connected, skipping re-registration (bands restored)");
        } else {
            let _ = modem.command("AT+COPS=2").await;
            tokio::time::sleep(Duration::from_secs(1)).await;
            let _ = modem.command("AT+COPS=0").await;

            // Verify data path recovery (registration + IPv4)
            let start = Instant::now();
            let mut recovered = false;
            while start.elapsed() < Duration::from_secs(45) {
                if let Ok(resp) = modem.command("AT+CEREG?").await {
                    if let Ok(status) = crate::lte_watchdog::parse_cereg(&resp) {
                        if status.is_registered()
                            && crate::lte_watchdog::interface_has_ipv4(&interface)
                        {
                            recovered = true;
                            break;
                        }
                    }
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            if !recovered {
                // Last resort: interface restart
                warn!("Scan: no IPv4 after restore, restarting interface");
                let openwrt = crate::lte_watchdog::is_openwrt();
                crate::lte_watchdog::action_restart_interface(&interface, openwrt, None).await;
                tokio::time::sleep(Duration::from_secs(5)).await;
                if crate::lte_watchdog::interface_has_ipv4(&interface) {
                    recovered = true;
                }
            }
            if !recovered {
                warn!("Scan: data path did not recover after restore, watchdog will handle");
            }
        }

        // Mark completed (don't overwrite "aborted" state)
        {
            let mut state = lte_state.lock().await;
            if let Some(ref mut scan) = state.scan_status {
                if scan.state != "aborted" {
                    scan.state = "completed".into();
                    scan.completed_at = Some(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                    );
                }
                scan.current_band = None;
                scan.results = results;
            }
            state.save_lte_data(&data_dir);
        }

        info!("Band scan finished");
    })
}

/// Run a download speed test using curl. Returns bytes per second or None on failure.
/// Uses `--connect-timeout` to fail fast when there's no data connectivity (common after
/// modem re-registration when QMI data session hasn't come up yet).
pub async fn run_download_speed_test(url: &str, interface: &str) -> Option<u64> {
    let output = tokio::process::Command::new("curl")
        .args([
            "-o",
            "/dev/null",
            "-w",
            "%{speed_download}",
            "-s",
            "--connect-timeout",
            "5",
            "--max-time",
            "10",
            "--interface",
            interface,
            url,
        ])
        .output()
        .await
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Parse speed even on non-zero exit (e.g. timeout after partial download)
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let speed = stdout.trim().parse::<f64>().ok().map(|v| v.max(0.0) as u64);

    // Return None only if we got zero or no measurement at all
    match speed {
        Some(0) | None => None,
        s => s,
    }
}

/// Run an upload speed test using curl. Returns bytes per second or None on failure.
/// Creates a 2MB temp file and uploads it to measure throughput.
pub async fn run_upload_speed_test(url: &str, interface: &str) -> Option<u64> {
    // Create a 2MB temp file for upload
    let tmp_path = "/tmp/sctl-upload-test.bin";
    let dd_result = tokio::process::Command::new("dd")
        .args([
            "if=/dev/urandom",
            &format!("of={tmp_path}"),
            "bs=256k",
            "count=8",
        ])
        .stderr(std::process::Stdio::null())
        .output()
        .await;
    if dd_result.is_err() || !dd_result.as_ref().is_ok_and(|o| o.status.success()) {
        return None;
    }

    let output = tokio::process::Command::new("curl")
        .args([
            "-X",
            "POST",
            "--data-binary",
            &format!("@{tmp_path}"),
            "-o",
            "/dev/null",
            "-w",
            "%{speed_upload}",
            "-s",
            "--connect-timeout",
            "3",
            "--max-time",
            "10",
            "--interface",
            interface,
            url,
        ])
        .output()
        .await;

    // Clean up temp file
    let _ = tokio::fs::remove_file(tmp_path).await;

    let output = output.ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let speed = stdout.trim().parse::<f64>().ok().map(|v| v.max(0.0) as u64);

    match speed {
        Some(0) | None => None,
        s => s,
    }
}

// ── Parsers ──────────────────────────────────────────────────────────

/// Parse `AT+CSQ` response → RSSI in dBm.
fn parse_csq(response: &str) -> Result<i32, String> {
    let line = response
        .lines()
        .find(|l| l.contains("+CSQ:"))
        .ok_or_else(|| format!("no +CSQ in response: {}", response.trim()))?;

    let data = line.split(':').nth(1).ok_or("malformed +CSQ line")?.trim();

    let rssi_raw: i32 = data
        .split(',')
        .next()
        .ok_or("no RSSI value")?
        .trim()
        .parse()
        .map_err(|e| format!("bad RSSI: {e}"))?;

    if rssi_raw == 99 {
        return Err("RSSI not detectable (99)".into());
    }

    Ok(-113 + 2 * rssi_raw)
}

/// Decode QENG bandwidth code to MHz string.
fn decode_bandwidth(code: &str) -> Option<&'static str> {
    match code {
        "0" => Some("1.4"),
        "1" => Some("3"),
        "2" => Some("5"),
        "3" => Some("10"),
        "4" => Some("15"),
        "5" => Some("20"),
        _ => None,
    }
}

/// Decompose a hex cell ID into (enodeb_id, sector).
fn decompose_cell_id(hex_str: &str) -> (Option<u32>, Option<u8>) {
    let Ok(val) = u32::from_str_radix(hex_str, 16) else {
        return (None, None);
    };
    (Some(val >> 8), Some((val & 0xFF) as u8))
}

/// Parsed serving cell data from AT+QENG.
struct QengData {
    rsrp: Option<i32>,
    rsrq: Option<i32>,
    sinr: Option<f64>,
    cell_id: Option<String>,
    state: Option<String>,
    duplex: Option<String>,
    plmn: Option<String>,
    pci: Option<u16>,
    earfcn: Option<u32>,
    freq_band: Option<u16>,
    ul_bw: Option<String>,
    dl_bw: Option<String>,
    tac: Option<String>,
    enodeb_id: Option<u32>,
    sector: Option<u8>,
}

impl QengData {
    fn empty() -> Self {
        Self {
            rsrp: None,
            rsrq: None,
            sinr: None,
            cell_id: None,
            state: None,
            duplex: None,
            plmn: None,
            pci: None,
            earfcn: None,
            freq_band: None,
            ul_bw: None,
            dl_bw: None,
            tac: None,
            enodeb_id: None,
            sector: None,
        }
    }
}

/// Parse `AT+QENG="servingcell"` → full serving cell details.
fn parse_qeng(response: &str) -> QengData {
    fn parse_str(parts: &[&str], idx: usize) -> Option<String> {
        parts.get(idx).and_then(|s| {
            let s = s.trim_matches('"');
            if s.is_empty() || s == "-" {
                None
            } else {
                Some(s.to_string())
            }
        })
    }

    let Some(line) = response
        .lines()
        .find(|l| l.contains("+QENG:") && l.contains("LTE"))
    else {
        return QengData::empty();
    };

    let data = match line.split(':').nth(1) {
        Some(d) => d.trim(),
        None => return QengData::empty(),
    };

    let parts: Vec<&str> = data.split(',').map(str::trim).collect();

    // LTE FDD layout:
    // 0:"servingcell" 1:"NOCONN" 2:"LTE" 3:"FDD" 4:mcc 5:mnc 6:cellid 7:pcid
    // 8:earfcn 9:freq_band 10:ul_bw 11:dl_bw 12:tac 13:rsrp 14:rsrq 15:rssi 16:sinr 17:srxlev
    if parts.len() < 17 {
        return QengData::empty();
    }

    let cell_id = parse_str(&parts, 6);
    let (enodeb_id, sector) = cell_id.as_deref().map_or((None, None), decompose_cell_id);

    let mcc = parse_str(&parts, 4);
    let mnc = parse_str(&parts, 5);
    let plmn = match (mcc, mnc) {
        (Some(m), Some(n)) => Some(format!("{m}{n}")),
        _ => None,
    };

    QengData {
        rsrp: parts.get(13).and_then(|s| s.parse::<i32>().ok()),
        rsrq: parts.get(14).and_then(|s| s.parse::<i32>().ok()),
        sinr: parts.get(16).and_then(|s| s.parse::<f64>().ok()),
        cell_id,
        state: parse_str(&parts, 1),
        duplex: parse_str(&parts, 3),
        plmn,
        pci: parts.get(7).and_then(|s| s.parse::<u16>().ok()),
        earfcn: parts.get(8).and_then(|s| s.parse::<u32>().ok()),
        freq_band: parts.get(9).and_then(|s| s.parse::<u16>().ok()),
        ul_bw: parts
            .get(10)
            .and_then(|s| decode_bandwidth(s))
            .map(String::from),
        dl_bw: parts
            .get(11)
            .and_then(|s| decode_bandwidth(s))
            .map(String::from),
        tac: parse_str(&parts, 12),
        enodeb_id,
        sector,
    }
}

/// Parse `AT+QENG="neighbourcell"` response into neighbor cell list.
fn parse_neighbour(response: &str) -> Vec<NeighborCell> {
    let mut neighbors = Vec::new();

    for line in response.lines() {
        if !line.contains("+QENG:") || !line.contains("neighbourcell") {
            continue;
        }

        let Some(data) = line.split(':').nth(1) else {
            continue;
        };
        let data = data.trim();

        let parts: Vec<&str> = data
            .split(',')
            .map(|s| s.trim().trim_matches('"'))
            .collect();

        // Determine cell type from first field
        let cell_type = if data.contains("neighbourcell intra") {
            "intra"
        } else if data.contains("neighbourcell inter") {
            "inter"
        } else {
            continue;
        };

        // LTE neighbourcell layout:
        // "neighbourcell intra","LTE",earfcn,pcid,rsrq,rsrp,rssi,sinr,...
        // "neighbourcell inter","LTE",earfcn,pcid,rsrq,rsrp,rssi,sinr,...
        // Fields: 0="neighbourcell X" 1="LTE" 2=earfcn 3=pcid 4=rsrq 5=rsrp 6=rssi 7=sinr
        if parts.len() < 6 {
            continue;
        }

        let Some(earfcn) = parts.get(2).and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Some(pci) = parts.get(3).and_then(|s| s.parse::<u16>().ok()) else {
            continue;
        };

        neighbors.push(NeighborCell {
            earfcn,
            pci,
            rsrq: parts.get(4).and_then(|s| s.parse::<i32>().ok()),
            rsrp: parts.get(5).and_then(|s| s.parse::<i32>().ok()),
            rssi: parts.get(6).and_then(|s| s.parse::<i32>().ok()),
            sinr: parts.get(7).and_then(|s| s.parse::<f64>().ok()),
            cell_type: cell_type.to_string(),
        });
    }

    neighbors
}

/// Parse `AT+QNWINFO` → (technology, band). Operator comes from AT+COPS? instead.
fn parse_qnwinfo(response: &str) -> (Option<String>, Option<String>) {
    let Some(line) = response.lines().find(|l| l.contains("+QNWINFO:")) else {
        return (None, None);
    };

    let data = match line.split(':').nth(1) {
        Some(d) => d.trim(),
        None => return (None, None),
    };

    let parts: Vec<&str> = data
        .split(',')
        .map(|s| s.trim().trim_matches('"'))
        .collect();

    let technology = parts.first().map(|s| {
        if s.contains("LTE") {
            "LTE".to_string()
        } else {
            (*s).to_string()
        }
    });

    let band = parts.get(2).map(|s| {
        if let Some(rest) = s.strip_prefix("LTE BAND ") {
            format!("B{rest}")
        } else if let Some(rest) = s.strip_prefix("WCDMA BAND ") {
            format!("B{rest}")
        } else {
            (*s).to_string()
        }
    });

    (technology, band)
}

/// Parse `AT+COPS?` → operator name.
///
/// Response: `+COPS: 0,0,"ROGERS ROGERS",7`
fn parse_cops(response: &str) -> Option<String> {
    let line = response.lines().find(|l| l.contains("+COPS:"))?;
    let data = line.split(':').nth(1)?.trim();
    // Find the quoted operator name
    let start = data.find('"')? + 1;
    let end = data[start..].find('"')? + start;
    let name = data[start..end].trim();
    if name.is_empty() {
        return None;
    }
    // Clean up "ROGERS ROGERS" → "Rogers"
    let words: Vec<&str> = name.split_whitespace().collect();
    if words.len() >= 2 && words[0].eq_ignore_ascii_case(words[1]) {
        // Deduplicate "ROGERS ROGERS" → "Rogers"
        Some(titlecase(words[0]))
    } else {
        Some(titlecase(name))
    }
}

/// Parse `AT+QCFG="band"` → list of enabled LTE band numbers.
///
/// Response: `+QCFG: "band",0x260,0x1808,0x0`
/// The second hex value is the LTE band bitmask where bit (N-1) = Band N.
pub fn parse_band_config(response: &str) -> Vec<u16> {
    let Some(line) = response
        .lines()
        .find(|l| l.contains("+QCFG:") && l.contains("band"))
    else {
        return vec![];
    };
    let Some(data) = line.split(':').nth(1) else {
        return vec![];
    };
    let parts: Vec<&str> = data.split(',').map(str::trim).collect();
    // parts[0]="band" parts[1]=GSM parts[2]=LTE parts[3]=TDS
    let Some(lte_hex) = parts.get(2) else {
        return vec![];
    };
    let lte_hex = lte_hex
        .trim_matches('"')
        .trim_start_matches("0x")
        .trim_start_matches("0X");
    let Ok(val) = u128::from_str_radix(lte_hex, 16) else {
        return vec![];
    };

    let mut bands = Vec::new();
    for bit in 0u16..128 {
        if val & (1u128 << bit) != 0 {
            bands.push(bit + 1);
        }
    }
    bands
}

/// Parse `AT+QCFG="bandpri"` → priority band number.
///
/// Response: `+QCFG: "bandpri",4`
pub fn parse_bandpri(response: &str) -> Option<u16> {
    let line = response
        .lines()
        .find(|l| l.contains("+QCFG:") && l.contains("bandpri"))?;
    let data = line.split(':').nth(1)?.trim();
    let parts: Vec<&str> = data.split(',').map(str::trim).collect();
    parts.get(1)?.trim_matches('"').parse::<u16>().ok()
}

/// Title-case a string: "ROGERS" → "Rogers".
fn titlecase(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => {
            let upper: String = c.to_uppercase().collect();
            upper + &chars.as_str().to_lowercase()
        }
    }
}

/// Parse a simple one-line AT response (e.g. `AT+GSN` → `866834049460285`).
fn parse_simple_line(response: &str) -> Option<String> {
    response
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("AT") && *l != "OK" && !l.contains("ERROR"))
        .map(String::from)
}

/// Parse `AT+QCCID` → ICCID.
fn parse_qccid(response: &str) -> Option<String> {
    let line = response.lines().find(|l| l.contains("+QCCID:"))?;
    let iccid = line.split(':').nth(1)?.trim();
    if iccid.is_empty() {
        None
    } else {
        Some(iccid.to_string())
    }
}

/// Parse `AT+CIMI` → IMSI. Response is a bare 15-digit number (no prefix).
fn parse_cimi(response: &str) -> Option<String> {
    response
        .lines()
        .map(str::trim)
        .find(|l| l.len() >= 6 && l.len() <= 15 && l.chars().all(|c| c.is_ascii_digit()))
        .map(String::from)
}

/// Parse `AT+CGDCONT?` → APN from default PDP context (CID 1).
/// Response format: `+CGDCONT: 1,"IP","apn.name",,0,0`
fn parse_cgdcont(response: &str) -> Option<String> {
    for line in response.lines() {
        let line = line.trim();
        if !line.starts_with("+CGDCONT:") {
            continue;
        }
        // +CGDCONT: <cid>,"<type>","<apn>",...
        let after_colon = line.split(':').nth(1)?.trim();
        let parts: Vec<&str> = after_colon.splitn(4, ',').collect();
        if parts.len() < 3 {
            continue;
        }
        let cid: u8 = parts[0].trim().parse().ok()?;
        if cid != 1 {
            continue;
        }
        let apn = parts[2].trim().trim_matches('"');
        if apn.is_empty() {
            return None;
        }
        return Some(apn.to_string());
    }
    None
}

/// Read static modem identity (IMEI, model, firmware, ICCID, IMSI).
#[allow(clippy::similar_names)]
async fn read_modem_info(modem: &Modem) -> ModemInfo {
    let model = match modem.command("AT+CGMM").await {
        Ok(resp) => parse_simple_line(&resp),
        Err(_) => None,
    };
    let firmware = match modem.command("AT+CGMR").await {
        Ok(resp) => parse_simple_line(&resp),
        Err(_) => None,
    };
    let imei = match modem.command("AT+GSN").await {
        Ok(resp) => parse_simple_line(&resp),
        Err(_) => None,
    };
    let iccid = match modem.command("AT+QCCID").await {
        Ok(resp) => parse_qccid(&resp),
        Err(_) => None,
    };
    let imsi = match modem.command("AT+CIMI").await {
        Ok(resp) => parse_cimi(&resp),
        Err(_) => None,
    };

    ModemInfo {
        model,
        firmware,
        imei,
        iccid,
        imsi,
    }
}

/// AT polling mode — determines how aggressively we poll the modem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AtPollMode {
    /// No active data path — full 7-command poll, no inter-command delay.
    Full,
    /// Data path active — 2 essential commands (CSQ + QENG) with spacing.
    Gentle,
    /// Disruption detected — single CSQ only, long interval.
    Cautious,
    /// Multiple disruptions in cautious mode — full suppression (legacy behavior).
    Suppressed,
}

/// State for the AT/QMI coexistence test mode (Phase 1 diagnostics).
struct AtTestState {
    level: u8,
    consecutive_ok: u32,
    disruptions: u64,
}

/// Tracks polling health for self-adjusting backoff.
struct AtPollingHealth {
    mode: AtPollMode,
    consecutive_ok: u32,
    cautious_disruptions: u32,
}

impl AtPollingHealth {
    fn new() -> Self {
        Self {
            mode: AtPollMode::Full,
            consecutive_ok: 0,
            cautious_disruptions: 0,
        }
    }

    fn record_ok(&mut self) {
        self.consecutive_ok += 1;
        match self.mode {
            AtPollMode::Cautious if self.consecutive_ok >= 5 => {
                info!(
                    "AT health: cautious→gentle after {} ok polls",
                    self.consecutive_ok
                );
                self.mode = AtPollMode::Gentle;
                self.consecutive_ok = 0;
                self.cautious_disruptions = 0;
            }
            _ => {}
        }
    }

    fn record_disruption(&mut self) {
        self.consecutive_ok = 0;
        match self.mode {
            AtPollMode::Gentle => {
                warn!("AT health: gentle→cautious (disruption detected)");
                self.mode = AtPollMode::Cautious;
            }
            AtPollMode::Cautious => {
                self.cautious_disruptions += 1;
                if self.cautious_disruptions >= 2 {
                    warn!("AT health: cautious→suppressed (2 disruptions in cautious)");
                    self.mode = AtPollMode::Suppressed;
                }
            }
            _ => {}
        }
    }
}

/// Spawn the background LTE signal poller. Returns a `JoinHandle` for abort on shutdown.
///
/// The poller uses data-path-aware polling modes instead of binary tunnel suppression:
/// - **Full**: when no QMI data session is active, polls all 7 AT commands
/// - **Gentle**: when data path is active, polls only CSQ + QENG with spacing
/// - **Cautious**: after a disruption, single CSQ with long interval
/// - **Suppressed**: fallback to legacy full suppression if cautious also fails
#[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
pub fn spawn_lte_poller(
    config: LteConfig,
    modem: Modem,
    lte_state: Arc<Mutex<LteState>>,
    session_events: broadcast::Sender<serde_json::Value>,
    mut modem_rx: watch::Receiver<Modem>,
    data_dir: String,
    tunnel_stats: Arc<TunnelStats>,
    poll_notify: Arc<Notify>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let interval = tokio::time::Duration::from_secs(config.poll_interval_secs);
        let gentle_interval = tokio::time::Duration::from_secs(config.active_poll_interval_secs);
        let cautious_interval = tokio::time::Duration::from_secs(300);
        let inter_cmd_delay = tokio::time::Duration::from_millis(config.inter_command_delay_ms);
        let mut modem = modem;
        let mut polls_since_save: u32 = 0;
        let mut at_health = AtPollingHealth::new();
        let mut at_test = AtTestState {
            level: 0,
            consecutive_ok: 0,
            disruptions: 0,
        };

        // Read static modem info once at startup
        let modem_info = read_modem_info(&modem).await;
        info!(
            "LTE modem: {} {} IMEI={}",
            modem_info.model.as_deref().unwrap_or("?"),
            modem_info.firmware.as_deref().unwrap_or("?"),
            modem_info.imei.as_deref().unwrap_or("?"),
        );
        lte_state.lock().await.modem = Some(modem_info);

        let mut consecutive_channel_errors: u32 = 0;

        loop {
            // Pick up refreshed modem handle if watchdog re-opened it
            if modem_rx.has_changed().unwrap_or(false) {
                modem = modem_rx.borrow_and_update().clone();
                consecutive_channel_errors = 0;
                info!("LTE poller: modem handle refreshed");
            }

            // Wait for watchdog to refresh dead modem handle (avoid racing USB cycle)
            if !modem.is_alive() || consecutive_channel_errors >= 3 {
                warn!(
                    "LTE poller: modem handle dead (alive={}, channel_errors={}), waiting for refresh",
                    modem.is_alive(),
                    consecutive_channel_errors
                );
                loop {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    if modem_rx.has_changed().unwrap_or(false) {
                        modem = modem_rx.borrow_and_update().clone();
                        consecutive_channel_errors = 0;
                        info!("LTE poller: modem handle refreshed by watchdog");
                        break;
                    }
                    // Watchdog sender dropped — no one will broadcast, self-recover
                    if modem_rx.has_changed().is_err() {
                        let actual_path = crate::modem::detect_quectel_at_port(&config.device);
                        match crate::modem::Modem::open(&actual_path) {
                            Ok(new_modem) => {
                                info!("LTE poller: modem re-opened (no watchdog) at {actual_path}");
                                modem = new_modem;
                                consecutive_channel_errors = 0;
                            }
                            Err(e) => {
                                warn!("LTE poller: modem re-open failed: {e}");
                            }
                        }
                        break;
                    }
                    // Check if handle recovered on its own
                    if modem.is_alive() {
                        consecutive_channel_errors = 0;
                        break;
                    }
                }
            }

            // Determine polling mode based on data path state, not tunnel state.
            // If the LTE interface has an IPv4 address, a QMI data bearer is active
            // and we should be gentle with AT commands to avoid disrupting it.
            let data_active = crate::lte_watchdog::interface_has_ipv4(&config.interface);
            let poll_mode = if !data_active {
                AtPollMode::Full
            } else if at_health.mode == AtPollMode::Suppressed {
                AtPollMode::Suppressed
            } else if at_health.mode == AtPollMode::Full {
                // Never use Full mode when data path is active — it sends all 7
                // AT commands which disrupts the QMI data bearer on the EC25.
                AtPollMode::Gentle
            } else {
                at_health.mode
            };

            // AT test mode: graduated testing when data path is active
            if config.at_test_mode && data_active && poll_mode != AtPollMode::Suppressed {
                let pre_pong = tunnel_stats.last_pong_age_ms.load(Ordering::Relaxed);
                let pre_connected = tunnel_stats.connected.load(Ordering::Relaxed);

                let test_cmds: Vec<&str> = match at_test.level {
                    0 => vec!["AT"],
                    1 => vec!["AT+CSQ"],
                    2 => vec!["AT+CSQ", "AT+QENG=\"servingcell\""],
                    // Level 3+: all 7 commands (level 3 = with spacing, level 4 = burst)
                    _ => vec![
                        "AT+CSQ",
                        "AT+QENG=\"servingcell\"",
                        "AT+QNWINFO",
                        "AT+COPS?",
                        "AT+QENG=\"neighbourcell\"",
                        "AT+QCFG=\"band\"",
                        "AT+QCFG=\"bandpri\"",
                    ],
                };
                let use_spacing = at_test.level <= 3;

                for cmd in &test_cmds {
                    let cmd_start = Instant::now();
                    match modem.command(cmd).await {
                        Ok(_) => {
                            #[allow(clippy::cast_possible_truncation)]
                            let latency_ms = cmd_start.elapsed().as_millis() as u64;
                            let pong = tunnel_stats.last_pong_age_ms.load(Ordering::Relaxed);
                            let connected = tunnel_stats.connected.load(Ordering::Relaxed);
                            info!(
                                "AT_TEST level={} cmd=\"{cmd}\" tunnel_ok={connected} pong_age_ms={pong} latency_ms={latency_ms}",
                                at_test.level,
                            );
                        }
                        Err(e) => {
                            info!(
                                "AT_TEST level={} cmd=\"{cmd}\" error=\"{e}\"",
                                at_test.level
                            );
                        }
                    }
                    if use_spacing && test_cmds.len() > 1 {
                        tokio::time::sleep(inter_cmd_delay).await;
                    }
                }

                // Check health after commands
                tokio::time::sleep(Duration::from_secs(5)).await;
                let post_connected = tunnel_stats.connected.load(Ordering::Relaxed);
                let post_pong = tunnel_stats.last_pong_age_ms.load(Ordering::Relaxed);

                let disrupted =
                    (pre_connected && !post_connected) || (pre_pong < 5000 && post_pong > 10_000);

                if disrupted {
                    at_test.disruptions += 1;
                    info!(
                        "AT_TEST DISRUPTION level={} pong_before={pre_pong}ms pong_after={post_pong}ms connected={post_connected} DEMOTE->0 total_disruptions={}",
                        at_test.level, at_test.disruptions,
                    );
                    at_test.level = 0;
                    at_test.consecutive_ok = 0;
                } else {
                    at_test.consecutive_ok += 1;
                    if at_test.consecutive_ok >= 3 && at_test.level < 4 {
                        info!("AT_TEST PROMOTE {}->{}", at_test.level, at_test.level + 1,);
                        at_test.level += 1;
                        at_test.consecutive_ok = 0;
                    }
                }

                // In test mode, use gentle interval for next tick
                tokio::time::sleep(gentle_interval).await;
                continue;
            }

            // Suppressed mode: skip all AT commands (legacy fallback)
            if poll_mode == AtPollMode::Suppressed {
                tokio::select! {
                    () = tokio::time::sleep(interval) => {}
                    () = poll_notify.notified() => {
                        debug!("LTE poller: on-demand poll skipped (suppressed mode)");
                    }
                }
                // Check if data path dropped — if so, we can resume
                if !crate::lte_watchdog::interface_has_ipv4(&config.interface) {
                    info!("LTE poller: data path down, exiting suppressed mode");
                    at_health.mode = AtPollMode::Full;
                    at_health.consecutive_ok = 0;
                    at_health.cautious_disruptions = 0;
                }
                continue;
            }

            // Snapshot pre-command tunnel health for disruption detection
            let pre_pong = if data_active {
                Some(tunnel_stats.last_pong_age_ms.load(Ordering::Relaxed))
            } else {
                None
            };
            let pre_connected = if data_active {
                tunnel_stats.connected.load(Ordering::Relaxed)
            } else {
                false
            };

            // 1. AT+CSQ for RSSI
            let rssi_result = match modem.command("AT+CSQ").await {
                Ok(resp) => {
                    consecutive_channel_errors = 0;
                    parse_csq(&resp)
                }
                Err(e) => {
                    if e.contains("I/O thread gone") || e.contains("reply channel dropped") {
                        consecutive_channel_errors += 1;
                    }
                    Err(e)
                }
            };

            let rssi_dbm = match rssi_result {
                Ok(v) => v,
                Err(e) => {
                    debug!("LTE: CSQ failed: {e}");
                    {
                        let lock_started = Instant::now();
                        let mut state = lte_state.lock().await;
                        #[allow(clippy::cast_possible_truncation)]
                        let lock_wait_ms = lock_started.elapsed().as_millis() as u64;
                        if lock_wait_ms >= 100 {
                            warn!(
                                lock_wait_ms,
                                "LTE poller: slow lte_state lock for error update"
                            );
                        }
                        state.errors_total += 1;
                        state.last_error = Some(e);
                    }
                    tokio::time::sleep(interval).await;
                    continue;
                }
            };

            // Inter-command delay when data path is active
            if data_active {
                tokio::time::sleep(inter_cmd_delay).await;
            }

            // 2. AT+QENG for RSRP/RSRQ/SINR/cell_id + serving cell details
            let qeng = match modem.command("AT+QENG=\"servingcell\"").await {
                Ok(resp) => parse_qeng(&resp),
                Err(e) => {
                    debug!("LTE: QENG failed: {e}");
                    QengData::empty()
                }
            };

            // Commands 3-7: only in Full mode (no active data path)
            let (technology, band, operator, neighbors, band_config) =
                if poll_mode == AtPollMode::Full {
                    // 3. AT+QNWINFO for band/technology
                    let (technology, band) = match modem.command("AT+QNWINFO").await {
                        Ok(resp) => parse_qnwinfo(&resp),
                        Err(e) => {
                            debug!("LTE: QNWINFO failed: {e}");
                            (None, None)
                        }
                    };

                    // 4. AT+COPS? for operator name
                    let operator = match modem.command("AT+COPS?").await {
                        Ok(resp) => parse_cops(&resp),
                        Err(e) => {
                            debug!("LTE: COPS failed: {e}");
                            None
                        }
                    };

                    // 5. AT+QENG="neighbourcell" for neighbor cells
                    let neighbors = match modem.command("AT+QENG=\"neighbourcell\"").await {
                        Ok(resp) => parse_neighbour(&resp),
                        Err(e) => {
                            debug!("LTE: neighbour failed: {e}");
                            vec![]
                        }
                    };

                    // 6. AT+QCFG="band" + "bandpri" for band configuration
                    let band_config = {
                        let enabled = match modem.command("AT+QCFG=\"band\"").await {
                            Ok(resp) => parse_band_config(&resp),
                            Err(e) => {
                                debug!("LTE: band config failed: {e}");
                                vec![]
                            }
                        };
                        let priority = match modem.command("AT+QCFG=\"bandpri\"").await {
                            Ok(resp) => parse_bandpri(&resp),
                            Err(e) => {
                                debug!("LTE: bandpri failed: {e}");
                                None
                            }
                        };
                        if enabled.is_empty() {
                            None
                        } else {
                            Some(BandConfig {
                                enabled_bands: enabled,
                                priority_band: priority,
                            })
                        }
                    };

                    (technology, band, operator, neighbors, band_config)
                } else {
                    // Gentle/Cautious: reuse cached values from last full poll
                    let cached = lte_state.lock().await;
                    let prev = cached.signal.as_ref();
                    (
                        prev.and_then(|s| s.technology.clone()),
                        prev.and_then(|s| s.band.clone()),
                        prev.and_then(|s| s.operator.clone()),
                        prev.map(|s| s.neighbors.clone()).unwrap_or_default(),
                        prev.and_then(|s| s.band_config.clone()),
                    )
                };

            // Disruption detection: check if AT commands killed the data path
            if let Some(pre) = pre_pong {
                let post_pong = tunnel_stats.last_pong_age_ms.load(Ordering::Relaxed);
                let post_connected = tunnel_stats.connected.load(Ordering::Relaxed);
                if (pre_connected && !post_connected) || (pre < 5000 && post_pong > 10_000) {
                    warn!(
                        "LTE poller: AT commands disrupted data path (pong {pre}→{post_pong}ms, connected {pre_connected}→{post_connected}), backing off"
                    );
                    at_health.record_disruption();
                }
            }

            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            let signal_bars = compute_signal_bars(qeng.rsrp, rssi_dbm);

            let signal = LteSignal {
                rssi_dbm,
                rsrp: qeng.rsrp,
                rsrq: qeng.rsrq,
                sinr: qeng.sinr,
                band,
                operator,
                technology,
                cell_id: qeng.cell_id,
                pci: qeng.pci,
                earfcn: qeng.earfcn,
                freq_band: qeng.freq_band,
                tac: qeng.tac,
                plmn: qeng.plmn,
                enodeb_id: qeng.enodeb_id,
                sector: qeng.sector,
                ul_bw_mhz: qeng.ul_bw,
                dl_bw_mhz: qeng.dl_bw,
                connection_state: qeng.state,
                duplex: qeng.duplex,
                neighbors,
                band_config,
                signal_bars,
                recorded_at: now,
            };

            debug!(
                "LTE: RSSI={} RSRP={:?} SINR={:?} bars={} band={:?} op={:?} pci={:?} earfcn={:?}",
                signal.rssi_dbm,
                signal.rsrp,
                signal.sinr,
                signal.signal_bars,
                signal.band,
                signal.operator,
                signal.pci,
                signal.earfcn,
            );

            let _ = session_events.send(serde_json::json!({
                "type": "lte.signal",
                "rssi_dbm": signal.rssi_dbm,
                "signal_bars": signal.signal_bars,
                "band": signal.band,
                "operator": signal.operator,
                "pci": signal.pci,
                "earfcn": signal.earfcn,
                "freq_band": signal.freq_band,
                "enodeb_id": signal.enodeb_id,
                "tac": signal.tac,
                "connection_state": signal.connection_state,
            }));

            {
                let lock_started = Instant::now();
                let mut state = lte_state.lock().await;
                #[allow(clippy::cast_possible_truncation)]
                let lock_wait_ms = lock_started.elapsed().as_millis() as u64;
                if lock_wait_ms >= 100 {
                    warn!(
                        lock_wait_ms,
                        "LTE poller: slow lte_state lock for signal update"
                    );
                }
                update_band_history(&mut state.band_history, &signal);
                state.signal = Some(signal);
                // Save band history every 10 polls (~10 min at 60s interval)
                polls_since_save += 1;
                if polls_since_save >= 10 {
                    polls_since_save = 0;
                    state.save_lte_data(&data_dir);
                }
            }

            // Record successful poll for health tracking
            if data_active {
                at_health.record_ok();
            }

            // Wait for next poll — use mode-appropriate interval.
            // On-demand polls (API refresh) always break out immediately.
            let wait_duration = match poll_mode {
                AtPollMode::Gentle => gentle_interval,
                AtPollMode::Cautious => cautious_interval,
                AtPollMode::Full | AtPollMode::Suppressed => interval,
            };
            tokio::select! {
                () = tokio::time::sleep(wait_duration) => {}
                () = poll_notify.notified() => {
                    debug!("LTE poller: on-demand poll triggered");
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_csq_valid() {
        let response = "+CSQ: 15,99\r\nOK";
        assert_eq!(parse_csq(response).unwrap(), -83);
    }

    #[test]
    fn test_parse_csq_not_detectable() {
        assert!(parse_csq("+CSQ: 99,99\r\nOK").is_err());
    }

    #[test]
    fn test_parse_csq_no_response() {
        assert!(parse_csq("ERROR\r\n").is_err());
    }

    #[test]
    fn test_parse_qeng_lte() {
        let response = "+QENG: \"servingcell\",\"NOCONN\",\"LTE\",\"FDD\",302,720,101A901,266,2050,4,5,5,61E4,-102,-11,-70,10,-\r\nOK";
        let q = parse_qeng(response);
        assert_eq!(q.rsrp, Some(-102));
        assert_eq!(q.rsrq, Some(-11));
        assert_eq!(q.sinr, Some(10.0));
        assert_eq!(q.cell_id.as_deref(), Some("101A901"));
        assert_eq!(q.state.as_deref(), Some("NOCONN"));
        assert_eq!(q.duplex.as_deref(), Some("FDD"));
        assert_eq!(q.plmn.as_deref(), Some("302720"));
        assert_eq!(q.pci, Some(266));
        assert_eq!(q.earfcn, Some(2050));
        assert_eq!(q.freq_band, Some(4));
        assert_eq!(q.ul_bw.as_deref(), Some("20"));
        assert_eq!(q.dl_bw.as_deref(), Some("20"));
        assert_eq!(q.tac.as_deref(), Some("61E4"));
    }

    #[test]
    fn test_parse_qeng_no_lte() {
        let q = parse_qeng("OK\r\n");
        assert!(q.rsrp.is_none());
        assert!(q.pci.is_none());
    }

    #[test]
    fn test_parse_qeng_enodeb_decomposition() {
        let response = "+QENG: \"servingcell\",\"NOCONN\",\"LTE\",\"FDD\",302,720,101A901,266,2050,4,5,5,61E4,-102,-11,-70,10,-\r\nOK";
        let q = parse_qeng(response);
        // 0x101A901 = 16885009 decimal
        // enodeb_id = 0x101A901 >> 8 = 0x101A9 = 65961
        // sector = 0x101A901 & 0xFF = 0x01 = 1
        assert_eq!(q.enodeb_id, Some(65961));
        assert_eq!(q.sector, Some(1));
    }

    #[test]
    fn test_decode_bandwidth() {
        assert_eq!(decode_bandwidth("0"), Some("1.4"));
        assert_eq!(decode_bandwidth("1"), Some("3"));
        assert_eq!(decode_bandwidth("2"), Some("5"));
        assert_eq!(decode_bandwidth("3"), Some("10"));
        assert_eq!(decode_bandwidth("4"), Some("15"));
        assert_eq!(decode_bandwidth("5"), Some("20"));
        assert_eq!(decode_bandwidth("6"), None);
        assert_eq!(decode_bandwidth(""), None);
    }

    #[test]
    fn test_parse_neighbour_intra() {
        let response = "+QENG: \"neighbourcell intra\",\"LTE\",2050,305,-12,-98,-68,8\r\nOK";
        let n = parse_neighbour(response);
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].earfcn, 2050);
        assert_eq!(n[0].pci, 305);
        assert_eq!(n[0].rsrq, Some(-12));
        assert_eq!(n[0].rsrp, Some(-98));
        assert_eq!(n[0].rssi, Some(-68));
        assert_eq!(n[0].sinr, Some(8.0));
        assert_eq!(n[0].cell_type, "intra");
    }

    #[test]
    fn test_parse_neighbour_inter() {
        let response = "+QENG: \"neighbourcell inter\",\"LTE\",5110,124,-15,-105,-75,3\r\nOK";
        let n = parse_neighbour(response);
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].earfcn, 5110);
        assert_eq!(n[0].pci, 124);
        assert_eq!(n[0].rsrp, Some(-105));
        assert_eq!(n[0].cell_type, "inter");
    }

    #[test]
    fn test_parse_neighbour_mixed() {
        let response = "\
+QENG: \"neighbourcell intra\",\"LTE\",2050,305,-12,-98,-68,8\r\n\
+QENG: \"neighbourcell intra\",\"LTE\",2050,410,-14,-101,-72,5\r\n\
+QENG: \"neighbourcell inter\",\"LTE\",5110,124,-15,-105,-75,3\r\n\
OK";
        let n = parse_neighbour(response);
        assert_eq!(n.len(), 3);
        assert_eq!(n[0].cell_type, "intra");
        assert_eq!(n[1].cell_type, "intra");
        assert_eq!(n[2].cell_type, "inter");
        assert_eq!(n[0].pci, 305);
        assert_eq!(n[1].pci, 410);
        assert_eq!(n[2].pci, 124);
    }

    #[test]
    fn test_parse_qnwinfo() {
        let response = "+QNWINFO: \"FDD LTE\",\"302720\",\"LTE BAND 4\",2050\r\nOK";
        let (tech, band) = parse_qnwinfo(response);
        assert_eq!(tech.as_deref(), Some("LTE"));
        assert_eq!(band.as_deref(), Some("B4"));
    }

    #[test]
    fn test_parse_qnwinfo_wcdma() {
        let response = "+QNWINFO: \"WCDMA\",\"Rogers\",\"WCDMA BAND 2\",412\r\nOK";
        let (tech, band) = parse_qnwinfo(response);
        assert_eq!(tech.as_deref(), Some("WCDMA"));
        assert_eq!(band.as_deref(), Some("B2"));
    }

    #[test]
    fn test_parse_cops_rogers() {
        let response = "\r\n+COPS: 0,0,\"ROGERS ROGERS\",7\r\n\r\nOK\r\n";
        assert_eq!(parse_cops(response).as_deref(), Some("Rogers"));
    }

    #[test]
    fn test_parse_cops_normal() {
        let response = "+COPS: 0,0,\"T-Mobile\",7\r\nOK";
        assert_eq!(parse_cops(response).as_deref(), Some("T-mobile"));
    }

    #[test]
    fn test_parse_cops_no_match() {
        assert!(parse_cops("OK\r\n").is_none());
    }

    #[test]
    fn test_parse_simple_line_imei() {
        let response = "\r\n866834049460285\r\n\r\nOK\r\n";
        assert_eq!(
            parse_simple_line(response).as_deref(),
            Some("866834049460285")
        );
    }

    #[test]
    fn test_parse_simple_line_model() {
        let response = "\r\nEC25\r\n\r\nOK\r\n";
        assert_eq!(parse_simple_line(response).as_deref(), Some("EC25"));
    }

    #[test]
    fn test_parse_qccid() {
        let response = "\r\n+QCCID: 89302720554115293655\r\n\r\nOK\r\n";
        assert_eq!(
            parse_qccid(response).as_deref(),
            Some("89302720554115293655")
        );
    }

    #[test]
    fn test_parse_band_config() {
        let response = "+QCFG: \"band\",0x260,0x1808,0x0\r\nOK";
        let bands = parse_band_config(response);
        assert_eq!(bands, vec![4, 12, 13]);
    }

    #[test]
    fn test_parse_band_config_all_bands() {
        // 0xFF = bits 0-7 = bands 1-8
        let response = "+QCFG: \"band\",0x0,0xFF,0x0\r\nOK";
        let bands = parse_band_config(response);
        assert_eq!(bands, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn test_parse_band_config_empty() {
        assert!(parse_band_config("OK\r\n").is_empty());
        assert!(parse_band_config("ERROR\r\n").is_empty());
    }

    #[test]
    fn test_parse_bandpri() {
        let response = "+QCFG: \"bandpri\",4\r\nOK";
        assert_eq!(parse_bandpri(response), Some(4));
    }

    #[test]
    fn test_parse_bandpri_none() {
        assert_eq!(parse_bandpri("OK\r\n"), None);
        assert_eq!(parse_bandpri("ERROR\r\n"), None);
    }

    #[test]
    fn test_signal_bars() {
        assert_eq!(compute_signal_bars(Some(-75), -50), 5);
        assert_eq!(compute_signal_bars(Some(-85), -50), 4);
        assert_eq!(compute_signal_bars(Some(-95), -50), 3);
        assert_eq!(compute_signal_bars(Some(-105), -50), 2);
        assert_eq!(compute_signal_bars(Some(-115), -50), 1);
        assert_eq!(compute_signal_bars(None, -60), 5);
        assert_eq!(compute_signal_bars(None, -90), 2);
    }

    #[test]
    fn test_earfcn_to_band() {
        assert_eq!(earfcn_to_band(2050), Some(4)); // B4
        assert_eq!(earfcn_to_band(5110), Some(12)); // B12
        assert_eq!(earfcn_to_band(5230), Some(13)); // B13
        assert_eq!(earfcn_to_band(0), Some(1)); // B1 start
        assert_eq!(earfcn_to_band(599), Some(1)); // B1 end
        assert_eq!(earfcn_to_band(600), Some(2)); // B2 start
        assert_eq!(earfcn_to_band(66436), Some(66)); // B66 start
        assert_eq!(earfcn_to_band(68586), Some(71)); // B71 start
        assert_eq!(earfcn_to_band(99999), None); // out of range
    }

    #[test]
    fn test_bands_to_hex() {
        assert_eq!(bands_to_hex(&[4, 12, 13]), "1808");
        assert_eq!(bands_to_hex(&[1]), "1");
        assert_eq!(bands_to_hex(&[1, 2, 3]), "7");
        assert_eq!(bands_to_hex(&[]), "0");
    }

    #[test]
    fn test_bands_to_hex_roundtrip() {
        // Parse hex → bands → hex should round-trip
        let response = "+QCFG: \"band\",0x260,0x1808,0x0\r\nOK";
        let bands = parse_band_config(response);
        assert_eq!(bands_to_hex(&bands), "1808");
    }

    #[test]
    fn test_update_band_history_serving() {
        let mut history = HashMap::new();
        let signal = LteSignal {
            rssi_dbm: -85,
            rsrp: Some(-97),
            rsrq: Some(-11),
            sinr: Some(10.0),
            band: Some("B4".into()),
            operator: Some("Rogers".into()),
            technology: Some("LTE".into()),
            cell_id: Some("101A901".into()),
            pci: Some(266),
            earfcn: Some(2050),
            freq_band: Some(4),
            tac: Some("61E4".into()),
            plmn: Some("302720".into()),
            enodeb_id: Some(65961),
            sector: Some(1),
            ul_bw_mhz: Some("20".into()),
            dl_bw_mhz: Some("20".into()),
            connection_state: Some("NOCONN".into()),
            duplex: Some("FDD".into()),
            neighbors: vec![],
            band_config: None,
            signal_bars: 3,
            recorded_at: 1000,
        };

        update_band_history(&mut history, &signal);
        assert_eq!(history.len(), 1);
        let entry = history.get(&4).unwrap();
        assert_eq!(entry.band, 4);
        assert_eq!(entry.best_rsrp, -97);
        assert_eq!(entry.latest_rsrp, -97);
        assert_eq!(entry.observation_count, 1);
        assert!(entry.recent[0].serving);
    }

    #[test]
    fn test_update_band_history_with_neighbors() {
        let mut history = HashMap::new();
        let signal = LteSignal {
            rssi_dbm: -85,
            rsrp: Some(-97),
            rsrq: Some(-11),
            sinr: Some(10.0),
            band: Some("B4".into()),
            operator: None,
            technology: None,
            cell_id: None,
            pci: Some(266),
            earfcn: Some(2050),
            freq_band: Some(4),
            tac: None,
            plmn: None,
            enodeb_id: None,
            sector: None,
            ul_bw_mhz: None,
            dl_bw_mhz: None,
            connection_state: None,
            duplex: None,
            neighbors: vec![NeighborCell {
                earfcn: 5110,
                pci: 124,
                rsrp: Some(-105),
                rsrq: Some(-15),
                rssi: Some(-75),
                sinr: Some(3.0),
                cell_type: "inter".into(),
            }],
            band_config: None,
            signal_bars: 3,
            recorded_at: 1000,
        };

        update_band_history(&mut history, &signal);
        assert_eq!(history.len(), 2);
        assert!(history.contains_key(&4));
        assert!(history.contains_key(&12));
        let b12 = history.get(&12).unwrap();
        assert_eq!(b12.best_rsrp, -105);
        assert!(!b12.recent[0].serving);
    }

    // -----------------------------------------------------------------------
    // SIM change / APN tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_cimi() {
        // Typical IMSI response
        assert_eq!(
            parse_cimi("\r\n302720123456789\r\n\r\nOK\r\n").as_deref(),
            Some("302720123456789")
        );
        // EMnify IMSI
        assert_eq!(
            parse_cimi("\r\n295050907578917\r\n\r\nOK\r\n").as_deref(),
            Some("295050907578917")
        );
    }

    #[test]
    fn test_parse_cimi_no_sim() {
        assert!(parse_cimi("+CME ERROR: 10\r\n").is_none());
        assert!(parse_cimi("\r\nOK\r\n").is_none());
        assert!(parse_cimi("").is_none());
    }

    #[test]
    fn test_parse_cgdcont() {
        // Standard response with CID 1
        let resp = "\r\n+CGDCONT: 1,\"IP\",\"em\",,0,0\r\n\r\nOK\r\n";
        assert_eq!(parse_cgdcont(resp).as_deref(), Some("em"));

        // Multiple CIDs — should pick CID 1
        let resp =
            "+CGDCONT: 1,\"IP\",\"ltemobile.apn\",,0,0\r\n+CGDCONT: 2,\"IP\",\"ims\",,0,0\r\n";
        assert_eq!(parse_cgdcont(resp).as_deref(), Some("ltemobile.apn"));

        // Only CID 2, no CID 1
        let resp = "+CGDCONT: 2,\"IP\",\"ims\",,0,0\r\n";
        assert!(parse_cgdcont(resp).is_none());

        // Empty APN
        let resp = "+CGDCONT: 1,\"IP\",\"\",,0,0\r\n";
        assert!(parse_cgdcont(resp).is_none());
    }

    #[test]
    fn test_lookup_apn_by_imsi() {
        // EMnify
        assert_eq!(lookup_apn_by_imsi("295050907578917"), Some("em"));
        // Rogers
        assert_eq!(lookup_apn_by_imsi("302720123456789"), Some("ltemobile.apn"));
        // Bell
        assert_eq!(lookup_apn_by_imsi("302610000000000"), Some("pda.bell.ca"));
        // Unknown carrier
        assert_eq!(lookup_apn_by_imsi("999999999999999"), None);
        // Short IMSI (shouldn't match anything)
        assert_eq!(lookup_apn_by_imsi("302"), None);
    }

    #[test]
    fn test_sim_state_serde_roundtrip() {
        let state = SimState {
            iccid: "89302720554115293655".to_string(),
            imsi: Some("302720123456789".to_string()),
            first_seen: 1_700_000_000,
            last_seen: 1_700_000_060,
        };
        let json = serde_json::to_string(&state).unwrap();
        let parsed: SimState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.iccid, state.iccid);
        assert_eq!(parsed.imsi, state.imsi);
        assert_eq!(parsed.first_seen, state.first_seen);
        assert_eq!(parsed.last_seen, state.last_seen);
    }
}
