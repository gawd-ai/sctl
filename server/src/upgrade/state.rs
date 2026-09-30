//! The one upgrade state: written by the running agent while it stages, by
//! the helper while it applies, read by the agent at start, pushed on the
//! tunnel as `upgrade.state`. Lives at `<state_dir>/upgrade/state.json`, on
//! persistent storage.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::Reason;

/// The message version, `v`.
pub const VERSION: u32 = 1;
/// The helper's log tail carried in the state.
pub const LOG_TAIL_BYTES: usize = 1024;
/// The largest `upgrade.state` message a relay accepts.
pub const MAX_BYTES: usize = 8 * 1024;

/// Where an upgrade is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Idle,
    Staging,
    Applying,
    Done,
}

/// How a finished upgrade ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The new version answered health twice with the tunnel up.
    Ok,
    /// Refused or failed before anything moved; `reason` says why.
    NotApplied,
    /// The new version did not pass health; the previous files are back.
    RolledBack,
    /// The restore did not come back healthy either.
    NeedsHands,
}

/// The state, as persisted and as pushed (with `type` and
/// `running_version` added by the sender).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradeState {
    pub v: u32,
    /// The clock when this state was written.
    pub ts: String,
    #[serde(default)]
    pub request_id: Option<String>,
    pub phase: Phase,
    #[serde(default)]
    pub outcome: Option<Outcome>,
    /// The version that accepted the request.
    #[serde(default)]
    pub from_version: Option<String>,
    /// The version asked for.
    #[serde(default)]
    pub to_version: Option<String>,
    #[serde(default)]
    pub reason: Option<Reason>,
    /// One line for a person, beside `reason`.
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub ended_at: Option<String>,
    /// The last [`LOG_TAIL_BYTES`] of the helper's log.
    #[serde(default)]
    pub log_tail: Option<String>,
}

impl UpgradeState {
    /// Nothing in flight, nothing to report.
    pub fn idle() -> Self {
        Self {
            v: VERSION,
            ts: crate::infra::now_iso(),
            request_id: None,
            phase: Phase::Idle,
            outcome: None,
            from_version: None,
            to_version: None,
            reason: None,
            detail: None,
            started_at: None,
            ended_at: None,
            log_tail: None,
        }
    }

    /// A request accepted: staging begins.
    pub fn staging(request_id: Option<String>, from: &str, to: &str) -> Self {
        let now = crate::infra::now_iso();
        Self {
            v: VERSION,
            ts: now.clone(),
            request_id,
            phase: Phase::Staging,
            outcome: None,
            from_version: Some(from.to_string()),
            to_version: Some(to.to_string()),
            reason: None,
            detail: None,
            started_at: Some(now),
            ended_at: None,
            log_tail: None,
        }
    }

    /// Whether the helper is, or was, in charge.
    pub fn in_flight(&self) -> bool {
        matches!(self.phase, Phase::Staging | Phase::Applying)
    }

    /// Move to `phase`, stamping the clock.
    #[must_use]
    pub fn with_phase(mut self, phase: Phase) -> Self {
        self.phase = phase;
        self.ts = crate::infra::now_iso();
        self
    }

    /// Finish with `outcome`.
    #[must_use]
    pub fn finished(mut self, outcome: Outcome) -> Self {
        let now = crate::infra::now_iso();
        self.phase = Phase::Done;
        self.outcome = Some(outcome);
        self.ts.clone_from(&now);
        self.ended_at = Some(now);
        self
    }

    /// Finish as `not_applied` for `reason`.
    #[must_use]
    pub fn not_applied(mut self, reason: Reason, detail: impl Into<String>) -> Self {
        self.reason = Some(reason);
        self.detail = Some(detail.into());
        self.finished(Outcome::NotApplied)
    }

    /// Attach the helper's log tail.
    #[must_use]
    pub fn with_log_tail(mut self, tail: Option<String>) -> Self {
        self.log_tail = tail;
        self
    }
}

/// The running agent's view of the state: the file, and a watch channel
/// the tunnel's `upgrade.state` forwarder and `/api/health` read.
pub struct Handle {
    path: PathBuf,
    tx: tokio::sync::watch::Sender<UpgradeState>,
    /// Taken by `POST /api/upgrade` for the length of a staging, so two
    /// requests cannot stage at once.
    pub in_flight: tokio::sync::Mutex<()>,
}

impl Handle {
    /// Read the file (or start idle) and open the channel.
    pub fn open(state_dir: &str) -> Self {
        let path = state_path(state_dir);
        let state = load(&path).unwrap_or_else(UpgradeState::idle);
        let (tx, _) = tokio::sync::watch::channel(state);
        Self {
            path,
            tx,
            in_flight: tokio::sync::Mutex::new(()),
        }
    }

    /// Where the file is.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The current state.
    pub fn current(&self) -> UpgradeState {
        self.tx.borrow().clone()
    }

    /// A receiver that wakes on every change.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<UpgradeState> {
        self.tx.subscribe()
    }

    /// Persist and publish `state`. A failed write is logged; the channel
    /// still moves so the tunnel tells what the agent knows.
    pub fn set(&self, state: UpgradeState) {
        if let Err(e) = save(&self.path, &state) {
            tracing::warn!("upgrade: could not write {}: {e}", self.path.display());
        }
        self.tx.send_replace(state);
    }

    /// Re-read the file (the helper wrote it) and publish what it says.
    pub fn reload(&self) -> Option<UpgradeState> {
        let state = load(&self.path)?;
        if *self.tx.borrow() != state {
            self.tx.send_replace(state.clone());
        }
        Some(state)
    }
}

/// `<state_dir>/upgrade/`: the state file, the stage and the lock.
pub fn upgrade_dir(state_dir: &str) -> PathBuf {
    Path::new(state_dir).join("upgrade")
}

/// The state file's path.
pub fn state_path(state_dir: &str) -> PathBuf {
    upgrade_dir(state_dir).join("state.json")
}

/// Read the state file; `None` when absent or unreadable.
pub fn load(path: &Path) -> Option<UpgradeState> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Write the state file atomically (write beside, rename over).
pub fn save(path: &Path, state: &UpgradeState) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(state).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// The last [`LOG_TAIL_BYTES`] of `log`, at a character boundary.
pub fn log_tail(log: &Path) -> Option<String> {
    let bytes = std::fs::read(log).ok()?;
    let start = bytes.len().saturating_sub(LOG_TAIL_BYTES);
    let mut start = start;
    while start < bytes.len() && (bytes[start] & 0xC0) == 0x80 {
        start += 1;
    }
    Some(String::from_utf8_lossy(&bytes[start..]).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("sctl-upgrade-state-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_state_round_trips_through_the_file() {
        let dir = tmp("rt");
        let path = state_path(dir.to_str().unwrap());
        assert_eq!(load(&path), None);
        let state = UpgradeState::staging(Some("r-1".into()), "0.6.7.1", "0.6.8.2")
            .with_phase(Phase::Applying);
        save(&path, &state).unwrap();
        assert_eq!(load(&path), Some(state.clone()));
        assert!(state.in_flight());
        let done = state.finished(Outcome::Ok);
        assert!(!done.in_flight());
        assert!(done.ended_at.is_some());
        save(&path, &done).unwrap();
        assert_eq!(load(&path).unwrap().outcome, Some(Outcome::Ok));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn not_applied_carries_the_reason() {
        let s = UpgradeState::staging(None, "0.6.7.1", "0.6.8.2")
            .not_applied(Reason::Sha256Mismatch, "sctl-x86_64");
        assert_eq!(s.phase, Phase::Done);
        assert_eq!(s.outcome, Some(Outcome::NotApplied));
        assert_eq!(s.reason, Some(Reason::Sha256Mismatch));
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["reason"], "sha256_mismatch");
        assert_eq!(json["outcome"], "not_applied");
    }

    #[test]
    fn the_log_tail_is_the_last_kib_on_a_char_boundary() {
        let dir = tmp("log");
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("log");
        let mut text = "x".repeat(2000);
        text.push('é');
        text.push_str(&"y".repeat(10));
        std::fs::write(&log, &text).unwrap();
        let tail = log_tail(&log).unwrap();
        assert!(tail.len() <= LOG_TAIL_BYTES);
        assert!(tail.ends_with(&"y".repeat(10)));
        assert!(tail.contains('é'));
        assert_eq!(log_tail(&dir.join("missing")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
