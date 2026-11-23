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
//! reports against. `instance` is opaque and random, made at start: what
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

/// The process that reads or writes the record.
#[derive(Clone, Copy, Debug)]
pub struct Process<'a> {
    /// Opaque, random, made at start (`GET /api/health`'s `instance`).
    pub instance: &'a str,
    /// When it started, unix ms (`net.state`'s `boot`).
    pub boot: u64,
}

/// An opaque value for a process that starts now: random, not a time, not
/// a counter, derived from nothing on the unit.
pub fn new_instance() -> String {
    uuid::Uuid::new_v4().simple().to_string()
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

    /// Whether the helper is gone: its lock is free, or it never had a
    /// directory to hold one in.
    pub fn helper_gone(&self) -> bool {
        !self.helper_dir.is_dir() || lock_is_free(&self.helper_dir)
    }
}

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
        Phase::Restarting if handle.helper_gone() => {
            if state.instance_before.as_deref() == Some(me.instance) {
                Some(state.finished(
                    Outcome::NotStarted,
                    Some(
                        "the helper is gone without an outcome, and the same process is running"
                            .to_string(),
                    ),
                ))
            } else {
                let mut ok = state.finished(
                    Outcome::Ok,
                    Some("the helper did not report; a new process is running".to_string()),
                );
                ok.instance_after = Some(me.instance.to_string());
                ok.boot_after = Some(me.boot);
                Some(ok)
            }
        }
        Phase::Restarting | Phase::Idle | Phase::Done => None,
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

/// Watch the record until the helper ends it, publishing every write; then
/// stamp it and say how it ended. Returns the record as it ended, or `None`
/// when the directory cannot be watched (the outcome is then read at the
/// next start).
pub async fn watch_until_done(
    handle: std::sync::Arc<Handle>,
    instance: String,
    boot: u64,
) -> Option<RestartState> {
    let dir = handle.path().parent().map(Path::to_path_buf)?;
    let watched = handle.clone();
    super::watch::watch_file(dir, FILE_NAME, move |_| {
        watched.reload().is_some_and(|s| s.phase == Phase::Done)
    })
    .await;
    let state = handle.current();
    if state.phase != Phase::Done {
        return None;
    }
    stamp(
        &handle,
        &Process {
            instance: &instance,
            boot,
        },
    );
    let state = handle.current();
    note_outcome(&state);
    Some(state)
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

        // The same process: nothing restarted it.
        handle.set(RestartState::accepted(None, &OLD).restarting());
        std::fs::create_dir_all(handle.helper_dir()).unwrap();
        assert!(!reconcile(&handle, &OLD));
        assert_eq!(handle.current().outcome, Some(Outcome::NotStarted));
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

    #[tokio::test]
    async fn the_watcher_publishes_what_the_helper_writes_and_stamps_it() {
        let (dir, handle) = bench("watch");
        let handle = std::sync::Arc::new(handle);
        handle.set(RestartState::accepted(Some("r-3".into()), &OLD).restarting());
        let path = handle.path().to_path_buf();
        let watcher = tokio::spawn(watch_until_done(
            handle.clone(),
            NEW.instance.to_string(),
            NEW.boot,
        ));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let mut done = RestartState::accepted(Some("r-3".into()), &OLD)
            .restarting()
            .finished(Outcome::Ok, None);
        done.instance_after = Some(NEW.instance.to_string());
        done.attempts = 1;
        save(&path, &done).unwrap();
        let ended = tokio::time::timeout(std::time::Duration::from_secs(5), watcher)
            .await
            .expect("the watcher returns on done")
            .unwrap()
            .expect("the record as it ended");
        assert_eq!(ended.outcome, Some(Outcome::Ok));
        assert_eq!(ended.boot_after, Some(NEW.boot));
        assert_eq!(handle.current(), ended);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_instance_is_opaque_and_new_each_time() {
        let (a, b) = (new_instance(), new_instance());
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
