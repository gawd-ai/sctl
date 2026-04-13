//! Infrastructure monitoring — discover and health-check LAN devices.
//!
//! This module is **opt-in**: it activates only when a monitoring config is
//! pushed via `POST /api/infra/config`. Until then, `GET /api/infra/results`
//! returns `{"status":"unconfigured"}`.
//!
//! ## Architecture
//!
//! Config is pushed from the fleet management server and persisted to
//! `/etc/netage/infra-monitor.json` so monitoring survives sctl restarts.
//! A tokio interval task runs checks (ping, HTTP, TCP, SNMP) against each
//! target and maintains a per-target state machine (UNKNOWN → UP / DEGRADED
//! → DOWN). Recovery actions execute locally on the BPI when a target
//! transitions to DOWN.
//!
//! Results are served via `GET /api/infra/results` and collected by
//! netage-server's health poller (one read per BPI per 30s cycle).

pub mod checks;
pub mod discovery;
pub mod monitor;
pub mod routes;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::warn;

// ─── Config types (pushed from fleet server, persisted to disk) ──────

/// Top-level monitoring configuration pushed via `POST /api/infra/config`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InfraConfig {
    /// Monotonically increasing version number. The fleet server bumps this
    /// on every config change so the health poller can detect stale configs
    /// on the BPI and re-push.
    pub version: u32,
    /// Global check interval fallback (individual targets can override).
    #[serde(default = "default_check_interval")]
    pub check_interval_secs: u64,
    /// Targets to monitor.
    pub targets: Vec<InfraTarget>,
}

fn default_check_interval() -> u64 {
    60
}

/// A single infrastructure device to monitor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InfraTarget {
    /// Stable identifier (UUID from the fleet DB).
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Check type.
    pub check: CheckSpec,
    /// Milliseconds above which the target is considered degraded.
    #[serde(default = "default_degraded_ms")]
    pub degraded_threshold_ms: u64,
    /// Consecutive failures before transitioning to DOWN.
    #[serde(default = "default_down_after")]
    pub down_after_consecutive: u32,
    /// Consecutive successes required to transition back to UP (hysteresis).
    #[serde(default = "default_up_after")]
    pub up_after_consecutive: u32,
    /// Optional per-target check interval override.
    pub interval_secs: Option<u64>,
    /// Optional recovery action.
    pub recovery: Option<RecoveryConfig>,
}

fn default_degraded_ms() -> u64 {
    200
}
fn default_down_after() -> u32 {
    3
}
fn default_up_after() -> u32 {
    2
}

/// What kind of check to run.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum CheckSpec {
    Ping {
        host: String,
        timeout_ms: Option<u64>,
    },
    Http {
        url: String,
        expected_status: Option<u16>,
        timeout_ms: Option<u64>,
    },
    Https {
        url: String,
        expected_status: Option<u16>,
        timeout_ms: Option<u64>,
    },
    TcpPort {
        host: String,
        port: u16,
        timeout_ms: Option<u64>,
    },
    Snmp {
        host: String,
        community: Option<String>,
        timeout_ms: Option<u64>,
    },
    CustomScript {
        command: String,
        timeout_ms: Option<u64>,
    },
}

/// User-configured recovery action that fires when a target enters DOWN.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryConfig {
    pub enabled: bool,
    /// Shell command to execute locally on the BPI.
    pub command: String,
    /// Minimum seconds between consecutive executions.
    #[serde(default = "default_cooldown")]
    pub cooldown_secs: u64,
    /// Maximum number of executions before exhaustion (resets on recovery).
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
}

fn default_cooldown() -> u64 {
    300
}
fn default_max_retries() -> u32 {
    2
}

// ─── Result types (served via GET /api/infra/results) ────────────────

/// Status of a single monitored target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetStatus {
    Unknown,
    Up,
    Degraded,
    Down,
}

impl std::fmt::Display for TargetStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown => write!(f, "unknown"),
            Self::Up => write!(f, "up"),
            Self::Degraded => write!(f, "degraded"),
            Self::Down => write!(f, "down"),
        }
    }
}

/// Per-target live state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetState {
    pub status: TargetStatus,
    pub latency_ms: Option<u64>,
    /// ISO 8601 timestamp of when the current status began.
    pub since: String,
    pub consecutive_ok: u32,
    pub consecutive_fail: u32,
    /// ISO 8601 timestamp of the last check.
    pub last_check: String,
    /// Human-readable detail of the last check result.
    pub detail: String,
    /// Target name from config (for event emission by netage-server).
    #[serde(default)]
    pub name: String,
}

/// A single recovery action execution log entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryLogEntry {
    pub ts: String,
    pub target_id: String,
    pub command: String,
    pub exit_code: i32,
    pub stdout: String,
}

/// Full results payload returned by `GET /api/infra/results`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InfraResults {
    /// ISO 8601 timestamp of when this snapshot was generated.
    pub ts: String,
    /// Config version currently active (for reconciliation).
    pub config_version: u32,
    /// Per-target status keyed by target ID.
    pub targets: HashMap<String, TargetState>,
    /// Recent recovery action executions (ring buffer, max 50).
    pub recovery_log: Vec<RecoveryLogEntry>,
}

// ─── Shared state ────────────────────────────────────────────────────

/// Shared infra monitoring state, held behind `Arc<Mutex<>>` on `AppState`.
pub struct InfraState {
    /// Current config (None until first push).
    pub config: Option<InfraConfig>,
    /// Latest results snapshot.
    pub results: InfraResults,
    /// Per-target recovery cooldown tracking: target_id → (last_exec_epoch, exec_count).
    pub recovery_tracker: HashMap<String, (u64, u32)>,
    /// Path for persistent config storage.
    pub config_path: PathBuf,
    /// Handle to the monitoring task (so we can abort and restart on config change).
    pub monitor_handle: Option<tokio::task::JoinHandle<()>>,
}

const MAX_RECOVERY_LOG: usize = 50;

impl InfraState {
    pub fn new(data_dir: &str) -> Self {
        Self {
            config: None,
            results: InfraResults {
                ts: now_iso(),
                config_version: 0,
                targets: HashMap::new(),
                recovery_log: Vec::new(),
            },
            recovery_tracker: HashMap::new(),
            config_path: Path::new(data_dir).join("infra-monitor.json"),
            monitor_handle: None,
        }
    }

    /// Load config from disk (called at startup).
    pub fn load_config(&mut self) {
        match std::fs::read_to_string(&self.config_path) {
            Ok(data) => match serde_json::from_str::<InfraConfig>(&data) {
                Ok(cfg) => {
                    self.config = Some(cfg);
                }
                Err(e) => warn!("Failed to parse infra config: {e}"),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!("Failed to read infra config: {e}"),
        }
    }

    /// Persist config to disk (atomic write via tmp + rename).
    pub fn save_config(&self) -> bool {
        let Some(ref cfg) = self.config else {
            return false;
        };
        let tmp = self.config_path.with_extension("json.tmp");
        let Ok(data) = serde_json::to_string_pretty(cfg) else {
            warn!("Failed to serialize infra config");
            return false;
        };
        if let Some(parent) = self.config_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&tmp, &data) {
            warn!("Failed to write infra config tmp: {e}");
            return false;
        }
        if let Err(e) = std::fs::rename(&tmp, &self.config_path) {
            warn!("Failed to rename infra config: {e}");
            return false;
        }
        true
    }

    /// Push a recovery log entry, evicting oldest if over capacity.
    pub fn push_recovery_log(&mut self, entry: RecoveryLogEntry) {
        if self.results.recovery_log.len() >= MAX_RECOVERY_LOG {
            self.results.recovery_log.remove(0);
        }
        self.results.recovery_log.push(entry);
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────

/// Current time as ISO 8601 string (no chrono dependency).
#[allow(clippy::many_single_char_names)]
pub fn now_iso() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let mins = (time_of_day % 3600) / 60;
    let sec = time_of_day % 60;
    let (year, month, day) = days_to_date(days);
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{mins:02}:{sec:02}Z")
}

/// Convert days since Unix epoch to (year, month, day).
fn days_to_date(days: u64) -> (u64, u64, u64) {
    // Algorithm from Howard Hinnant's `civil_from_days`
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

/// Current time as Unix epoch seconds.
pub fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
