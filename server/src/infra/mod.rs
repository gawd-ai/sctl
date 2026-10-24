//! Infrastructure monitoring — discover and health-check LAN devices.
//!
//! This module is **opt-in**: it activates only when a monitoring config is
//! pushed via `POST /api/infra/config`. Until then, `GET /api/infra/results`
//! returns empty results: `config_version: 0` and no targets.
//!
//! ## Architecture
//!
//! Config is pushed from an external controller and persisted under the server
//! data directory so monitoring survives sctl restarts. A push equal to the
//! config held, every field and the version included, changes nothing.
//! A tokio interval task runs checks (ping, HTTP, TCP, SNMP) against each
//! target and maintains a per-target state machine (UNKNOWN → UP / DEGRADED
//! → DOWN). A good answer slower than the target's threshold never makes it
//! Up, so a slow, steady target stays Degraded (ADR-010 decision 8). The
//! monitor runs under a supervisor, at boot and for a push, and never in an
//! agent that started in safe mode.
//!
//! A recovery action runs on the unit when a failed check of the monitor
//! leaves its target Down: the first Down and every failed check after it,
//! within `cooldown_secs` and `max_retries`, never on a good answer (ADR-010
//! decision 9). It runs in a task of its own, outside the Infra lock and
//! after the Down is signalled, one run per target at a time, and nothing
//! but its own 300 s limit or the agent's stop ends it. Each target's count
//! and each run not yet ended are saved in `<state_dir>/infra-recovery.json`
//! before the command starts, so the count holds across the restarts a
//! recovery causes, and the next start logs a run that did not end. The run
//! writes that file, synced, on the blocking pool and outside the Infra
//! lock ([`RecoveryFile`]).
//!
//! Check now (`POST /api/infra/check/{target_id}`) is a reading: its result
//! is applied as a monitor check's is, and it never starts a recovery.
//!
//! Results are served via `GET /api/infra/results` for external health polling,
//! and pushed over the tunnel as `infra.state` (see `tunnel::infra_state`)
//! whenever a target's status changes, a config is applied or removed, or a
//! recovery run is logged: [`InfraState::changed`] is the signal, and it
//! moves `seq`; [`InfraState::watch_changes`] is the way to wait on it.
//!
//! A unit that pushes only changes is quiet while its targets hold steady,
//! so it proves on every poll that its monitor is checking (ADR-010): the
//! [`Digest`] is the `infra` group of `GET /api/info` (`rounds`, `seq`,
//! `hash`, `off`), kept in a cell of its own that `GET /api/info` reads
//! without the Infra lock.

pub mod checks;
pub mod discovery;
pub mod monitor;
pub mod profiles;
pub mod routes;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Digest as _;
use tokio::sync::watch;
use tokio::time::Instant;
use tracing::warn;

// ─── Config types (pushed from fleet server, persisted to disk) ──────

/// Top-level monitoring configuration pushed via `POST /api/infra/config`.
///
/// Compared whole: a push equal to the config held, every field and the
/// version included, changes nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InfraConfig {
    /// The caller's name for this config, not a counter: the fleet sends a
    /// fingerprint of it. 0 means the unit holds no config, so a push of
    /// version 0 is refused. Reported back in the results, so a collector
    /// sees which config the unit holds.
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InfraTarget {
    /// Stable identifier (UUID from the fleet DB).
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Check type.
    pub check: CheckSpec,
    /// A good answer above this many milliseconds is slow: it makes Up
    /// Degraded, and it never makes a target Up.
    #[serde(default = "default_degraded_ms")]
    pub degraded_threshold_ms: u64,
    /// Failures in a row that take Degraded to Down.
    #[serde(default = "default_down_after")]
    pub down_after_consecutive: u32,
    /// Good answers within the threshold in a row that take Degraded back
    /// to Up. From Down, good answers in a row that end it: Up when all
    /// were within the threshold, Degraded otherwise.
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
/// Lives in `<state_dir>/infra-secrets.json` (mode 0600), never in the
/// monitoring config and never in any response. The file read routes
/// refuse it, its directory and any copy by its name (ADR-011).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credential {
    pub username: String,
    pub password: String,
}

/// A recovery action: a command the unit runs when a failed check of the
/// monitor leaves the target Down, the first Down and every failed check
/// after it, within `cooldown_secs` and `max_retries`. Never on a good
/// answer, and never on Check now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryConfig {
    pub enabled: bool,
    /// Shell command run on the unit, under a 300 s limit.
    pub command: String,
    /// Minimum seconds between the starts of two runs. After the agent
    /// starts, a target with a saved count waits a full cooldown.
    #[serde(default = "default_cooldown")]
    pub cooldown_secs: u64,
    /// Runs before the recovery gives up. The count is saved, so it holds
    /// across restarts, and it clears when a good answer moves the target
    /// out of Down or `unknown`.
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// Credentials the command names (ADR-006, fleet ADR-0017): entry `n`
    /// (from 1) is in the command's environment as `SCTL_CRED_USER_n` and
    /// `SCTL_CRED_PASS_n`, read from this unit's credential store. The
    /// command carries the variable names, never the values.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credential_ids: Vec<String>,
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
    /// Good answers in a row, slow ones included.
    pub consecutive_ok: u32,
    pub consecutive_fail: u32,
    /// Good answers within the threshold in a row: what takes Degraded back
    /// to Up. It stays inside the agent: no report carries it.
    #[serde(skip)]
    pub consecutive_within: u32,
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
    /// When the run was logged: at its end, or at the start that found it
    /// not ended.
    pub ts: String,
    pub target_id: String,
    pub command: String,
    /// -1 when the run gave no exit code: the agent could not start it, it
    /// passed its 300 s limit, or its outcome is unknown; `stdout` says
    /// which.
    pub exit_code: i32,
    /// The command's output, each password of at least 4 bytes masked, as
    /// written and in its HTTP Basic form (the base64 of
    /// `username:password`), then cut to 512 bytes on a character boundary
    /// (`...` marks a cut).
    pub stdout: String,
    /// The command's error output, masked and cut as `stdout` is: a failed
    /// command says why there.
    #[serde(default)]
    pub stderr: String,
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

// ─── The digest: proof that the monitor is checking (ADR-010) ────────

/// Why no monitor is expected: the `off` of the `infra` group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Off {
    /// The agent started in safe mode, which keeps the monitor off,
    /// whatever the flag file says now.
    SafeMode,
    /// The unit holds no config.
    NoConfig,
}

/// The `infra` group of `GET /api/info` (ADR-010 decision 2): the proof
/// that the monitor is checking, and the name of the picture the unit
/// holds. A steady unit's group is about 60 bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Group {
    /// Monitor rounds since the agent started. It never goes back while
    /// the agent runs.
    pub rounds: u64,
    /// 1 when the agent starts, +1 at every change it pushes.
    pub seq: u64,
    /// [`picture_hash`] of [`picture_text`]: changes only when `seq` moves.
    pub hash: String,
    /// Present only when no monitor is expected; then `rounds` stands.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub off: Option<Off>,
}

/// What every report names beside its targets (ADR-010 decision 3), as it
/// stood when the report was built: `infra.state` and
/// `GET /api/infra/results` carry it, so a copy built from either can be
/// compared with the `infra` group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamp {
    /// When the agent started, unix ms: the `boot` that `net.state` carries.
    /// A new value means `seq` began again.
    pub boot: u64,
    /// 1 when the agent starts, +1 at every change it pushes.
    pub seq: u64,
    /// The picture's hash, as in the `infra` group.
    pub hash: String,
    /// Recovery runs logged since `boot`.
    pub recoveries: u64,
}

/// The digest's cell. Every writer of the results updates it, under the
/// Infra lock; `GET /api/info` reads it without that lock, so a lock held
/// for good shows as `rounds` standing still, never as a slow answer. It
/// also holds the recovery runs in progress, which a managed upgrade waits
/// for without that lock.
#[derive(Debug)]
pub struct Digest {
    /// When the agent started, unix ms.
    boot: u64,
    /// Whether the agent started in safe mode.
    safe_mode: bool,
    cell: StdMutex<Cell>,
    runs: Runs,
}

#[derive(Debug)]
struct Cell {
    rounds: u64,
    seq: u64,
    hash: String,
    recoveries: u64,
    holds_config: bool,
}

impl Digest {
    /// The digest of an agent that started at `boot` (unix ms), in safe
    /// mode or not, before it holds a config: `seq` 1, no round, the hash
    /// of a unit that holds nothing.
    pub fn new(boot: u64, safe_mode: bool) -> Self {
        Self {
            boot,
            safe_mode,
            cell: StdMutex::new(Cell {
                rounds: 0,
                seq: 1,
                hash: picture_hash(&picture_text(boot, 0, [])),
                recoveries: 0,
                holds_config: false,
            }),
            runs: Runs::default(),
        }
    }

    /// When the agent started, unix ms.
    pub fn boot(&self) -> u64 {
        self.boot
    }

    /// Whether the agent started in safe mode, which keeps the monitor off
    /// whatever the flag file says now.
    pub fn safe_mode(&self) -> bool {
        self.safe_mode
    }

    /// The recovery runs in progress.
    pub fn runs(&self) -> Runs {
        self.runs.clone()
    }

    /// The `infra` group as it stands now.
    pub fn group(&self) -> Group {
        let cell = self.cell();
        let off = if self.safe_mode {
            Some(Off::SafeMode)
        } else if cell.holds_config {
            None
        } else {
            Some(Off::NoConfig)
        };
        Group {
            rounds: cell.rounds,
            seq: cell.seq,
            hash: cell.hash.clone(),
            off,
        }
    }

    /// What a report names, as it stands now.
    pub fn stamp(&self) -> Stamp {
        let cell = self.cell();
        Stamp {
            boot: self.boot,
            seq: cell.seq,
            hash: cell.hash.clone(),
            recoveries: cell.recoveries,
        }
    }

    /// Count one monitor round: the end of a tick that met the round rule.
    pub fn count_round(&self) {
        let mut cell = self.cell();
        cell.rounds = cell.rounds.saturating_add(1);
    }

    fn cell(&self) -> MutexGuard<'_, Cell> {
        self.cell.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The text `hash` digests (ADR-010 decision 2): the agent's `boot` in
/// decimal, a newline, the config version in decimal (0 for none), a
/// newline, then per target in byte order of its id: the id, a space, its
/// status (`unknown`, `up`, `degraded` or `down`), a newline. The fleet
/// hashes its copy by the same text, so a release that hashes other text
/// names the field anew.
pub fn picture_text<'a>(
    boot: u64,
    config_version: u32,
    targets: impl IntoIterator<Item = (&'a str, TargetStatus)>,
) -> String {
    let mut targets: Vec<(&str, TargetStatus)> = targets.into_iter().collect();
    // A `str` orders by its bytes.
    targets.sort_unstable_by(|a, b| a.0.cmp(b.0));
    let mut text = format!("{boot}\n{config_version}\n");
    for (id, status) in targets {
        let _ = writeln!(text, "{id} {status}");
    }
    text
}

/// The first 16 hex characters, lowercase, of the SHA-256 of `text`.
pub fn picture_hash(text: &str) -> String {
    sha2::Sha256::digest(text.as_bytes()).iter().take(8).fold(
        String::with_capacity(16),
        |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        },
    )
}

// ─── Recovery runs in progress ───────────────────────────────────────

/// The recovery runs in progress, by target id (ADR-010 decision 9). A
/// target has at most one at a time, and a managed upgrade waits for every
/// one to end before it hands off to its helper. Read without the Infra
/// lock.
#[derive(Debug, Clone)]
pub struct Runs(Arc<watch::Sender<BTreeSet<String>>>);

/// A run in progress: it ends when this drops, however its task ends.
#[derive(Debug)]
pub struct RunGuard {
    runs: Runs,
    target_id: String,
}

impl Default for Runs {
    fn default() -> Self {
        Self(Arc::new(watch::channel(BTreeSet::new()).0))
    }
}

impl Runs {
    /// Mark a run of `target_id` in progress; `None` while one is.
    pub fn start(&self, target_id: &str) -> Option<RunGuard> {
        let mut started = false;
        self.0.send_if_modified(|runs| {
            started = runs.insert(target_id.to_string());
            started
        });
        started.then(|| RunGuard {
            runs: self.clone(),
            target_id: target_id.to_string(),
        })
    }

    /// Whether a run of `target_id` is in progress.
    pub fn contains(&self, target_id: &str) -> bool {
        self.0.borrow().contains(target_id)
    }

    /// How many runs are in progress.
    pub fn in_progress(&self) -> usize {
        self.0.borrow().len()
    }

    /// Wait until no run is in progress, at most `limit`. True when none
    /// is.
    pub async fn wait_idle(&self, limit: Duration) -> bool {
        let mut runs = self.0.subscribe();
        let idle = tokio::time::timeout(limit, runs.wait_for(BTreeSet::is_empty)).await;
        matches!(idle, Ok(Ok(_)))
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        self.runs
            .0
            .send_if_modified(|runs| runs.remove(&self.target_id));
    }
}

// ─── Checks in flight ────────────────────────────────────────────────

/// The targets with a check in flight. The monitor never starts a target
/// whose check is in flight, and its round rule reads when each running
/// check took its permit. It lives in the Infra state, so every monitor the
/// unit runs, a rebuilt one included, and Check now see the same set. A
/// target whose failed check started a recovery stays in flight until the
/// run ends.
#[derive(Debug, Clone, Default)]
pub struct InFlight(Arc<StdMutex<Flights>>);

#[derive(Debug, Default)]
struct Flights {
    next_token: u64,
    by_target: HashMap<String, FlightEntry>,
}

#[derive(Debug)]
struct FlightEntry {
    /// Which [`Flight`] holds the place: only that one marks its permit,
    /// publishes its result or frees it.
    token: u64,
    /// When the check took the permit it holds; `None` while it waits for
    /// one, and once it gave it back.
    permit_at: Option<Instant>,
    /// The check's result once it has one, for a Check now that waits for
    /// it.
    result: watch::Sender<Option<checks::CheckResult>>,
}

/// What a check in flight will answer: its result, once it has one. The
/// channel closes without one when the check is cancelled with its
/// monitor.
pub type Answer = watch::Receiver<Option<checks::CheckResult>>;

/// A target's place in flight, held by its check until its result is
/// applied, then by the recovery run that result started, if any.
/// Dropping it, when the check or the run ends or is aborted, frees the
/// target.
#[derive(Debug)]
pub struct Flight {
    set: InFlight,
    target_id: String,
    token: u64,
}

impl InFlight {
    /// Take `target_id`'s place in flight; `None` while a check of it is.
    pub fn start(&self, target_id: &str) -> Option<Flight> {
        self.take_or_wait(target_id).ok()
    }

    /// Take `target_id`'s place in flight or, while a check of it is in
    /// flight, a receiver for that check's result: one decision, so a
    /// target never has two checks in flight.
    pub fn take_or_wait(&self, target_id: &str) -> Result<Flight, Answer> {
        let mut flights = self.flights();
        if let Some(entry) = flights.by_target.get(target_id) {
            return Err(entry.result.subscribe());
        }
        flights.next_token = flights.next_token.wrapping_add(1);
        let token = flights.next_token;
        flights.by_target.insert(
            target_id.to_string(),
            FlightEntry {
                token,
                permit_at: None,
                result: watch::channel(None).0,
            },
        );
        Ok(Flight {
            set: self.clone(),
            target_id: target_id.to_string(),
            token,
        })
    }

    /// Whether a check of `target_id` is in flight.
    pub fn contains(&self, target_id: &str) -> bool {
        self.flights().by_target.contains_key(target_id)
    }

    /// When each running check took its permit.
    pub fn permit_times(&self) -> Vec<Instant> {
        self.flights()
            .by_target
            .values()
            .filter_map(|entry| entry.permit_at)
            .collect()
    }

    fn flights(&self) -> MutexGuard<'_, Flights> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Flight {
    /// The check took its permit at `at`: it is running.
    pub fn took_permit(&self, at: Instant) {
        self.with_entry(|entry| entry.permit_at = Some(at));
    }

    /// The check gave its permit back: it is no longer running, and its
    /// target stays in flight until its result is applied.
    pub fn left_permit(&self) {
        self.with_entry(|entry| entry.permit_at = None);
    }

    /// The check's result: every Check now waiting for this check answers
    /// it, and so does one that comes while the place is still held.
    pub fn publish(&self, result: &checks::CheckResult) {
        self.with_entry(|entry| {
            entry.result.send_replace(Some(result.clone()));
        });
    }

    fn with_entry(&self, f: impl FnOnce(&mut FlightEntry)) {
        if let Some(entry) = self.set.flights().by_target.get_mut(&self.target_id) {
            if entry.token == self.token {
                f(entry);
            }
        }
    }
}

impl Drop for Flight {
    fn drop(&mut self) {
        let mut flights = self.set.flights();
        if flights
            .by_target
            .get(&self.target_id)
            .is_some_and(|entry| entry.token == self.token)
        {
            flights.by_target.remove(&self.target_id);
        }
    }
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
    /// Per-target recovery cooldown tracking: target_id → (when its last
    /// run started, on the monotonic clock, and the runs since the count
    /// last cleared). A count loaded at start takes the start as its last
    /// run, so the target waits a full cooldown.
    pub recovery_tracker: HashMap<String, (Instant, u32)>,
    /// The recovery runs started and not yet logged, by target id: saved
    /// as not ended before each command starts.
    pub runs_not_ended: HashMap<String, SavedRun>,
    /// The saved recovery counts and runs (mode 0600), and the order its
    /// writes reach it in.
    pub recovery_file: RecoveryFile,
    /// Path for persistent config storage.
    pub config_path: PathBuf,
    /// Whether the last save of the held config reached the disk; an
    /// identical push writes it again only when it did not.
    pub config_saved: bool,
    /// Which config the unit holds: it moves at every config applied or
    /// removed, so a check's result is applied only while the config it
    /// ran under is still held.
    pub config_generation: u64,
    /// The supervisor of the monitoring task, aborted when the config
    /// changes or is removed. Aborting it ends the monitor and its checks,
    /// never a recovery run.
    pub monitor_handle: Option<tokio::task::JoinHandle<()>>,
    /// How the monitor is built: the agent's own clock and checks, or a
    /// test's.
    pub(crate) monitor_kit: monitor::Kit,
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
    /// The targets with a check in flight, shared by every monitor the
    /// unit runs.
    pub in_flight: InFlight,
    /// The digest of the monitor, shared with `AppState` so that
    /// `GET /api/info` reads it without this state's lock.
    digest: Arc<Digest>,
    /// Holds `seq`, set by [`Self::changed`]: a target's status changed, a
    /// config was applied or removed, or a recovery run was logged. What
    /// the tunnel's `infra.state` forwarder waits on.
    changes: watch::Sender<u64>,
}

const MAX_RECOVERY_LOG: usize = 50;
/// How many structured readings to keep per target. At the default 60 s
/// interval this is half an hour, longer than the relay windows a moving
/// vehicle loses.
pub const MAX_DATA_HISTORY: usize = 30;

impl InfraState {
    /// `state_dir` is where the config and the credentials persist; on a
    /// RAM-booted unit it is the one directory that survives a reboot. The
    /// digest takes now as the agent's start, outside safe mode; the agent
    /// itself builds its state with [`Self::with_digest`].
    pub fn new(state_dir: &str) -> Self {
        Self::with_digest(state_dir, Arc::new(Digest::new(now_unix_ms(), false)))
    }

    /// The state of the agent whose digest is `digest`: `main` makes it
    /// with the agent's `boot` and whether it started in safe mode, and
    /// gives `AppState` the same one.
    pub fn with_digest(state_dir: &str, digest: Arc<Digest>) -> Self {
        let seq = digest.stamp().seq;
        Self {
            config: None,
            results: InfraResults {
                ts: now_iso(),
                config_version: 0,
                targets: HashMap::new(),
                recovery_log: Vec::new(),
            },
            recovery_tracker: HashMap::new(),
            runs_not_ended: HashMap::new(),
            recovery_file: RecoveryFile::new(Path::new(state_dir).join("infra-recovery.json")),
            config_path: Path::new(state_dir).join("infra-monitor.json"),
            config_saved: false,
            config_generation: 0,
            monitor_handle: None,
            monitor_kit: monitor::Kit::default(),
            discovery_progress: DiscoveryProgress::default(),
            credentials: HashMap::new(),
            secrets_path: Path::new(state_dir).join("infra-secrets.json"),
            sessions: HashMap::new(),
            data_history: HashMap::new(),
            in_flight: InFlight::default(),
            digest,
            changes: watch::channel(seq).0,
        }
    }

    /// The digest of the monitor, the cell `GET /api/info` reads.
    pub fn digest(&self) -> Arc<Digest> {
        self.digest.clone()
    }

    /// What a report built now names: read under this state's lock, it
    /// matches the results beside it.
    pub fn stamp(&self) -> Stamp {
        self.digest.stamp()
    }

    /// Whether the agent started in safe mode: a push then stores the
    /// config and starts no monitor.
    pub fn safe_mode(&self) -> bool {
        self.digest.safe_mode
    }

    /// The recovery runs in progress.
    pub fn runs(&self) -> Runs {
        self.digest.runs()
    }

    /// Whether the unit still holds the config of `generation`, and
    /// `target_id` is in it: a check's result is applied only then.
    pub fn holds(&self, generation: u64, target_id: &str) -> bool {
        self.config_generation == generation
            && self
                .config
                .as_ref()
                .is_some_and(|cfg| cfg.targets.iter().any(|t| t.id == target_id))
    }

    /// Say the results changed in a way worth pushing: a target's status
    /// (not its latency or its counters alone), a config applied or
    /// removed, or a recovery run logged. `seq` moves by one, the digest
    /// takes the hash of the picture the results now show, and every
    /// receiver from [`Self::watch_changes`] wakes.
    pub fn changed(&self) {
        let seq = self.publish(true);
        self.changes.send_modify(|n| *n = seq);
    }

    /// A receiver that wakes on every [`Self::changed`]; its value is `seq`.
    pub fn watch_changes(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    /// Bring the digest up to date with the results: the hash of their
    /// picture and whether a config is held, `seq` moved by one when
    /// `moved`. Returns `seq`.
    fn publish(&self, moved: bool) -> u64 {
        let text = picture_text(
            self.digest.boot,
            self.results.config_version,
            self.results
                .targets
                .iter()
                .map(|(id, target)| (id.as_str(), target.status)),
        );
        let hash = picture_hash(&text);
        let mut cell = self.digest.cell();
        if moved {
            cell.seq += 1;
        }
        cell.hash = hash;
        cell.holds_config = self.config.is_some();
        cell.seq
    }

    /// Hold `cfg`, as a push does. The results reflect it at once: targets
    /// that left are dropped, new ones appear as `unknown`, and the version
    /// is the pushed one, so a collector reading the results between the
    /// push and the first check sees the config it pushed. A target that
    /// left loses its recovery count. A config equal to the one held, every
    /// field and the version included, changes nothing and signals nothing.
    /// True when it changed what is held; the caller saves it.
    pub fn hold_config(&mut self, cfg: InfraConfig) -> bool {
        if self.config.as_ref() == Some(&cfg) {
            return false;
        }
        let keep: HashSet<&str> = cfg.targets.iter().map(|t| t.id.as_str()).collect();
        self.results
            .targets
            .retain(|id, _| keep.contains(id.as_str()));
        let counts = self.recovery_tracker.len();
        self.recovery_tracker
            .retain(|id, _| keep.contains(id.as_str()));
        let counts_dropped = self.recovery_tracker.len() != counts;
        self.seed_results(&cfg);
        self.config = Some(cfg);
        self.config_saved = false;
        self.config_generation += 1;
        if counts_dropped {
            self.save_recoveries();
        }
        self.changed();
        true
    }

    /// Hold no config, as a removal does: no target, version 0, no
    /// recovery count. A run in progress goes on and is logged when it
    /// ends. True when there was something to remove; removing nothing is
    /// not a change.
    pub fn drop_config(&mut self) -> bool {
        let held = self.config.take().is_some()
            || !self.results.targets.is_empty()
            || self.results.config_version != 0;
        self.results.targets.clear();
        self.results.config_version = 0;
        self.results.ts = now_iso();
        if !self.recovery_tracker.is_empty() {
            self.recovery_tracker.clear();
            self.save_recoveries();
        }
        if held {
            self.config_generation += 1;
            self.changed();
        }
        held
    }

    /// Clear `target_id`'s recovery count, as a good answer that moves it
    /// out of Down or `unknown` does, and save the counts when one was
    /// there. The save goes to the blocking pool and nothing waits for it:
    /// it comes on the check path, under the Infra lock.
    pub fn clear_recovery_count(&mut self, target_id: &str) {
        if self.recovery_tracker.remove(target_id).is_some() {
            self.recoveries_snapshot().hand_off();
        }
    }

    /// Load the saved recovery counts and runs (called at startup, after
    /// the config). A saved count stands and waits a full cooldown from
    /// now, so `max_retries` holds across the restarts a recovery causes;
    /// one for a target the held config lacks is dropped. Each run saved as
    /// not ended is logged with its outcome unknown, which moves `seq` and
    /// `recoveries`. Missing is normal.
    pub fn load_recoveries(&mut self) {
        let saved = match std::fs::read_to_string(self.recovery_file.path()) {
            Ok(data) => match serde_json::from_str::<SavedRecoveries>(&data) {
                Ok(saved) => saved,
                Err(e) => {
                    warn!("Failed to parse infra recovery counts: {e}");
                    return;
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                warn!("Failed to read infra recovery counts: {e}");
                return;
            }
        };
        let now = Instant::now();
        // A target the held config lacks has no count to keep. With no
        // config held (one that did not load), the counts wait for a push.
        let held: Option<HashSet<&str>> = self
            .config
            .as_ref()
            .map(|cfg| cfg.targets.iter().map(|t| t.id.as_str()).collect());
        let mut counts = HashMap::new();
        for (id, count) in &saved.counts {
            if *count > 0 && held.as_ref().is_none_or(|held| held.contains(id.as_str())) {
                counts.insert(id.clone(), (now, *count));
            }
        }
        let changed = counts.len() != saved.counts.len() || !saved.running.is_empty();
        self.recovery_tracker = counts;
        for (target_id, run) in saved.running {
            let started = iso_from_unix_ms(run.started_ms);
            warn!(
                "Infra: the recovery run of {target_id} started at {started} did not end \
                 before the agent stopped; its outcome is unknown"
            );
            self.push_recovery_log(RecoveryLogEntry {
                ts: now_iso(),
                target_id,
                command: run.command,
                exit_code: -1,
                stdout: format!(
                    "OUTCOME UNKNOWN: the agent stopped before this run ended (it started at {started})"
                ),
                stderr: String::new(),
            });
        }
        if changed {
            self.save_recoveries();
        }
    }

    /// Save the recovery counts and the runs not yet ended, synced, owner
    /// readable only, here on this thread; with neither, remove the file.
    /// False when the disk did not take it. A config applied or removed,
    /// and the start, save this way; a recovery run writes its
    /// [`Self::recoveries_snapshot`] on the blocking pool.
    pub fn save_recoveries(&self) -> bool {
        self.recoveries_snapshot().write_here()
    }

    /// The recovery counts and the runs not yet ended as they stand,
    /// numbered: what `infra-recovery.json` is to hold. Taken under the
    /// Infra lock, it can be written outside it.
    pub fn recoveries_snapshot(&self) -> RecoverySnapshot {
        RecoverySnapshot {
            number: self.recovery_file.0.taken.fetch_add(1, Ordering::Relaxed) + 1,
            file: self.recovery_file.clone(),
            saved: SavedRecoveries {
                counts: self
                    .recovery_tracker
                    .iter()
                    .map(|(id, (_, count))| (id.clone(), *count))
                    .collect(),
                running: self
                    .runs_not_ended
                    .iter()
                    .map(|(id, run)| (id.clone(), run.clone()))
                    .collect(),
            },
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
    ///
    /// The loaded config is the picture the agent starts with: the digest
    /// takes its hash, and `seq` stays 1.
    pub fn load_config(&mut self) {
        match std::fs::read_to_string(&self.config_path) {
            Ok(data) => match serde_json::from_str::<InfraConfig>(&data) {
                Ok(cfg) => {
                    self.seed_results(&cfg);
                    self.config = Some(cfg);
                    self.config_saved = true;
                    self.config_generation += 1;
                    self.publish(false);
                }
                Err(e) => warn!("Failed to parse infra config: {e}"),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!("Failed to read infra config: {e}"),
        }
    }

    /// Reflect a config in the results before any check has run: its
    /// version, and an `unknown` placeholder per target so the target set is
    /// visible. Entries that already carry a result are left alone. It
    /// signals nothing: its caller says whether this was a change.
    fn seed_results(&mut self, cfg: &InfraConfig) {
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
                    consecutive_within: 0,
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

    /// Persist config to disk (atomic write via tmp + rename), and
    /// remember whether it reached the disk.
    pub fn save_config(&mut self) -> bool {
        self.config_saved = self.write_config();
        self.config_saved
    }

    fn write_config(&self) -> bool {
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

    /// Log a recovery run, evicting the oldest entry past the ring's size.
    /// A logged run is a change worth pushing: `recoveries` and `seq` move,
    /// so a collector that has stopped pulling learns of every run.
    pub fn push_recovery_log(&mut self, entry: RecoveryLogEntry) {
        if self.results.recovery_log.len() >= MAX_RECOVERY_LOG {
            self.results.recovery_log.remove(0);
        }
        self.results.recovery_log.push(entry);
        {
            let mut cell = self.digest.cell();
            cell.recoveries = cell.recoveries.saturating_add(1);
        }
        self.changed();
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

/// Write a file owner-readable only, synced, through a temporary file
/// renamed over it, then sync its directory: a power cut leaves either the
/// old file or this one, never a part of it.
fn write_synced(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    write_private(&tmp, data)?;
    std::fs::rename(&tmp, path)?;
    // The rename itself, on a filesystem that keeps it only once its
    // directory is synced; best effort, since not every one takes it.
    if let Some(dir) = path.parent().and_then(|dir| std::fs::File::open(dir).ok()) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// `<state_dir>/infra-recovery.json` (ADR-010 decision 9), and the order
/// its writes reach it in.
///
/// A save is a [`RecoverySnapshot`], taken under the Infra lock and
/// numbered then. A recovery run writes its own on the blocking pool,
/// outside that lock, and a count cleared on the check path hands its own
/// to the pool: a synced write to a unit's flash can take hundreds of
/// milliseconds, and a one-core unit has one async worker, which the
/// forwarder, the checks and every route share. Writes go one at a time,
/// and a snapshot never replaces a newer one on disk, whatever order the
/// pool runs them in.
#[derive(Debug, Clone)]
pub struct RecoveryFile(Arc<RecoveryFileInner>);

#[derive(Debug)]
struct RecoveryFileInner {
    path: PathBuf,
    /// The number of the last snapshot taken.
    taken: AtomicU64,
    /// The number of the snapshot on disk, held while a write runs.
    on_disk: StdMutex<u64>,
}

impl RecoveryFile {
    fn new(path: PathBuf) -> Self {
        Self(Arc::new(RecoveryFileInner {
            path,
            taken: AtomicU64::new(0),
            on_disk: StdMutex::new(0),
        }))
    }

    /// Where the file is.
    pub fn path(&self) -> &Path {
        &self.0.path
    }
}

/// The recovery counts and the runs not yet ended as they stood when it
/// was taken, numbered, for [`RecoveryFile`].
#[derive(Debug)]
#[must_use = "a snapshot reaches the disk only once it is written"]
pub struct RecoverySnapshot {
    file: RecoveryFile,
    number: u64,
    saved: SavedRecoveries,
}

impl RecoverySnapshot {
    /// Write it on the blocking pool, and wait until the disk has it. True
    /// when it, or a newer snapshot, is on disk.
    pub async fn write(self) -> bool {
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => runtime
                .spawn_blocking(move || self.write_here())
                .await
                .unwrap_or(false),
            Err(_) => self.write_here(),
        }
    }

    /// Hand it to the blocking pool, and do not wait.
    pub fn hand_off(self) {
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => drop(runtime.spawn_blocking(move || self.write_here())),
            Err(_) => {
                self.write_here();
            }
        }
    }

    /// Write it on this thread, synced, owner readable only; with nothing
    /// to save, remove the file. True when it, or a newer snapshot, is on
    /// disk: a newer one holds every run this one holds as not ended, since
    /// only a run's end, saved after its command, marks it ended.
    pub fn write_here(self) -> bool {
        let file = &self.file.0;
        let mut on_disk = file.on_disk.lock().unwrap_or_else(PoisonError::into_inner);
        if *on_disk >= self.number {
            return true;
        }
        let written = if self.saved.counts.is_empty() && self.saved.running.is_empty() {
            match std::fs::remove_file(&file.path) {
                Ok(()) => true,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
                Err(e) => {
                    warn!("Failed to remove infra recovery counts: {e}");
                    false
                }
            }
        } else if let Ok(data) = serde_json::to_vec(&self.saved) {
            if let Some(parent) = file.path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match write_synced(&file.path, &data) {
                Ok(()) => true,
                Err(e) => {
                    warn!("Failed to save infra recovery counts: {e}");
                    false
                }
            }
        } else {
            warn!("Failed to serialize infra recovery counts");
            false
        };
        if written {
            *on_disk = self.number;
        }
        written
    }
}

/// What `<state_dir>/infra-recovery.json` holds (ADR-010 decision 9).
#[derive(Debug, Default, Serialize, Deserialize)]
struct SavedRecoveries {
    /// Each target's recovery runs since its count last cleared.
    #[serde(default)]
    counts: BTreeMap<String, u32>,
    /// The runs started and not yet logged: not ended.
    #[serde(default)]
    running: BTreeMap<String, SavedRun>,
}

/// A recovery run saved as not ended, written and synced before its
/// command starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedRun {
    /// When it started, unix ms: for its log entry only, since the wall
    /// clock can step. The cooldown reads the monotonic clock.
    pub started_ms: u64,
    pub command: String,
}

/// Current time as ISO 8601 string (no chrono dependency).
pub fn now_iso() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    iso_from_unix_secs(dur.as_secs())
}

/// A unix time in milliseconds as an ISO 8601 string, to the second.
fn iso_from_unix_ms(ms: u64) -> String {
    iso_from_unix_secs(ms / 1000)
}

#[allow(clippy::many_single_char_names)]
fn iso_from_unix_secs(secs: u64) -> String {
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

/// Current time as Unix epoch milliseconds.
pub fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Consumer side of the producer contract with the fleet app. The fleet
    /// test `infra-config.contract.test.ts` asserts it emits exactly these
    /// bytes (`fixtures/http-api-target.push.json`) for an access-point
    /// target; this test asserts the device accepts them. Change both or
    /// neither.
    // A customer coach's router certificate fingerprint: a public value, not a secret.
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
            consecutive_within: 1,
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

        // The loaded config is the picture the agent starts with: `seq`
        // stays 1, and the hash names that picture.
        let stamp = state.stamp();
        assert_eq!(stamp.seq, 1);
        assert_eq!(
            stamp.hash,
            picture_hash(&picture_text(
                stamp.boot,
                7,
                [(
                    "6f0b2c4e-9d3a-4c1f-8e2b-1a2b3c4d5e6f",
                    TargetStatus::Unknown
                )]
            ))
        );
        assert_eq!(state.digest().group().off, None, "a monitor is expected");
        std::fs::remove_dir_all(&dir).ok();
    }

    // ─── The digest (ADR-010) ────────────────────────────────────────

    /// The test vector `docs/http-api.md` gives, for the fleet's tests too.
    const VECTOR_TEXT: &str = "1790000000000\n7\nSwitch down\napi-1 up\nt10 degraded\nt9 unknown\n";
    const VECTOR_HASH: &str = "42189b104bcb1637";

    #[test]
    fn the_hash_is_its_test_vector() {
        let text = picture_text(
            1_790_000_000_000,
            7,
            [
                ("t9", TargetStatus::Unknown),
                ("api-1", TargetStatus::Up),
                ("t10", TargetStatus::Degraded),
                ("Switch", TargetStatus::Down),
            ],
        );
        // Byte order of the ids: an upper case letter before a lower case
        // one, `t10` before `t9`.
        assert_eq!(text, VECTOR_TEXT);
        assert_eq!(picture_hash(&text), VECTOR_HASH);
        // A unit that holds nothing: the start and version 0.
        assert_eq!(picture_text(1_790_000_000_000, 0, []), "1790000000000\n0\n");
        assert_eq!(
            picture_hash("1790000000000\n0\n"),
            "c2ccc438d385f035",
            "16 lowercase hex characters"
        );
    }

    fn ping_target(id: &str) -> InfraTarget {
        InfraTarget {
            id: id.into(),
            name: format!("Target {id}"),
            check: CheckSpec::Ping {
                host: "192.0.2.1".into(),
                timeout_ms: None,
            },
            degraded_threshold_ms: 200,
            down_after_consecutive: 3,
            up_after_consecutive: 2,
            interval_secs: None,
            recovery: None,
        }
    }

    fn config(version: u32, ids: &[&str]) -> InfraConfig {
        InfraConfig {
            version,
            check_interval_secs: 60,
            targets: ids.iter().map(|id| ping_target(id)).collect(),
        }
    }

    #[test]
    fn the_hash_is_the_same_whatever_the_order_of_the_targets() {
        let digest = Arc::new(Digest::new(1_790_000_000_000, false));
        let mut forward = InfraState::with_digest("/tmp/sctl-infra-digest-test", digest);
        forward.hold_config(config(7, &["t9", "api-1", "t10", "Switch"]));
        let backward_digest = Arc::new(Digest::new(1_790_000_000_000, false));
        let mut backward = InfraState::with_digest("/tmp/sctl-infra-digest-test", backward_digest);
        backward.hold_config(config(7, &["Switch", "t10", "api-1", "t9"]));
        assert_eq!(forward.stamp().hash, backward.stamp().hash);
        assert_eq!(
            forward.stamp().hash,
            picture_hash(&picture_text(
                1_790_000_000_000,
                7,
                ["t9", "api-1", "t10", "Switch"].map(|id| (id, TargetStatus::Unknown))
            ))
        );
    }

    #[test]
    fn an_agent_starts_at_seq_1_holding_nothing() {
        let state = InfraState::with_digest(
            "/tmp/sctl-infra-digest-test",
            Arc::new(Digest::new(1_790_000_000_000, false)),
        );
        let group = state.digest().group();
        assert_eq!(
            (group.rounds, group.seq, group.off),
            (0, 1, Some(Off::NoConfig))
        );
        assert_eq!(group.hash, "c2ccc438d385f035");
        assert_eq!(*state.watch_changes().borrow(), 1, "the watch holds `seq`");
    }

    #[test]
    fn an_identical_push_moves_no_seq_and_a_changed_one_does() {
        let mut state = InfraState::new("/tmp/sctl-infra-digest-test");
        assert!(state.hold_config(config(7, &["t1"])));
        let after_first = state.stamp();
        assert_eq!(after_first.seq, 2, "a config applied is a change");

        // Every field and the version equal: nothing changes.
        assert!(!state.hold_config(config(7, &["t1"])));
        assert_eq!(state.stamp(), after_first);

        // One field changed under the same version: applied, a change,
        // with the same picture.
        let mut moved = config(7, &["t1"]);
        moved.targets[0].check = CheckSpec::Ping {
            host: "192.0.2.2".into(),
            timeout_ms: None,
        };
        assert!(state.hold_config(moved));
        assert_eq!(state.stamp().seq, 3);
        assert_eq!(state.stamp().hash, after_first.hash);

        // Another target set: a new picture.
        assert!(state.hold_config(config(8, &["t1", "t2"])));
        assert_eq!(state.stamp().seq, 4);
        assert_ne!(state.stamp().hash, after_first.hash);
        assert_eq!(state.results.targets.len(), 2);
    }

    #[test]
    fn removing_a_config_is_a_change_removing_nothing_is_not() {
        let mut state = InfraState::new("/tmp/sctl-infra-digest-test");
        let empty = state.stamp();
        assert!(!state.drop_config());
        assert_eq!(state.stamp(), empty);

        state.hold_config(config(7, &["t1"]));
        assert_eq!(state.digest().group().off, None);
        assert!(state.drop_config());
        let removed = state.stamp();
        assert_eq!(removed.seq, 3);
        assert_eq!(removed.hash, empty.hash, "the picture of holding nothing");
        assert_eq!(state.digest().group().off, Some(Off::NoConfig));
        assert!(!state.drop_config());
        assert_eq!(state.stamp(), removed);
    }

    #[test]
    fn a_logged_run_moves_seq_and_recoveries() {
        let mut state = InfraState::new("/tmp/sctl-infra-digest-test");
        state.hold_config(config(7, &["t1"]));
        let before = state.stamp();
        state.push_recovery_log(RecoveryLogEntry {
            ts: now_iso(),
            target_id: "t1".into(),
            command: "true".into(),
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
        });
        let after = state.stamp();
        assert_eq!(after.seq, before.seq + 1);
        assert_eq!(after.recoveries, 1);
        assert_eq!(after.hash, before.hash, "no status moved");
        assert_eq!(*state.watch_changes().borrow(), after.seq);
    }

    #[test]
    fn off_says_why_no_monitor_is_expected() {
        let normal = InfraState::with_digest(
            "/tmp/sctl-infra-digest-test",
            Arc::new(Digest::new(1, false)),
        );
        assert_eq!(normal.digest().group().off, Some(Off::NoConfig));

        // Safe mode, with or without a config: the monitor stays off.
        let mut safe = InfraState::with_digest(
            "/tmp/sctl-infra-digest-test",
            Arc::new(Digest::new(1, true)),
        );
        assert_eq!(safe.digest().group().off, Some(Off::SafeMode));
        safe.hold_config(config(7, &["t1"]));
        assert_eq!(safe.digest().group().off, Some(Off::SafeMode));

        let group = |off| {
            serde_json::to_value(Group {
                rounds: 3,
                seq: 1,
                hash: "c2ccc438d385f035".into(),
                off,
            })
            .unwrap()
        };
        assert_eq!(group(Some(Off::SafeMode))["off"], "safe_mode");
        assert_eq!(group(Some(Off::NoConfig))["off"], "no_config");
        assert!(group(None).get("off").is_none(), "absent, not null");
    }

    #[test]
    fn a_target_in_flight_is_not_started_twice_and_frees_itself() {
        let in_flight = InFlight::default();
        let flight = in_flight.start("t1").expect("free");
        assert!(in_flight.start("t1").is_none(), "already in flight");
        assert!(in_flight.contains("t1"));
        assert!(
            in_flight.permit_times().is_empty(),
            "waiting for a permit is not running"
        );
        let at = Instant::now();
        flight.took_permit(at);
        assert_eq!(in_flight.permit_times(), vec![at]);

        // A check that gave its permit back is no longer running, and its
        // target stays in flight until its result is in.
        flight.left_permit();
        assert!(in_flight.permit_times().is_empty());
        assert!(in_flight.start("t1").is_none(), "still in flight");
        flight.took_permit(at);

        // A clone is the same set: a rebuilt monitor sees it.
        let seen_by_another = in_flight.clone();
        assert!(seen_by_another.start("t1").is_none());
        drop(flight);
        assert!(!seen_by_another.contains("t1"));
        assert!(seen_by_another.start("t1").is_some());
    }

    #[tokio::test]
    async fn check_now_gets_the_result_of_the_check_in_flight() {
        let in_flight = InFlight::default();
        let flight = in_flight.take_or_wait("t1").expect("free");
        let mut early = in_flight.take_or_wait("t1").expect_err("in flight");
        assert!(early.borrow().is_none(), "no result yet");

        flight.publish(&checks::CheckResult {
            ok: true,
            latency_ms: Some(42),
            ..checks::CheckResult::default()
        });
        // One that comes after the result, while the place is held (as a
        // recovery run holds it), has it at once.
        let late = in_flight.take_or_wait("t1").expect_err("still held");
        assert_eq!(late.borrow().as_ref().and_then(|r| r.latency_ms), Some(42));
        drop(flight);
        let seen = early.wait_for(Option::is_some).await.expect("published");
        assert_eq!(seen.as_ref().and_then(|r| r.latency_ms), Some(42));
        drop(seen);
        assert!(in_flight.take_or_wait("t1").is_ok(), "free again");

        // A check cancelled with its monitor ends with no result.
        let cancelled = in_flight.start("t2").expect("free");
        let mut waiting = in_flight.take_or_wait("t2").expect_err("in flight");
        drop(cancelled);
        assert!(waiting.wait_for(Option::is_some).await.is_err());
    }

    /// A run writes its record on the blocking pool, which runs writes in
    /// any order: a snapshot never replaces a newer one on disk.
    #[test]
    fn an_older_recovery_snapshot_never_replaces_a_newer_one() {
        let dir = std::env::temp_dir().join(format!("sctl-infra-snapshots-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut state = InfraState::new(dir.to_str().unwrap());
        let path = dir.join("infra-recovery.json");
        let saved =
            || -> Value { serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap() };

        state
            .recovery_tracker
            .insert("pdu".into(), (Instant::now(), 1));
        let older = state.recoveries_snapshot();
        state
            .recovery_tracker
            .insert("cam".into(), (Instant::now(), 2));
        let newer = state.recoveries_snapshot();
        assert!(newer.write_here());
        assert!(older.write_here(), "a newer one is on disk");
        assert_eq!(
            saved(),
            serde_json::json!({"counts": {"cam": 2, "pdu": 1}, "running": {}})
        );

        // One taken after both is written, nothing left to save included.
        state.recovery_tracker.clear();
        assert!(state.save_recoveries());
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(start_paused = true)]
    async fn one_run_per_target_and_an_upgrade_waits_for_every_run() {
        let runs = Runs::default();
        let pdu = runs.start("pdu").expect("free");
        assert!(runs.start("pdu").is_none(), "one run per target at a time");
        let cam = runs.start("cam").expect("another target");
        assert!(runs.contains("pdu") && runs.contains("cam"));
        assert_eq!(runs.in_progress(), 2);

        let waiter = {
            let runs = runs.clone();
            tokio::spawn(async move {
                let at = Instant::now();
                (runs.wait_idle(Duration::from_mins(5)).await, at.elapsed())
            })
        };
        tokio::time::sleep(Duration::from_secs(10)).await;
        drop(pdu);
        assert!(!runs.contains("pdu"));
        tokio::time::sleep(Duration::from_secs(10)).await;
        drop(cam);
        assert_eq!(
            waiter.await.unwrap(),
            (true, Duration::from_secs(20)),
            "until the last run ended"
        );

        // A run that does not end: at most the limit. Bounded a second past
        // it: on the paused clock with no other timer, a wait that lost its
        // limit would wait for good, and the run would hang.
        let stuck = runs.start("pdu").expect("free again");
        let at = Instant::now();
        let idle = tokio::time::timeout(
            Duration::from_mins(5) + Duration::from_secs(1),
            runs.wait_idle(Duration::from_mins(5)),
        )
        .await
        .expect("bounded by the limit");
        assert!(!idle);
        assert_eq!(at.elapsed(), Duration::from_mins(5));
        drop(stuck);
        assert!(runs.wait_idle(Duration::ZERO).await, "none in progress");
    }
}
