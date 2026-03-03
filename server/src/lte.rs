//! LTE signal quality monitoring via Quectel modem AT commands.
//!
//! When `[lte]` is present in the config, a background poller queries the modem
//! for signal strength, serving cell info, network info, and operator name
//! at the configured interval, storing results in [`LteState`].
//!
//! Static modem identity (IMEI, model, firmware, ICCID) is read once at startup.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, watch, Mutex};
use tracing::{debug, info, warn};

use crate::config::LteConfig;
use crate::lte_watchdog::parse_cereg;
use crate::modem::Modem;

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

/// Band config that last sustained tunnel connectivity — persisted to `{data_dir}/safe_bands.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafeBandConfig {
    pub bands: Vec<u16>,
    pub priority_band: Option<u16>,
    /// Epoch seconds when tunnel was confirmed stable on this config.
    pub confirmed_at: u64,
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
    pub fn promote_safe_bands(&mut self, data_dir: &str, bands: &[u16], priority: Option<u16>) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        self.safe_bands = Some(SafeBandConfig {
            bands: bands.to_vec(),
            priority_band: priority,
            confirmed_at: now,
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
        if b >= 1 && b <= 128 {
            mask |= 1u128 << (b - 1);
        }
    }
    format!("{mask:X}")
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
        let mut ls = state.lock().await;
        ls.band_action_until = Some(Instant::now() + timeout + Duration::from_secs(5));
    }

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

    // 2. Write new bands
    let lte_hex = bands_to_hex(lte_bands);
    let cmd = format!("AT+QCFG=\"band\",0x260,{lte_hex},0");
    modem
        .command(&cmd)
        .await
        .map_err(|e| format!("failed to set bands: {e}"))?;

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
                            let mut ls = state.lock().await;
                            ls.band_action_until = None;
                        }
                        // Success — read back the config we set
                        let new_bands = lte_bands.to_vec();
                        return Ok(BandConfig {
                            enabled_bands: new_bands,
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

    // 5. Timeout — rollback
    warn!("Band change timed out, rolling back to previous config");
    let rollback_hex = bands_to_hex(&old_bands);
    let rollback_cmd = format!("AT+QCFG=\"band\",0x260,{rollback_hex},0");
    let _ = modem.command(&rollback_cmd).await;
    if let Some(pri) = old_priority {
        let _ = modem.command(&format!("AT+QCFG=\"bandpri\",{pri}")).await;
    }

    // Clear band action suppression
    if let Some(state) = lte_state {
        let mut ls = state.lock().await;
        ls.band_action_until = None;
    }

    Err("modem did not register on new bands within timeout, rolled back".into())
}

/// Spawn a background band scan task. Locks to each band, measures signal,
/// optionally runs speed test, then restores original config.
pub fn spawn_band_scan(
    modem: Modem,
    lte_state: Arc<Mutex<LteState>>,
    bands_to_scan: Vec<u16>,
    include_speed_test: bool,
    speed_test_url: Option<String>,
    data_dir: String,
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
            // Update current band
            {
                let mut state = lte_state.lock().await;
                if let Some(ref mut scan) = state.scan_status {
                    scan.current_band = Some(band);
                }
            }

            let single_hex = bands_to_hex(&[band]);
            let cmd = format!("AT+QCFG=\"band\",0x260,{single_hex},0");
            if let Err(e) = modem.command(&cmd).await {
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
                        result.download_bps = run_speed_test(url).await;
                    }
                }
            }

            results.push(result);

            // Update scan status
            {
                let mut state = lte_state.lock().await;
                if let Some(ref mut scan) = state.scan_status {
                    scan.bands_scanned.push(band);
                    scan.results = results.clone();
                }
            }
        }

        // Always restore original config and force re-registration.
        // The scan leaves the modem deregistered (AT+COPS=2) after the last band,
        // so we must explicitly re-register or the modem stays offline.
        let restore_hex = bands_to_hex(&original_bands);
        let restore_cmd = format!("AT+QCFG=\"band\",0x260,{restore_hex},0");
        let _ = modem.command(&restore_cmd).await;
        if let Some(pri) = original_priority {
            let _ = modem.command(&format!("AT+QCFG=\"bandpri\",{pri}")).await;
        }
        let _ = modem.command("AT+COPS=0").await; // re-register on restored bands

        // Mark completed
        let completed_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        {
            let mut state = lte_state.lock().await;
            if let Some(ref mut scan) = state.scan_status {
                scan.state = "completed".into();
                scan.completed_at = Some(completed_at);
                scan.current_band = None;
                scan.results = results;
            }
            state.save_lte_data(&data_dir);
        }

        info!("Band scan completed");
    })
}

/// Run a download speed test using curl. Returns bytes per second or None on failure.
/// Uses `--connect-timeout` to fail fast when there's no data connectivity (common after
/// modem re-registration when QMI data session hasn't come up yet).
async fn run_speed_test(url: &str) -> Option<u64> {
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
            url,
        ])
        .output()
        .await
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Parse speed even on non-zero exit (e.g. timeout after partial download)
    let speed = stdout.trim().parse::<f64>().ok().map(|v| v as u64);

    // Return None only if we got zero or no measurement at all
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
    for bit in 0..128 {
        if val & (1u128 << bit) != 0 {
            bands.push((bit + 1) as u16);
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

/// Read static modem identity (IMEI, model, firmware, ICCID).
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

    ModemInfo {
        model,
        firmware,
        imei,
        iccid,
    }
}

/// Spawn the background LTE signal poller. Returns a `JoinHandle` for abort on shutdown.
#[allow(clippy::needless_pass_by_value)] // config is moved into spawned task
pub fn spawn_lte_poller(
    config: LteConfig,
    modem: Modem,
    lte_state: Arc<Mutex<LteState>>,
    session_events: broadcast::Sender<serde_json::Value>,
    mut modem_rx: watch::Receiver<Modem>,
    data_dir: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let interval = tokio::time::Duration::from_secs(config.poll_interval_secs);
        let mut modem = modem;
        let mut polls_since_save: u32 = 0;

        // Read static modem info once at startup
        let modem_info = read_modem_info(&modem).await;
        info!(
            "LTE modem: {} {} IMEI={}",
            modem_info.model.as_deref().unwrap_or("?"),
            modem_info.firmware.as_deref().unwrap_or("?"),
            modem_info.imei.as_deref().unwrap_or("?"),
        );
        lte_state.lock().await.modem = Some(modem_info);

        let mut ticker = tokio::time::interval(interval);
        // First tick is immediate — get an initial signal reading
        ticker.tick().await;

        loop {
            // Pick up refreshed modem handle if watchdog re-opened it
            if modem_rx.has_changed().unwrap_or(false) {
                modem = modem_rx.borrow_and_update().clone();
                info!("LTE poller: modem handle refreshed");
            }

            // 1. AT+CSQ for RSSI
            let rssi_result = match modem.command("AT+CSQ").await {
                Ok(resp) => parse_csq(&resp),
                Err(e) => Err(e),
            };

            let rssi_dbm = match rssi_result {
                Ok(v) => v,
                Err(e) => {
                    debug!("LTE: CSQ failed: {e}");
                    let mut state = lte_state.lock().await;
                    state.errors_total += 1;
                    state.last_error = Some(e);
                    ticker.tick().await;
                    continue;
                }
            };

            // 2. AT+QENG for RSRP/RSRQ/SINR/cell_id + serving cell details
            let qeng = match modem.command("AT+QENG=\"servingcell\"").await {
                Ok(resp) => parse_qeng(&resp),
                Err(e) => {
                    debug!("LTE: QENG failed: {e}");
                    QengData::empty()
                }
            };

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
                let mut state = lte_state.lock().await;
                update_band_history(&mut state.band_history, &signal);
                state.signal = Some(signal);
                // Save band history every 10 polls (~10 min at 60s interval)
                polls_since_save += 1;
                if polls_since_save >= 10 {
                    polls_since_save = 0;
                    state.save_lte_data(&data_dir);
                }
            }

            ticker.tick().await;
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
}
