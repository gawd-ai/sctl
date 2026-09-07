//! Infrastructure monitoring — discover and health-check LAN devices.
//!
//! This module is **opt-in**: it activates only when a monitoring config is
//! pushed via `POST /api/infra/config`. Until then, `GET /api/infra/results`
//! returns `{"status":"unconfigured"}`.
//!
//! ## Architecture
//!
//! Config is pushed from an external controller and persisted under the server
//! data directory so monitoring survives sctl restarts.
//! A tokio interval task runs checks (ping, HTTP, TCP, SNMP) against each
//! target and maintains a per-target state machine (UNKNOWN → UP / DEGRADED
//! → DOWN). Recovery actions execute locally on the BPI when a target
//! transitions to DOWN.
//!
//! Results are served via `GET /api/infra/results` for external health polling.

pub mod checks;
pub mod discovery;
pub mod monitor;
pub mod profiles;
pub mod routes;

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
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
    /// Log into a device's JSON management API over sctl's own TLS stack and
    /// reduce what it says to a structured snapshot. The profile owns the
    /// login, the endpoints and the reduction; the credential is looked up by
    /// id in the secrets file, never carried in the config.
    HttpApi {
        /// Scheme, host and optional port, e.g. `https://192.168.50.1`.
        base_url: String,
        profile: ApiProfile,
        /// Hex SHA-256 of the device's certificate DER. Required for https on
        /// a network shared with untrusted hosts; a check with no pin reports
        /// the presented fingerprint instead of trusting it.
        pin_sha256: Option<String>,
        /// Key into the credentials store (`POST /api/infra/credentials`).
        credential_id: Option<String>,
        /// Per-request timeout; the check makes several requests.
        timeout_ms: Option<u64>,
    },
}

/// Which vendor's API a `http_api` check speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiProfile {
    /// Peplink / Pepwave routers (MAX BR1 family): `POST /api/login`, then
    /// `status.wan.connection`, `status.client`, `info.location`,
    /// `status.lan`, `status.traffic`, `info.system`.
    Peplink,
}

/// A username and password a `http_api` check logs in with.
///
/// Lives in `<data_dir>/infra-secrets.json` (mode 0600), never in the
/// monitoring config and never in any response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credential {
    pub username: String,
    pub password: String,
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
    /// Target name from config, suitable for event emission.
    #[serde(default)]
    pub name: String,
    /// HTTP status of the last check, when the check speaks HTTP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    /// The structured snapshot a profile reduced the device's answers to
    /// (`http_api` checks only). Absent for every other kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// One past reading of a target that produces structured data, kept so a
/// collector that was cut off (a WAN outage on a vehicle) can backfill the
/// window it missed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataSample {
    pub ts: String,
    pub status: TargetStatus,
    pub latency_ms: Option<u64>,
    pub data: Value,
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

/// Real-time progress of a running discovery scan.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DiscoveryProgress {
    /// Whether a scan is currently running.
    pub active: bool,
    /// Current phase: "arp", "ping", "ports", "hostname", "complete", "error".
    pub phase: String,
    /// 1-based phase number (1–4 during scan, 0 when idle).
    pub phase_number: u8,
    /// Total phases in the scan.
    pub total_phases: u8,
    /// Number of hosts discovered so far.
    pub hosts_found: usize,
    /// Partial results accumulated so far.
    pub devices: Vec<discovery::DiscoveredDevice>,
    /// When the scan started (ISO 8601).
    pub started_at: Option<String>,
    /// Wall-clock time elapsed since scan start.
    pub elapsed_ms: u64,
}

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
    /// Real-time discovery scan progress (polled by UI).
    pub discovery_progress: DiscoveryProgress,
    /// Credentials for `http_api` targets, keyed by credential id.
    pub credentials: HashMap<String, Credential>,
    /// Path of the credentials file (mode 0600).
    pub secrets_path: PathBuf,
    /// Login sessions per target id (a cookie), so a check does not log in
    /// on every tick and the password crosses the LAN as rarely as possible.
    pub sessions: HashMap<String, String>,
    /// Recent structured readings per target id (ring, newest last).
    pub data_history: HashMap<String, VecDeque<DataSample>>,
}

const MAX_RECOVERY_LOG: usize = 50;
/// How many structured readings to keep per target. At the default 60 s
/// interval this is half an hour, longer than the relay windows a moving
/// vehicle loses.
pub const MAX_DATA_HISTORY: usize = 30;

impl InfraState {
    /// `state_dir` is where the config and the credentials persist; on a
    /// RAM-booted unit it is the one directory that survives a reboot.
    pub fn new(state_dir: &str) -> Self {
        Self {
            config: None,
            results: InfraResults {
                ts: now_iso(),
                config_version: 0,
                targets: HashMap::new(),
                recovery_log: Vec::new(),
            },
            recovery_tracker: HashMap::new(),
            config_path: Path::new(state_dir).join("infra-monitor.json"),
            monitor_handle: None,
            discovery_progress: DiscoveryProgress::default(),
            credentials: HashMap::new(),
            secrets_path: Path::new(state_dir).join("infra-secrets.json"),
            sessions: HashMap::new(),
            data_history: HashMap::new(),
        }
    }

    /// Load the credentials file (called at startup). Missing is normal.
    pub fn load_credentials(&mut self) {
        match std::fs::read_to_string(&self.secrets_path) {
            Ok(data) => match serde_json::from_str::<HashMap<String, Credential>>(&data) {
                Ok(c) => self.credentials = c,
                Err(e) => warn!("Failed to parse infra credentials: {e}"),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!("Failed to read infra credentials: {e}"),
        }
    }

    /// Persist the credentials file, owner-readable only, atomically.
    pub fn save_credentials(&self) -> bool {
        let tmp = self.secrets_path.with_extension("json.tmp");
        let Ok(data) = serde_json::to_string(&self.credentials) else {
            warn!("Failed to serialize infra credentials");
            return false;
        };
        if let Some(parent) = self.secrets_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = write_private(&tmp, data.as_bytes()) {
            warn!("Failed to write infra credentials tmp: {e}");
            return false;
        }
        if let Err(e) = std::fs::rename(&tmp, &self.secrets_path) {
            warn!("Failed to rename infra credentials: {e}");
            return false;
        }
        true
    }

    /// Remember a structured reading, evicting the oldest past the cap.
    pub fn push_data_sample(&mut self, target_id: &str, sample: DataSample) {
        let ring = self.data_history.entry(target_id.to_string()).or_default();
        if ring.len() >= MAX_DATA_HISTORY {
            ring.pop_front();
        }
        ring.push_back(sample);
    }

    /// Load config from disk (called at startup).
    ///
    /// The results are seeded from it at once: the config version and one
    /// `unknown` entry per target. Until the first check lands the device
    /// would otherwise report `config_version: 0` and no targets, which is
    /// exactly what a device that holds NOTHING reports, and a collector
    /// cannot tell the two apart. Now `0` means "holds nothing".
    pub fn load_config(&mut self) {
        match std::fs::read_to_string(&self.config_path) {
            Ok(data) => match serde_json::from_str::<InfraConfig>(&data) {
                Ok(cfg) => {
                    self.seed_results(&cfg);
                    self.config = Some(cfg);
                }
                Err(e) => warn!("Failed to parse infra config: {e}"),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!("Failed to read infra config: {e}"),
        }
    }

    /// Reflect a config in the results before any check has run: its
    /// version, and an `unknown` placeholder per target so the target set is
    /// visible. Entries that already carry a result are left alone.
    pub fn seed_results(&mut self, cfg: &InfraConfig) {
        let now = now_iso();
        for target in &cfg.targets {
            self.results
                .targets
                .entry(target.id.clone())
                .or_insert_with(|| TargetState {
                    status: TargetStatus::Unknown,
                    latency_ms: None,
                    since: now.clone(),
                    consecutive_ok: 0,
                    consecutive_fail: 0,
                    // Empty: no check has produced a timestamp yet. A
                    // collector that records observations keys on this
                    // and must skip an empty one.
                    last_check: String::new(),
                    detail: "not checked yet".to_string(),
                    name: target.name.clone(),
                    http_status: None,
                    data: None,
                });
        }
        self.results.config_version = cfg.version;
        self.results.ts = now;
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

/// Write a file readable by its owner only. The mode is set at creation, so
/// there is no window in which the content is world-readable.
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?
    };
    #[cfg(not(unix))]
    let mut file = std::fs::File::create(path)?;
    file.write_all(data)?;
    file.sync_all()
}

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

#[cfg(test)]
mod tests {
    use super::*;

    /// Consumer side of the producer contract with the fleet app. The fleet
    /// test `infra-config.contract.test.ts` asserts it emits exactly these
    /// bytes (`fixtures/http-api-target.push.json`) for an access-point
    /// target; this test asserts the device accepts them. Change both or
    /// neither.
    // The Bus 01 router's certificate fingerprint: a public value, not a secret.
    const FIXTURE_PIN: &str = "e882b1b69849bcf9a076e027277975d62c704fed88bd1c038a1b71e0ff398773"; // secret-scan: allow
    /// The fixture with the pin as a placeholder, so the hex lives on one
    /// marked line above and the JSON stays byte-comparable to the fleet file.
    const FLEET_HTTP_API_TARGET: &str = r#"{
  "id": "6f0b2c4e-9d3a-4c1f-8e2b-1a2b3c4d5e6f",
  "name": "Peplink MAX BR1",
  "check": {
    "method": "http_api",
    "base_url": "https://192.168.50.1",
    "profile": "peplink",
    "timeout_ms": 15000,
    "pin_sha256": "__PIN__",
    "credential_id": "6f0b2c4e-9d3a-4c1f-8e2b-1a2b3c4d5e6f"
  },
  "degraded_threshold_ms": 2000,
  "down_after_consecutive": 3,
  "up_after_consecutive": 2,
  "interval_secs": 60
}"#;

    #[test]
    fn the_fleet_emitted_http_api_target_parses() {
        let json = FLEET_HTTP_API_TARGET.replace("__PIN__", FIXTURE_PIN);
        let t: InfraTarget = serde_json::from_str(&json).unwrap();
        assert_eq!(t.name, "Peplink MAX BR1");
        assert_eq!(t.interval_secs, Some(60));
        match t.check {
            CheckSpec::HttpApi {
                base_url,
                profile,
                pin_sha256,
                credential_id,
                timeout_ms,
            } => {
                assert_eq!(base_url, "https://192.168.50.1");
                assert_eq!(profile, ApiProfile::Peplink);
                assert_eq!(pin_sha256.as_deref(), Some(FIXTURE_PIN));
                assert_eq!(credential_id.as_deref(), Some(t.id.as_str()));
                assert_eq!(timeout_ms, Some(15000));
            }
            other => panic!("expected http_api, got {other:?}"),
        }
    }

    #[test]
    fn a_target_without_the_new_fields_still_parses() {
        // Configs written before this kind existed carry none of it.
        let t: InfraTarget = serde_json::from_str(
            r#"{"id":"a","name":"gw","check":{"method":"ping","host":"10.0.0.1"}}"#,
        )
        .unwrap();
        assert!(matches!(t.check, CheckSpec::Ping { .. }));
        assert_eq!(t.degraded_threshold_ms, 200);
    }

    #[test]
    fn results_omit_data_and_http_status_for_plain_kinds() {
        let s = serde_json::to_string(&TargetState {
            status: TargetStatus::Up,
            latency_ms: Some(3),
            since: now_iso(),
            consecutive_ok: 1,
            consecutive_fail: 0,
            last_check: now_iso(),
            detail: "PING OK 3ms".into(),
            name: "gw".into(),
            http_status: None,
            data: None,
        })
        .unwrap();
        assert!(!s.contains("\"data\""), "{s}");
        assert!(!s.contains("http_status"), "{s}");
    }

    #[test]
    fn credentials_are_written_owner_only_and_never_into_the_config_file() {
        let dir = std::env::temp_dir().join(format!("sctl-infra-secrets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut st = InfraState::new(dir.to_str().unwrap());
        st.credentials.insert(
            "t1".into(),
            Credential {
                username: "admin".into(),
                password: "hunter2".into(),
            },
        );
        assert!(st.save_credentials());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&st.secrets_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        st.config = Some(InfraConfig {
            version: 1,
            check_interval_secs: 60,
            targets: vec![],
        });
        assert!(st.save_config());
        let cfg = std::fs::read_to_string(&st.config_path).unwrap();
        assert!(!cfg.contains("hunter2"));
        let mut again = InfraState::new(dir.to_str().unwrap());
        again.load_credentials();
        assert_eq!(
            again.credentials.get("t1").map(|c| c.password.as_str()),
            Some("hunter2")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_loaded_config_is_visible_in_the_results_before_any_check() {
        let dir = std::env::temp_dir().join(format!(
            "sctl-infra-seed-{}-{}",
            std::process::id(),
            now_epoch()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let target = FLEET_HTTP_API_TARGET.replace("__PIN__", FIXTURE_PIN);
        std::fs::write(
            dir.join("infra-monitor.json"),
            format!(r#"{{"version": 7, "targets": [{target}]}}"#),
        )
        .unwrap();

        let mut state = InfraState::new(dir.to_str().unwrap());
        assert_eq!(state.results.config_version, 0, "nothing loaded yet");
        state.load_config();

        // A device that holds a config never reports the shape of a device
        // that holds nothing (version 0, no targets).
        assert_eq!(state.results.config_version, 7);
        let seeded = &state.results.targets["6f0b2c4e-9d3a-4c1f-8e2b-1a2b3c4d5e6f"];
        assert_eq!(seeded.status, TargetStatus::Unknown);
        assert_eq!(seeded.name, "Peplink MAX BR1");
        assert!(seeded.last_check.is_empty(), "no check has run");
        assert!(seeded.data.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
