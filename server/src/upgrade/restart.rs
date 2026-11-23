//! The record of the agent's own restart (ADR-014): written by the running
//! agent when it accepts a request, by the helper when the restart ends,
//! read by the agent at start, carried by `GET /api/upgrade` and by the
//! `upgrade.state` push as `restart`. Lives at
//! `<state_dir>/upgrade/restart.json`, beside the upgrade's state, on
//! persistent storage: `data_dir` is in `/tmp` on the units whose init
//! removes its run directory at every start, and the outcome has to be
//! there after the start.
//!
//! Two values name a process. `boot` is when it started, in unix ms: what
//! `net.state` and `infra.state` carry, and what a caller matches its
//! reports against. `instance` is opaque, made at start without a wait: what
//! `GET /api/health` carries, the route that needs no key, and what the
//! helper tells two processes apart by.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::apply::lock_is_free;
use super::state::upgrade_dir;

/// The record's version, `v`.
pub const VERSION: u32 = 1;
/// The record's file name in the upgrade directory.
pub const FILE_NAME: &str = "restart.json";

/// Where a restart is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Nothing asked, nothing to report.
    Idle,
    /// Answered: the agent is ending what the restart ends.
    Accepted,
    /// The helper has the unit.
    Restarting,
    Done,
}

/// How a restart ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// A new process answered health twice.
    Ok,
    /// The process that accepted the request still answered at the end of
    /// the window: the restart command did not stop it.
    NotRestarted,
    /// No new process answered twice within the window.
    NotHealthy,
    /// The helper never ran.
    NotStarted,
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde's `skip_serializing_if` passes a reference.
fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// The record, as persisted and as reported. A key with nothing to say is
/// left out, so an idle record is `{"v": 1, "phase": "idle"}`: it rides
/// every `upgrade.state` a unit sends.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartState {
    pub v: u32,
    pub phase: Phase,
    /// The caller's name for this restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Outcome>,
    /// One line for a person, beside `outcome`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The `boot` of the process that accepted the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_before: Option<u64>,
    /// The `boot` of the process the helper proved, written by that process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_after: Option<u64>,
    /// The `instance` of the process that accepted the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_before: Option<String>,
    /// The `instance` the helper read from the new process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_after: Option<String>,
    /// How many times the helper ran the restart command: 2 means the first
    /// run started nothing, which is a defect to chase.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    /// The last kibibyte of the helper's log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_tail: Option<String>,
}

impl RestartState {
    /// Nothing asked, nothing to report.
    pub fn idle() -> Self {
        Self {
            v: VERSION,
            phase: Phase::Idle,
            request_id: None,
            outcome: None,
            detail: None,
            boot_before: None,
            boot_after: None,
            instance_before: None,
            instance_after: None,
            attempts: 0,
            accepted_at: None,
            ended_at: None,
            log_tail: None,
        }
    }

    /// A request accepted by the process `me`.
    pub fn accepted(request_id: Option<String>, me: &Process<'_>) -> Self {
        Self {
            phase: Phase::Accepted,
            request_id,
            boot_before: Some(me.boot),
            instance_before: Some(me.instance.to_string()),
            accepted_at: Some(crate::infra::now_iso()),
            ..Self::idle()
        }
    }

    /// Whether a restart is accepted or its helper is, or was, in charge.
    pub fn in_flight(&self) -> bool {
        matches!(self.phase, Phase::Accepted | Phase::Restarting)
    }

    /// The helper has the unit.
    #[must_use]
    pub fn restarting(mut self) -> Self {
        self.phase = Phase::Restarting;
        self
    }

    /// Finish with `outcome` and the line that says why.
    #[must_use]
    pub fn finished(mut self, outcome: Outcome, detail: Option<String>) -> Self {
        self.phase = Phase::Done;
        self.outcome = Some(outcome);
        self.detail = detail;
        self.ended_at = Some(crate::infra::now_iso());
        self
    }

    /// Attach the helper's log tail.
    #[must_use]
    pub fn with_log_tail(mut self, tail: Option<String>) -> Self {
        self.log_tail = tail;
        self
    }
}

/// At most this many job names in [`Ends::jobs`].
pub const ENDS_JOB_NAMES: usize = 16;
/// Each job name is cut to this many bytes, on a character boundary.
pub const ENDS_JOB_NAME_BYTES: usize = 64;

/// What a restart ends on the unit, as the agent holds it when it is asked
/// (ADR-014 decision 4): the answer says so before anything stops.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Ends {
    /// Running terminals.
    pub sessions: usize,
    /// Running terminals with a client attached now.
    pub attached: usize,
    /// The running jobs' names, at most [`ENDS_JOB_NAMES`]; a job with no
    /// name is named by its session id.
    pub jobs: Vec<String>,
    /// File transfers in flight.
    pub transfers: usize,
}

impl Ends {
    /// Build it from the session list and the phase of each transfer held.
    pub fn of<'a>(
        sessions: &[crate::sessions::SessionListItem],
        transfer_phases: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        let running = || sessions.iter().filter(|s| s.status == "running");
        let terminals = || running().filter(|s| s.kind == "terminal");
        let jobs = running()
            .filter(|s| s.kind == "job")
            .take(ENDS_JOB_NAMES)
            .map(|s| {
                let name = s.name.as_deref().unwrap_or(&s.session_id);
                let mut end = name.len().min(ENDS_JOB_NAME_BYTES);
                while !name.is_char_boundary(end) {
                    end -= 1;
                }
                name[..end].to_string()
            })
            .collect();
        Self {
            sessions: terminals().count(),
            attached: terminals().filter(|s| s.attached).count(),
            jobs,
            transfers: transfer_phases
                .into_iter()
                .filter(|phase| matches!(*phase, "init" | "transferring" | "paused" | "verifying"))
                .count(),
        }
    }

    /// Read it from the agent's sessions and transfers.
    pub async fn read(
        sessions: &crate::sessions::SessionManager,
        transfers: &crate::gawdxfer::manager::TransferManager,
    ) -> Self {
        let sessions = sessions.list_sessions().await;
        let transfers = transfers.list().await;
        Self::of(
            &sessions,
            transfers.transfers.iter().map(|t| t.phase.as_str()),
        )
    }
}

/// The process that reads or writes the record.
#[derive(Clone, Copy, Debug)]
pub struct Process<'a> {
    /// Opaque, made at start (`GET /api/health`'s `instance`).
    pub instance: &'a str,
    /// When it started, unix ms (`net.state`'s `boot`).
    pub boot: u64,
}

/// The clocks and the pid a process is told apart by when the kernel gives
/// nothing random without a wait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Moment {
    pid: u32,
    /// The monotonic clock, in nanoseconds.
    monotonic_ns: u128,
    /// The wall clock, in nanoseconds since the epoch.
    wall_ns: u128,
}

impl Moment {
    fn now() -> Self {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `ts` is a valid timespec, and CLOCK_MONOTONIC is always
        // there on Linux. A failed call leaves the zeroes.
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut ts) };
        #[allow(clippy::cast_sign_loss)] // The monotonic clock is not negative.
        let monotonic_ns = (ts.tv_sec as u128) * 1_000_000_000 + ts.tv_nsec as u128;
        Self {
            pid: std::process::id(),
            monotonic_ns,
            wall_ns: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        }
    }
}

/// Two hashers keyed by the kernel without a wait: std's hash keys come
/// from getrandom(2) asked not to wait, and from `/dev/urandom` where the
/// call is missing (`net_tools/snmp.rs`, `first_request_id`, draws the same
/// way for the same reason). `None` when even that cannot be read.
fn kernel_keyed() -> Option<[std::collections::hash_map::DefaultHasher; 2]> {
    use std::hash::BuildHasher;
    std::panic::catch_unwind(|| {
        [(); 2].map(|()| std::collections::hash_map::RandomState::new().build_hasher())
    })
    .ok()
}

/// The instance for `moment`, from `keyed` hashers when the kernel gave
/// some, else from the moment alone.
fn instance_from(
    keyed: Option<[std::collections::hash_map::DefaultHasher; 2]>,
    moment: Moment,
) -> String {
    use std::hash::Hasher;
    let hashers = keyed.unwrap_or_else(|| {
        // Nothing random without a wait: the pid and the two clocks. An
        // instance only has to differ from the last start's, and no two
        // starts share all three.
        [(); 2].map(|()| std::collections::hash_map::DefaultHasher::new())
    });
    let mut halves = [0_u64; 2];
    for (lane, (mut hasher, half)) in (0_u64..).zip(hashers.into_iter().zip(&mut halves)) {
        hasher.write_u64(lane);
        hasher.write_u32(moment.pid);
        hasher.write_u128(moment.monotonic_ns);
        hasher.write_u128(moment.wall_ns);
        *half = hasher.finish();
    }
    format!("{:016x}{:016x}", halves[0], halves[1])
}

/// An opaque value for a process that starts now (`GET /api/health`'s
/// `instance`). It is an identifier, not a secret, and it is drawn on the
/// start path, before the listener binds: it must never wait for the
/// kernel's entropy pool. A generator that waits (getrandom(2) as a UUID
/// draws it, which on a kernel without the call polls `/dev/random` with no
/// limit) would leave a unit with a thin pool silent on health, on the API
/// and on the tunnel while it only waits. So: random where the kernel gives
/// it without waiting, mixed with the pid and the clocks, and from those
/// alone where it gives nothing.
pub fn new_instance() -> String {
    instance_from(kernel_keyed(), Moment::now())
}

/// The record's path.
pub fn record_path(state_dir: &str) -> PathBuf {
    upgrade_dir(state_dir).join(FILE_NAME)
}

/// The helper's directory: its plan, its lock and its log.
pub fn helper_dir(state_dir: &str) -> PathBuf {
    upgrade_dir(state_dir).join("restart")
}

/// Read the record; `None` when absent or not a record.
pub fn load(path: &Path) -> Option<RestartState> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Write the record atomically (write beside, rename over).
pub fn save(path: &Path, state: &RestartState) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(state).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// The running agent's view of the record: the file, and a watch channel
/// the tunnel's `upgrade.state` forwarder and `GET /api/upgrade` read.
pub struct Handle {
    path: PathBuf,
    helper_dir: PathBuf,
    tx: tokio::sync::watch::Sender<RestartState>,
    /// Until when a helper this process started is given to take its lock:
    /// before that, a free lock does not mean the helper is gone.
    helper_grace_until: std::sync::Mutex<Option<std::time::Instant>>,
}

impl Handle {
    /// Read the file (or start idle) and open the channel.
    pub fn open(state_dir: &str) -> Self {
        let path = record_path(state_dir);
        let state = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                warn!(
                    "restart: {} is not a record ({e}); reading it as idle",
                    path.display()
                );
                RestartState::idle()
            }),
            Err(_) => RestartState::idle(),
        };
        let (tx, _) = tokio::sync::watch::channel(state);
        Self {
            path,
            helper_dir: helper_dir(state_dir),
            tx,
            helper_grace_until: std::sync::Mutex::new(None),
        }
    }

    /// Where the record is.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Where the helper's plan, lock and log are.
    pub fn helper_dir(&self) -> &Path {
        &self.helper_dir
    }

    /// The current record.
    pub fn current(&self) -> RestartState {
        self.tx.borrow().clone()
    }

    /// A receiver that wakes on every change.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<RestartState> {
        self.tx.subscribe()
    }

    /// Persist and publish `state`. A failed write is logged; the channel
    /// still moves so the tunnel tells what the agent knows.
    pub fn set(&self, state: RestartState) {
        if let Err(e) = save(&self.path, &state) {
            warn!("restart: could not write {}: {e}", self.path.display());
        }
        self.tx.send_replace(state);
    }

    /// Re-read the file (the helper wrote it) and publish what it says.
    pub fn reload(&self) -> Option<RestartState> {
        let state = load(&self.path)?;
        if *self.tx.borrow() != state {
            self.tx.send_replace(state.clone());
        }
        Some(state)
    }

    /// This process has just started a helper: for `grace` a free lock
    /// does not mean it is gone, since it has yet to take it.
    pub fn helper_started(&self, grace: std::time::Duration) {
        let until = std::time::Instant::now() + grace;
        *self
            .helper_grace_until
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(until);
    }

    /// Whether the helper is gone: its lock is free, or it never had a
    /// directory to hold one in. Never true in the grace a helper this
    /// process started is given to take its lock.
    pub fn helper_gone(&self) -> bool {
        let in_grace = self
            .helper_grace_until
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some_and(|until| std::time::Instant::now() < until);
        !in_grace && (!self.helper_dir.is_dir() || lock_is_free(&self.helper_dir))
    }
}

/// How long the agent waits on its helper, and how often it looks.
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    /// How often the record and the helper's lock are looked at.
    pub poll: std::time::Duration,
    /// How long a helper just started is given to take its lock.
    pub grace: std::time::Duration,
    /// The helper's settle time before it runs the command.
    pub settle: std::time::Duration,
    /// The restart command's deadline.
    pub command: std::time::Duration,
    /// One of the helper's windows.
    pub window: std::time::Duration,
    /// One health read's deadline.
    pub read: std::time::Duration,
    /// What the helper allows itself beyond its steps.
    pub helper_margin: std::time::Duration,
    /// What is allowed beyond the helper's hard limit before it is given
    /// up.
    pub margin: std::time::Duration,
}

impl Timing {
    /// The helper's real times.
    pub fn production() -> Self {
        Self {
            poll: std::time::Duration::from_secs(1),
            grace: std::time::Duration::from_secs(15),
            settle: super::apply::SETTLE,
            command: super::restart_apply::COMMAND_LIMIT,
            window: super::apply::HEALTH_WINDOW,
            read: super::restart_apply::READ_LIMIT,
            helper_margin: super::restart_apply::HARD_MARGIN,
            margin: std::time::Duration::from_secs(30),
        }
    }

    /// The longest a helper may take on `layout` before the agent gives it
    /// up: the helper's own hard limit (`restart_apply::Limits`), which is
    /// derived from its bounded steps, and a margin. The agent never calls
    /// a helper lost that is still inside its own time.
    pub fn bound(&self, layout: super::install::Layout) -> std::time::Duration {
        super::restart_apply::Limits::with(
            layout,
            self.settle,
            self.command,
            self.window,
            super::apply::HEALTH_INTERVAL,
            self.read,
            self.helper_margin,
        )
        .hard
            + self.margin
    }
}

/// What a record says when its helper is gone, or overdue, without an
/// outcome while the process that accepted the request still runs.
pub const HELPER_LOST: &str =
    "helper lost: it ended, or ran out of time, without an outcome; this process was not restarted";

/// Settle a record whose helper is gone without an outcome, as the process
/// `me` reads it. `None` when there is nothing to settle: no restart in
/// flight, or a helper still at work.
fn settled(handle: &Handle, me: &Process<'_>) -> Option<RestartState> {
    let state = handle.current();
    match state.phase {
        Phase::Accepted => Some(state.finished(
            Outcome::NotStarted,
            Some("the agent stopped before it handed off to its helper".to_string()),
        )),
        Phase::Restarting if handle.helper_gone() => Some(settled_now(state, me)),
        Phase::Restarting | Phase::Idle | Phase::Done => None,
    }
}

/// End a `restarting` record that no helper will end, as the process `me`
/// reads it.
fn settled_now(state: RestartState, me: &Process<'_>) -> RestartState {
    if state.instance_before.as_deref() == Some(me.instance) {
        state.finished(Outcome::NotRestarted, Some(HELPER_LOST.to_string()))
    } else {
        let mut ok = state.finished(
            Outcome::Ok,
            Some("the helper did not report; a new process is running".to_string()),
        );
        ok.instance_after = Some(me.instance.to_string());
        ok.boot_after = Some(me.boot);
        ok
    }
}

/// At start: settle what the previous process left, and write this
/// process's `boot` where the helper proved it. Returns whether the helper
/// is still working, in which case the caller watches the record.
pub fn reconcile(handle: &Handle, me: &Process<'_>) -> bool {
    if let Some(state) = settled(handle, me) {
        info!(outcome = ?state.outcome, "restart: settled at start: {}", state.detail.as_deref().unwrap_or(""));
        handle.set(state);
        return false;
    }
    if handle.current().phase == Phase::Restarting {
        info!("restart: the helper is still at it; watching its outcome");
        return true;
    }
    stamp(handle, me);
    false
}

/// A helper that ended without the agent seeing it (`POST /api/restart`
/// and `POST /api/upgrade` ask before they judge what is in flight).
pub fn settle_lost_helper(handle: &Handle, me: &Process<'_>) {
    if handle.current().phase == Phase::Restarting {
        handle.reload();
        if let Some(state) = settled(handle, me) {
            handle.set(state);
        }
    }
}

/// The process the helper proved writes its `boot` into the record, once.
/// Returns whether it wrote.
pub fn stamp(handle: &Handle, me: &Process<'_>) -> bool {
    let mut state = handle.current();
    let proved = state.phase == Phase::Done
        && state.outcome == Some(Outcome::Ok)
        && state.instance_after.as_deref() == Some(me.instance)
        && state.boot_after.is_none();
    if proved {
        state.boot_after = Some(me.boot);
        handle.set(state);
    }
    proved
}

/// What the agent says when it reads an outcome. A second run of the
/// restart command is never quiet: the first one started nothing.
pub fn note_outcome(state: &RestartState) {
    if state.phase != Phase::Done {
        return;
    }
    if state.attempts >= 2 {
        warn!(
            request_id = state.request_id.as_deref().unwrap_or(""),
            outcome = ?state.outcome,
            "restart: the restart command was run {} times: the first run started nothing. {}",
            state.attempts,
            state.detail.as_deref().unwrap_or("")
        );
    } else {
        info!(
            request_id = state.request_id.as_deref().unwrap_or(""),
            outcome = ?state.outcome,
            "restart: ended. {}",
            state.detail.as_deref().unwrap_or("")
        );
    }
}

/// Wait on the helper, with a bound: look at the record every
/// `timing.poll` until the helper ends it, the helper is found gone without
/// an outcome, or `bound` passes. A helper that is gone or overdue is
/// settled as this process reads it, so a helper that died (or hangs) never
/// leaves a restart in flight for good. Returns the record as it ended.
///
/// Both processes run it: the one that started the helper, which is still
/// here only if the restart did not stop it, and the one the restart
/// started, while the helper reads its health.
pub async fn supervise(
    handle: std::sync::Arc<Handle>,
    instance: String,
    boot: u64,
    timing: Timing,
    bound: std::time::Duration,
) -> RestartState {
    let me = Process {
        instance: &instance,
        boot,
    };
    let started = std::time::Instant::now();
    loop {
        tokio::time::sleep(timing.poll).await;
        // The outcome first: a helper that has written it and gone is done,
        // not lost.
        handle.reload();
        if handle.current().phase != Phase::Restarting {
            break;
        }
        let overdue = started.elapsed() >= bound;
        if overdue || handle.helper_gone() {
            handle.reload();
            let state = handle.current();
            if state.phase == Phase::Restarting {
                let lost = settled_now(state, &me);
                warn!(
                    overdue,
                    "restart: the helper left no outcome: {}",
                    lost.detail.as_deref().unwrap_or("")
                );
                handle.set(lost);
            }
            break;
        }
    }
    stamp(&handle, &me);
    let state = handle.current();
    note_outcome(&state);
    state
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const OLD: Process<'static> = Process {
        instance: "old-instance",
        boot: 1_790_000_000_000,
    };
    const NEW: Process<'static> = Process {
        instance: "new-instance",
        boot: 1_790_000_004_321,
    };

    fn bench(tag: &str) -> (PathBuf, Handle) {
        let dir =
            std::env::temp_dir().join(format!("sctl-restart-record-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let handle = Handle::open(dir.to_str().unwrap());
        (dir, handle)
    }

    #[test]
    fn an_idle_record_is_two_keys() {
        assert_eq!(
            serde_json::to_value(RestartState::idle()).unwrap(),
            json!({"v": 1, "phase": "idle"})
        );
    }

    #[test]
    fn the_record_round_trips_and_ignores_a_key_it_does_not_know() {
        let (dir, handle) = bench("rt");
        assert_eq!(handle.current(), RestartState::idle());
        let accepted = RestartState::accepted(Some("r-41".into()), &OLD);
        assert!(accepted.in_flight());
        let json = serde_json::to_value(&accepted).unwrap();
        assert_eq!(json["phase"], "accepted");
        assert_eq!(json["request_id"], "r-41");
        assert_eq!(json["boot_before"], 1_790_000_000_000_u64);
        assert_eq!(json["instance_before"], "old-instance");
        assert!(json.get("outcome").is_none() && json.get("attempts").is_none());
        handle.set(accepted.clone());
        assert_eq!(load(handle.path()), Some(accepted.clone()));

        let mut later = serde_json::to_value(accepted.clone().restarting()).unwrap();
        later["a_key_of_a_later_release"] = json!({"x": 1});
        std::fs::write(handle.path(), serde_json::to_vec(&later).unwrap()).unwrap();
        assert_eq!(handle.reload().unwrap().phase, Phase::Restarting);

        let done = accepted.finished(Outcome::NotHealthy, Some("nothing answered".into()));
        assert!(!done.in_flight() && done.ended_at.is_some());
        assert_eq!(
            serde_json::to_value(&done).unwrap()["outcome"],
            "not_healthy"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_that_is_not_a_record_reads_as_idle() {
        let (dir, handle) = bench("junk");
        std::fs::create_dir_all(handle.path().parent().unwrap()).unwrap();
        std::fs::write(handle.path(), b"not json").unwrap();
        let again = Handle::open(dir.to_str().unwrap());
        assert_eq!(again.current(), RestartState::idle());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_record_left_accepted_ends_as_not_started() {
        let (dir, handle) = bench("accepted");
        handle.set(RestartState::accepted(None, &OLD));
        assert!(!reconcile(&handle, &NEW));
        let state = handle.current();
        assert_eq!(state.outcome, Some(Outcome::NotStarted));
        assert_eq!(state.boot_after, None);
        assert_eq!(
            load(handle.path()).unwrap().outcome,
            Some(Outcome::NotStarted)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_lost_helper_is_settled_by_who_is_running() {
        let (dir, handle) = bench("lost");
        // A new process: the restart happened, and the helper did not report.
        handle.set(RestartState::accepted(Some("r-1".into()), &OLD).restarting());
        assert!(!reconcile(&handle, &NEW));
        let state = handle.current();
        assert_eq!(state.outcome, Some(Outcome::Ok));
        assert_eq!(state.instance_after.as_deref(), Some("new-instance"));
        assert_eq!(state.boot_after, Some(NEW.boot));
        assert!(state.detail.unwrap().contains("did not report"));

        // The same process: nothing restarted it, and its helper is lost.
        handle.set(RestartState::accepted(None, &OLD).restarting());
        std::fs::create_dir_all(handle.helper_dir()).unwrap();
        assert!(!reconcile(&handle, &OLD));
        assert_eq!(handle.current().outcome, Some(Outcome::NotRestarted));
        assert_eq!(handle.current().detail.as_deref(), Some(HELPER_LOST));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_working_helper_is_left_alone() {
        let (dir, handle) = bench("alive");
        std::fs::create_dir_all(handle.helper_dir()).unwrap();
        let _lock = crate::upgrade::apply::take_lock(handle.helper_dir()).unwrap();
        handle.set(RestartState::accepted(None, &OLD).restarting());
        assert!(reconcile(&handle, &NEW));
        assert_eq!(handle.current().phase, Phase::Restarting);
        settle_lost_helper(&handle, &NEW);
        assert_eq!(handle.current().phase, Phase::Restarting);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_process_the_helper_proved_writes_its_boot_once() {
        let (dir, handle) = bench("stamp");
        let mut ok = RestartState::accepted(Some("r-2".into()), &OLD)
            .restarting()
            .finished(Outcome::Ok, None);
        ok.instance_after = Some("new-instance".into());
        ok.attempts = 1;
        handle.set(ok.clone());
        // Another process is not the one the helper proved.
        assert!(!stamp(
            &handle,
            &Process {
                instance: "a-third",
                boot: 7
            }
        ));
        assert_eq!(handle.current().boot_after, None);
        assert!(!reconcile(&handle, &NEW));
        assert_eq!(handle.current().boot_after, Some(NEW.boot));
        assert_eq!(load(handle.path()).unwrap().boot_after, Some(NEW.boot));
        assert!(!stamp(&handle, &NEW), "once");
        // An outcome that is not ok takes no boot.
        handle.set(
            RestartState::accepted(None, &OLD)
                .restarting()
                .finished(Outcome::NotHealthy, None),
        );
        assert!(!stamp(&handle, &NEW));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Times small enough for a test: a look every 10 ms, 80 ms for the
    /// helper to take its lock, and a helper given up after 400 ms.
    const QUICK: Timing = Timing {
        poll: std::time::Duration::from_millis(10),
        grace: std::time::Duration::from_millis(80),
        settle: std::time::Duration::ZERO,
        command: std::time::Duration::ZERO,
        window: std::time::Duration::from_millis(300),
        read: std::time::Duration::ZERO,
        helper_margin: std::time::Duration::ZERO,
        margin: std::time::Duration::from_millis(100),
    };

    #[test]
    fn the_bound_is_the_helpers_own_time_and_a_margin() {
        use crate::upgrade::install::Layout;
        let real = Timing::production();
        // The helper's hard limit (2 s; a minute for the command, the 180 s
        // window and a 5 s read; 30 s), and 30 s more.
        assert_eq!(real.bound(Layout::UsrBin).as_secs(), 2 + 245 + 30 + 30);
        assert_eq!(real.bound(Layout::Ramboot), real.bound(Layout::Systemd));
        // gz-tmp may run the command a second time.
        assert_eq!(real.bound(Layout::GzTmp).as_secs(), 2 + 490 + 30 + 30);
    }

    #[tokio::test]
    async fn the_supervisor_publishes_what_the_helper_writes_and_stamps_it() {
        let (dir, handle) = bench("supervise");
        let handle = std::sync::Arc::new(handle);
        handle.set(RestartState::accepted(Some("r-3".into()), &OLD).restarting());
        std::fs::create_dir_all(handle.helper_dir()).unwrap();
        let lock = crate::upgrade::apply::take_lock(handle.helper_dir()).unwrap();
        let path = handle.path().to_path_buf();
        let supervisor = tokio::spawn(supervise(
            handle.clone(),
            NEW.instance.to_string(),
            NEW.boot,
            QUICK,
            std::time::Duration::from_secs(30),
        ));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let mut done = RestartState::accepted(Some("r-3".into()), &OLD)
            .restarting()
            .finished(Outcome::Ok, None);
        done.instance_after = Some(NEW.instance.to_string());
        done.attempts = 1;
        save(&path, &done).unwrap();
        drop(lock);
        let ended = tokio::time::timeout(std::time::Duration::from_secs(5), supervisor)
            .await
            .expect("the supervisor returns on done")
            .unwrap();
        assert_eq!(ended.outcome, Some(Outcome::Ok));
        assert_eq!(ended.boot_after, Some(NEW.boot));
        assert_eq!(handle.current(), ended);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A helper that died after it was started leaves no outcome. The
    /// process that started it does not wait for ever: once the helper's
    /// grace has passed and its lock is free, the record is settled.
    #[tokio::test]
    async fn a_helper_that_died_is_settled_once_its_lock_is_found_free() {
        let (dir, handle) = bench("died");
        let handle = std::sync::Arc::new(handle);
        handle.set(RestartState::accepted(Some("r-4".into()), &OLD).restarting());
        std::fs::create_dir_all(handle.helper_dir()).unwrap();
        handle.helper_started(QUICK.grace);
        // Just started: a free lock is not yet a lost helper.
        assert!(!handle.helper_gone());
        settle_lost_helper(&handle, &OLD);
        assert_eq!(handle.current().phase, Phase::Restarting);
        let at = std::time::Instant::now();
        let ended = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            supervise(
                handle.clone(),
                OLD.instance.to_string(),
                OLD.boot,
                QUICK,
                std::time::Duration::from_secs(30),
            ),
        )
        .await
        .expect("bounded by the helper's lock, not by the bound");
        assert!(at.elapsed() >= QUICK.grace, "not before its grace");
        assert_eq!(ended.outcome, Some(Outcome::NotRestarted));
        assert_eq!(ended.detail.as_deref(), Some(HELPER_LOST));
        assert_eq!(load(handle.path()).unwrap(), ended);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A helper that hangs holds its lock and writes nothing. Past its own
    /// time and the margin it is given up, lock held or not.
    #[tokio::test]
    async fn a_helper_that_hangs_is_given_up_at_the_bound() {
        use crate::upgrade::install::Layout;
        let (dir, handle) = bench("hung");
        let handle = std::sync::Arc::new(handle);
        handle.set(RestartState::accepted(None, &OLD).restarting());
        std::fs::create_dir_all(handle.helper_dir()).unwrap();
        let _held = crate::upgrade::apply::take_lock(handle.helper_dir()).unwrap();
        let bound = QUICK.bound(Layout::UsrBin);
        let at = std::time::Instant::now();
        let ended = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            supervise(
                handle.clone(),
                OLD.instance.to_string(),
                OLD.boot,
                QUICK,
                bound,
            ),
        )
        .await
        .expect("bounded");
        assert!(at.elapsed() >= bound);
        assert_eq!(ended.outcome, Some(Outcome::NotRestarted));
        assert_eq!(ended.detail.as_deref(), Some(HELPER_LOST));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn session(
        id: &str,
        kind: &str,
        status: &str,
        attached: bool,
        name: Option<&str>,
    ) -> crate::sessions::SessionListItem {
        crate::sessions::SessionListItem {
            session_id: id.to_string(),
            pid: 1,
            persistent: true,
            pty: kind == "terminal",
            kind: kind.to_string(),
            attached,
            status: status.to_string(),
            exit_code: None,
            idle: false,
            idle_timeout: 0,
            name: name.map(ToString::to_string),
            created_at: 0,
            user_allows_ai: false,
            ai_is_working: false,
            ai_activity: None,
            ai_status_message: None,
        }
    }

    #[test]
    fn ends_counts_what_runs_and_names_the_jobs() {
        assert_eq!(Ends::of(&[], []), Ends::default());
        let sessions = [
            session("s1", "terminal", "running", false, None),
            session("s2", "terminal", "running", true, Some("console")),
            session("s3", "terminal", "exited", true, None),
            session("j1", "job", "running", true, Some("pb:outlet-cycle")),
            session("j2", "job", "running", false, None),
            session("j3", "job", "exited", false, Some("pb:done")),
        ];
        let ends = Ends::of(
            &sessions,
            [
                "init",
                "transferring",
                "paused",
                "verifying",
                "complete",
                "failed",
                "aborted",
            ],
        );
        assert_eq!(
            ends,
            Ends {
                sessions: 2,
                attached: 1,
                jobs: vec!["pb:outlet-cycle".into(), "j2".into()],
                transfers: 4,
            }
        );
        assert_eq!(
            serde_json::to_value(&ends).unwrap(),
            json!({"sessions": 2, "attached": 1, "jobs": ["pb:outlet-cycle", "j2"], "transfers": 4})
        );
    }

    #[test]
    fn ends_names_at_most_sixteen_jobs_each_cut_on_a_character_boundary() {
        let many: Vec<_> = (0..20)
            .map(|i| session(&format!("j{i}"), "job", "running", false, None))
            .collect();
        assert_eq!(Ends::of(&many, []).jobs.len(), ENDS_JOB_NAMES);
        // 63 bytes, then a two-byte character across the cut.
        let long = format!("{}é-and-more", "x".repeat(63));
        let ends = Ends::of(&[session("j", "job", "running", false, Some(&long))], []);
        assert_eq!(ends.jobs, ["x".repeat(63)]);
        let exact = "y".repeat(64);
        let ends = Ends::of(
            &[session(
                "j",
                "job",
                "running",
                false,
                Some(&format!("{exact}z")),
            )],
            [],
        );
        assert_eq!(ends.jobs, [exact]);
    }

    #[test]
    fn an_instance_is_opaque_and_new_each_time() {
        let (a, b) = (new_instance(), new_instance());
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    /// Nothing on the start path may wait for entropy. The draw goes
    /// through std's hash keys, which the kernel gives without a wait, and
    /// through no generator that can block: not a UUID, not getrandom, not
    /// ring's.
    #[test]
    fn the_instance_is_drawn_without_a_generator_that_can_wait() {
        let source = include_str!("restart.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("the code before the tests");
        for waits in [
            "Uuid",
            "uuid::",
            "getrandom",
            "ring::",
            "OsRng",
            "/dev/random",
        ] {
            let used = production
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .any(|l| l.contains(waits));
            assert!(!used, "{waits} can wait for the kernel's pool");
        }
        assert!(production.contains("RandomState::new()"));
    }

    /// Where the kernel gives nothing, the instance is the pid and the two
    /// clocks: it still differs from the last start's, and it never waits.
    #[test]
    fn with_nothing_random_the_instance_is_the_pid_and_the_clocks() {
        let moment = Moment {
            pid: 4242,
            monotonic_ns: 61_000_000_000,
            wall_ns: 1_790_000_000_000_000_000,
        };
        let instance = instance_from(None, moment);
        assert_eq!(instance.len(), 32);
        assert!(instance.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(instance, instance_from(None, moment), "nothing else in it");
        // Its two halves are not one value twice.
        assert_ne!(instance[..16], instance[16..]);
        // Another pid, a later monotonic clock or a later wall clock: another.
        for other in [
            Moment {
                pid: 4243,
                ..moment
            },
            Moment {
                monotonic_ns: moment.monotonic_ns + 1,
                ..moment
            },
            Moment {
                wall_ns: moment.wall_ns + 1,
                ..moment
            },
        ] {
            assert_ne!(instance_from(None, other), instance, "{other:?}");
        }
        // With the kernel's keys, the same moment gives another value each
        // time: the keys are in it.
        let keyed = || instance_from(kernel_keyed(), moment);
        assert_ne!(keyed(), keyed());
        assert_ne!(keyed(), instance);
        // The clocks are read.
        let now = Moment::now();
        assert_eq!(now.pid, std::process::id());
        assert!(now.monotonic_ns > 0 && now.wall_ns > 0);
    }
}
