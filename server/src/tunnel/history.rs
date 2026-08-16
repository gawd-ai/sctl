//! Relay-side record of device connection sessions (connect → disconnect).
//!
//! The relay is the only observer of a device outage — a device cannot report
//! its own absence — so this module is the fleet's primary outage record. That
//! record has to satisfy three invariants the 0.5.x implementation broke:
//!
//! 1. **Sessions close by identity, not by recency.** Every session carries
//!    the `connection_id` that already fences the relay's device map. The old
//!    reverse-scan closed "the most recent open session for the serial", so a
//!    reconnect (new session pushed, old handler's cleanup firing after)
//!    closed the NEW session and orphaned the old one open forever — the
//!    overlapping-sessions/negative-gaps artifact measured in production.
//! 2. **One device cannot evict the fleet's history.** The old single
//!    100-entry ring meant one flapping vehicle consumed the entire record
//!    (~30h observed) and every other device's history vanished. Rings are
//!    per-serial now, with a per-serial cap and a serial-count bound.
//! 3. **History survives a relay restart without guessing.** The journald
//!    re-seed parsed log text and collapsed every disconnect reason to
//!    "disconnected". Events are now appended to a JSONL file in the relay's
//!    data dir as they happen, replayed on startup, and compacted on load.
//!    The file starts empty on first boot: fleet Postgres holds long-term
//!    history, this file only has to bridge restarts.

use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// Maximum sessions retained per serial.
const PER_SERIAL_CAP: usize = 50;
/// Maximum number of serials tracked. When exceeded, the serial whose latest
/// activity is oldest (and which has no open session) is evicted.
const SERIAL_CAP: usize = 512;

/// A recorded device connection session (connect → disconnect).
#[derive(Clone, Debug)]
pub struct ConnectionSession {
    pub serial: String,
    /// The relay's per-process connection counter — the same value that
    /// fences the device map. Sessions loaded from a previous process keep
    /// the id they were recorded with.
    pub connection_id: u64,
    pub connected_at: u64,
    pub disconnected_at: Option<u64>,
    pub reason: Option<String>,
    /// Age of last heartbeat at disconnect time (ms). Low = sudden death, high = gradual.
    pub last_heartbeat_age_ms: Option<u64>,
    /// Public address the tunnel arrived from. Comparing across sessions is
    /// what makes uplink path changes observable after the fact.
    pub egress_ip: Option<String>,
}

/// One line of the on-disk JSONL log. Append-only while running; the file is
/// replayed and compacted at startup. Unparseable lines (a torn tail after a
/// crash) are skipped, not fatal.
#[derive(Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum HistoryEvent {
    Connect {
        serial: String,
        connection_id: u64,
        ts: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        egress_ip: Option<String>,
    },
    Disconnect {
        serial: String,
        connection_id: u64,
        ts: u64,
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_heartbeat_age_ms: Option<u64>,
    },
}

struct Inner {
    rings: HashMap<String, VecDeque<ConnectionSession>>,
    /// Append handle for the JSONL log. `None` in memory-only mode (tests,
    /// or a relay with no data dir).
    log: Option<File>,
    /// Path of the JSONL log, kept for runtime compaction rewrites.
    path: Option<PathBuf>,
    /// Bytes appended since the last compaction. The rings bound what a
    /// rewrite retains, so file size is bounded by (compacted size +
    /// `COMPACT_APPEND_BYTES`) — without this, a flapping device grows the
    /// log for the whole relay uptime (18-day uptimes are normal) on a
    /// nearly-full disk.
    appended_bytes: u64,
}

/// Appended bytes that trigger an in-place compaction rewrite.
const COMPACT_APPEND_BYTES: u64 = 4 * 1024 * 1024;

/// Per-device rings of connection sessions, with optional JSONL persistence.
pub struct RelayConnectionHistory {
    inner: tokio::sync::Mutex<Inner>,
}

impl RelayConnectionHistory {
    /// Memory-only history (no persistence).
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: tokio::sync::Mutex::new(Inner {
                rings: HashMap::new(),
                log: None,
                path: None,
                appended_bytes: 0,
            }),
        }
    }

    /// History persisted as JSONL at `path`: existing events are replayed
    /// into the rings, the file is rewritten compacted to what the rings
    /// retain, and subsequent events append. Runs synchronously — call at
    /// startup, before serving.
    #[must_use]
    pub fn with_persistence(path: &PathBuf) -> Self {
        let mut rings: HashMap<String, VecDeque<ConnectionSession>> = HashMap::new();

        let mut replayed = 0usize;
        let mut skipped = 0usize;
        if let Ok(file) = File::open(path) {
            for line in BufReader::new(file).lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<HistoryEvent>(&line) {
                    Ok(event) => {
                        apply(&mut rings, &event);
                        replayed += 1;
                    }
                    Err(_) => skipped += 1,
                }
            }
        }
        if skipped > 0 {
            warn!("Connection history: skipped {skipped} unparseable JSONL line(s)");
        }

        // Compact: rewrite the file to exactly the events the rings retain.
        // The same rewrite runs again at runtime once appends since the last
        // compaction pass `COMPACT_APPEND_BYTES`.
        let log = match rewrite_compacted(path, &rings) {
            Ok(file) => Some(file),
            Err(e) => {
                warn!("Connection history: compaction write failed: {e}");
                None
            }
        };
        if replayed > 0 {
            info!(
                "Connection history: replayed {replayed} event(s) into {} serial ring(s)",
                rings.len()
            );
        }

        Self {
            inner: tokio::sync::Mutex::new(Inner {
                rings,
                log,
                path: Some(path.clone()),
                appended_bytes: 0,
            }),
        }
    }

    /// Record a new device connection. Any session still open for the serial
    /// is closed as "replaced" at the same timestamp — the structural
    /// guarantee that a serial never has two open sessions and gaps never go
    /// negative.
    pub async fn record_connect(&self, serial: &str, connection_id: u64, egress_ip: Option<&str>) {
        let event = HistoryEvent::Connect {
            serial: serial.to_string(),
            connection_id,
            ts: now_epoch_secs(),
            egress_ip: egress_ip.map(ToString::to_string),
        };
        let mut inner = self.inner.lock().await;
        apply(&mut inner.rings, &event);
        let appended = append(&mut inner.log, &event);
        inner.appended_bytes += appended;
        maybe_compact(&mut inner);
    }

    /// Record a device disconnection for exactly the session identified by
    /// `(serial, connection_id)`. A session already closed (e.g. as
    /// "replaced" by a successor's connect) is left untouched, so a stale
    /// handler's late cleanup can never close somebody else's session.
    pub async fn record_disconnect(
        &self,
        serial: &str,
        connection_id: u64,
        reason: &str,
        last_heartbeat_age_ms: Option<u64>,
    ) {
        let event = HistoryEvent::Disconnect {
            serial: serial.to_string(),
            connection_id,
            ts: now_epoch_secs(),
            reason: reason.to_string(),
            last_heartbeat_age_ms,
        };
        let mut inner = self.inner.lock().await;
        apply(&mut inner.rings, &event);
        let appended = append(&mut inner.log, &event);
        inner.appended_bytes += appended;
        maybe_compact(&mut inner);
    }

    /// Snapshot all sessions for the health endpoint, chronological by
    /// connect time across the whole fleet.
    pub async fn snapshot(&self) -> Vec<ConnectionSession> {
        let inner = self.inner.lock().await;
        let mut all: Vec<ConnectionSession> = inner
            .rings
            .values()
            .flat_map(|ring| ring.iter().cloned())
            .collect();
        all.sort_by_key(|s| (s.connected_at, s.connection_id));
        all
    }
}

impl Default for RelayConnectionHistory {
    fn default() -> Self {
        Self::new()
    }
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Apply one event to the rings. Shared verbatim by the live path and the
/// startup replay, so what the file reproduces is exactly what happened.
fn apply(rings: &mut HashMap<String, VecDeque<ConnectionSession>>, event: &HistoryEvent) {
    match event {
        HistoryEvent::Connect {
            serial,
            connection_id,
            ts,
            egress_ip,
        } => {
            let ring = rings.entry(serial.clone()).or_default();
            if let Some(open) = ring.iter_mut().rev().find(|s| s.disconnected_at.is_none()) {
                open.disconnected_at = Some(*ts);
                open.reason = Some("replaced".to_string());
            }
            if ring.len() >= PER_SERIAL_CAP {
                ring.pop_front();
            }
            ring.push_back(ConnectionSession {
                serial: serial.clone(),
                connection_id: *connection_id,
                connected_at: *ts,
                disconnected_at: None,
                reason: None,
                last_heartbeat_age_ms: None,
                egress_ip: egress_ip.clone(),
            });
            enforce_serial_cap(rings, serial);
        }
        HistoryEvent::Disconnect {
            serial,
            connection_id,
            ts,
            reason,
            last_heartbeat_age_ms,
        } => {
            if let Some(ring) = rings.get_mut(serial) {
                if let Some(session) = ring
                    .iter_mut()
                    .rev()
                    .find(|s| s.connection_id == *connection_id && s.disconnected_at.is_none())
                {
                    session.disconnected_at = Some(*ts);
                    session.reason = Some(reason.clone());
                    session.last_heartbeat_age_ms = *last_heartbeat_age_ms;
                }
            }
        }
    }
}

/// Drop the least-recently-active fully-closed serial when the serial count
/// exceeds the bound. `just_touched` is never evicted.
fn enforce_serial_cap(
    rings: &mut HashMap<String, VecDeque<ConnectionSession>>,
    just_touched: &str,
) {
    while rings.len() > SERIAL_CAP {
        let victim = rings
            .iter()
            .filter(|(serial, ring)| {
                serial.as_str() != just_touched && ring.iter().all(|s| s.disconnected_at.is_some())
            })
            .min_by_key(|(_, ring)| {
                ring.iter()
                    .map(|s| s.disconnected_at.unwrap_or(s.connected_at))
                    .max()
                    .unwrap_or(0)
            })
            .map(|(serial, _)| serial.clone());
        match victim {
            Some(serial) => {
                rings.remove(&serial);
            }
            // Every other serial has an open session — nothing sane to evict.
            None => break,
        }
    }
}

/// Rebuild a minimal event stream that replays to exactly the given rings.
/// Per-session pairs are emitted in connect order and stable-sorted by
/// timestamp, which keeps a session's connect ahead of its own disconnect
/// and a predecessor's replaced-close ahead of its successor's connect even
/// when they share a second.
fn reconstruct_events(rings: &HashMap<String, VecDeque<ConnectionSession>>) -> Vec<HistoryEvent> {
    let mut sessions: Vec<&ConnectionSession> =
        rings.values().flat_map(|ring| ring.iter()).collect();
    sessions.sort_by_key(|s| (s.connected_at, s.connection_id));

    let mut events: Vec<(u64, HistoryEvent)> = Vec::with_capacity(sessions.len() * 2);
    for s in sessions {
        events.push((
            s.connected_at,
            HistoryEvent::Connect {
                serial: s.serial.clone(),
                connection_id: s.connection_id,
                ts: s.connected_at,
                egress_ip: s.egress_ip.clone(),
            },
        ));
        if let Some(closed_at) = s.disconnected_at {
            events.push((
                closed_at,
                HistoryEvent::Disconnect {
                    serial: s.serial.clone(),
                    connection_id: s.connection_id,
                    ts: closed_at,
                    reason: s.reason.clone().unwrap_or_else(|| "disconnected".into()),
                    last_heartbeat_age_ms: s.last_heartbeat_age_ms,
                },
            ));
        }
    }
    events.sort_by_key(|(ts, _)| *ts);
    events.into_iter().map(|(_, e)| e).collect()
}

fn append(log: &mut Option<File>, event: &HistoryEvent) -> u64 {
    if let Some(file) = log {
        let line = serde_json::to_string(event).unwrap_or_default();
        if writeln!(file, "{line}").is_err() {
            warn!("Connection history: append failed, dropping persistence");
            *log = None;
            return 0;
        }
        return line.len() as u64 + 1;
    }
    0
}

/// Write the ring-retained events to `path` atomically (tmp + rename) and
/// return a fresh append handle. Used at load and for runtime compaction.
fn rewrite_compacted(
    path: &PathBuf,
    rings: &HashMap<String, VecDeque<ConnectionSession>>,
) -> std::io::Result<File> {
    let compacted = reconstruct_events(rings);
    let tmp = path.with_extension("jsonl.tmp");
    let mut f = File::create(&tmp)?;
    for event in &compacted {
        writeln!(f, "{}", serde_json::to_string(event).unwrap_or_default())?;
    }
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    OpenOptions::new().append(true).create(true).open(path)
}

/// After an append, compact in place once enough new bytes accumulated.
/// On failure the log falls back to memory-only — appending onward to a
/// file we can no longer garbage-collect would defeat the size bound.
fn maybe_compact(inner: &mut Inner) {
    if inner.log.is_none() || inner.appended_bytes < COMPACT_APPEND_BYTES {
        return;
    }
    let Some(path) = inner.path.clone() else {
        return;
    };
    match rewrite_compacted(&path, &inner.rings) {
        Ok(file) => {
            inner.log = Some(file);
            inner.appended_bytes = 0;
        }
        Err(e) => {
            warn!("Connection history: runtime compaction failed, dropping persistence: {e}");
            inner.log = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The production artifact this rewrite kills: on reconnect the new
    /// session registers first, the old handler's cleanup fires after — and
    /// the old reverse-scan closed the NEW session. Keyed by connection_id,
    /// the late cleanup is a no-op and exactly one session stays open.
    #[tokio::test]
    async fn late_replaced_cleanup_cannot_close_the_successor() {
        let h = RelayConnectionHistory::new();
        h.record_connect("bus01", 1, Some("1.1.1.1")).await;
        h.record_connect("bus01", 2, Some("2.2.2.2")).await;
        h.record_disconnect("bus01", 1, "replaced", None).await;

        let snap = h.snapshot().await;
        assert_eq!(snap.len(), 2);
        let open: Vec<_> = snap
            .iter()
            .filter(|s| s.disconnected_at.is_none())
            .collect();
        assert_eq!(open.len(), 1, "exactly one open session");
        assert_eq!(open[0].connection_id, 2);
        let closed = snap.iter().find(|s| s.connection_id == 1).unwrap();
        assert_eq!(closed.reason.as_deref(), Some("replaced"));
    }

    /// No serial ever has overlapping sessions, and no gap is negative, even
    /// through a flap storm with out-of-order cleanups.
    #[tokio::test]
    async fn no_overlap_and_no_negative_gap() {
        let h = RelayConnectionHistory::new();
        for cid in 1..=20u64 {
            h.record_connect("bus01", cid, None).await;
            if cid % 3 == 0 {
                // Simulate the old handler's cleanup arriving late.
                h.record_disconnect("bus01", cid - 1, "replaced", None)
                    .await;
            }
            if cid % 4 == 0 {
                h.record_disconnect("bus01", cid, "ws_close", None).await;
            }
        }
        let snap = h.snapshot().await;
        let bus: Vec<_> = snap.iter().filter(|s| s.serial == "bus01").collect();
        assert!(bus.iter().filter(|s| s.disconnected_at.is_none()).count() <= 1);
        for pair in bus.windows(2) {
            let closed = pair[0]
                .disconnected_at
                .expect("every non-final session must be closed");
            assert!(
                pair[1].connected_at >= closed,
                "negative gap: {} connects at {} before {} closed at {}",
                pair[1].connection_id,
                pair[1].connected_at,
                pair[0].connection_id,
                closed
            );
        }
    }

    /// One flapping device fills only its own ring; other serials' history
    /// survives untouched.
    #[tokio::test]
    async fn a_flapper_cannot_evict_the_fleet() {
        let h = RelayConnectionHistory::new();
        h.record_connect("quiet", 1, None).await;
        h.record_disconnect("quiet", 1, "ws_close", None).await;
        for cid in 2..=400u64 {
            h.record_connect("flapper", cid, None).await;
            h.record_disconnect("flapper", cid, "heartbeat_timeout", None)
                .await;
        }
        let snap = h.snapshot().await;
        assert_eq!(
            snap.iter().filter(|s| s.serial == "quiet").count(),
            1,
            "the quiet device's history must survive"
        );
        assert_eq!(
            snap.iter().filter(|s| s.serial == "flapper").count(),
            PER_SERIAL_CAP
        );
    }

    #[tokio::test]
    async fn jsonl_round_trips_and_tolerates_a_torn_tail() {
        let dir = std::env::temp_dir().join(format!("sctl-hist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("connection_history.jsonl");
        let _ = std::fs::remove_file(&path);

        {
            let h = RelayConnectionHistory::with_persistence(&path);
            h.record_connect("bus01", 1, Some("9.9.9.9")).await;
            h.record_disconnect("bus01", 1, "pong_timeout", Some(31_000))
                .await;
            h.record_connect("bus02", 2, None).await;
        }
        // Torn tail: a crash mid-append leaves a partial line.
        {
            use std::io::Write as _;
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            write!(f, "{{\"event\":\"connect\",\"serial\":\"bus").unwrap();
        }

        let h = RelayConnectionHistory::with_persistence(&path);
        let snap = h.snapshot().await;
        assert_eq!(snap.len(), 2);
        let s1 = snap.iter().find(|s| s.serial == "bus01").unwrap();
        assert_eq!(s1.reason.as_deref(), Some("pong_timeout"));
        assert_eq!(s1.last_heartbeat_age_ms, Some(31_000));
        assert_eq!(s1.egress_ip.as_deref(), Some("9.9.9.9"));
        let s2 = snap.iter().find(|s| s.serial == "bus02").unwrap();
        assert!(
            s2.disconnected_at.is_none(),
            "open session survives restart"
        );

        // The compaction rewrote the torn tail away.
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents
            .lines()
            .all(|l| l.trim().is_empty() || serde_json::from_str::<HistoryEvent>(l).is_ok()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Runtime compaction: hammering one flapping serial must not grow the
    /// file for the life of the process — once appends pass the threshold
    /// the log is rewritten to the ring-retained events.
    #[tokio::test]
    async fn runtime_compaction_bounds_the_file() {
        let dir = std::env::temp_dir().join(format!("sctl-hist-compact-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("connection_history.jsonl");
        let _ = std::fs::remove_file(&path);

        let h = RelayConnectionHistory::with_persistence(&path);
        // Each round appends ~250 bytes; push well past the threshold so the
        // rewrite provably fires at least once.
        let rounds = COMPACT_APPEND_BYTES / 100;
        for i in 0..rounds {
            h.record_connect("flapper", i, Some("1.2.3.4")).await;
            h.record_disconnect("flapper", i, "pong_timeout", Some(31_000))
                .await;
        }
        let size = std::fs::metadata(&path).unwrap().len();
        assert!(
            size < COMPACT_APPEND_BYTES + 64 * 1024,
            "file stayed bounded, got {size}"
        );
        // The compacted file still replays into a valid ring.
        let h2 = RelayConnectionHistory::with_persistence(&path);
        let snap = h2.snapshot().await;
        assert!(!snap.is_empty() && snap.len() <= PER_SERIAL_CAP);
    }

    /// Driven through `apply` directly so each serial gets a distinct
    /// timestamp — the eviction policy is "least recent activity", which a
    /// wall-clock test cannot pin down inside one second.
    #[test]
    fn serial_cap_evicts_the_stalest_closed_serial() {
        let mut rings = HashMap::new();
        for i in 0..=SERIAL_CAP as u64 {
            let serial = format!("dev{i:04}");
            let ts = 1_000 + i * 10;
            apply(
                &mut rings,
                &HistoryEvent::Connect {
                    serial: serial.clone(),
                    connection_id: i + 1,
                    ts,
                    egress_ip: None,
                },
            );
            apply(
                &mut rings,
                &HistoryEvent::Disconnect {
                    serial,
                    connection_id: i + 1,
                    ts: ts + 5,
                    reason: "ws_close".to_string(),
                    last_heartbeat_age_ms: None,
                },
            );
        }
        assert_eq!(rings.len(), SERIAL_CAP);
        assert!(
            !rings.contains_key("dev0000"),
            "the stalest closed serial is the one evicted"
        );
        // An OPEN session shields its serial from eviction.
        let mut rings = HashMap::new();
        apply(
            &mut rings,
            &HistoryEvent::Connect {
                serial: "immortal".to_string(),
                connection_id: 1,
                ts: 1,
                egress_ip: None,
            },
        );
        for i in 0..=SERIAL_CAP as u64 {
            let ts = 1_000 + i * 10;
            apply(
                &mut rings,
                &HistoryEvent::Connect {
                    serial: format!("dev{i:04}"),
                    connection_id: i + 2,
                    ts,
                    egress_ip: None,
                },
            );
            apply(
                &mut rings,
                &HistoryEvent::Disconnect {
                    serial: format!("dev{i:04}"),
                    connection_id: i + 2,
                    ts: ts + 5,
                    reason: "ws_close".to_string(),
                    last_heartbeat_age_ms: None,
                },
            );
        }
        assert!(
            rings.contains_key("immortal"),
            "a serial with an open session must never be evicted"
        );
    }
}
