//! In-memory activity journal with real-time broadcast.
//!
//! Tracks REST and WebSocket operations (exec, file I/O, session lifecycle) in a
//! fixed-size ring buffer and broadcasts each new entry to all connected WebSocket
//! clients via the existing `session_events` channel.
//!
//! ## Design
//!
//! - **Ring buffer**: `VecDeque<ActivityEntry>` capped at `max_entries` (default 200).
//!   Old entries are silently dropped when the buffer is full.
//! - **Monotonic IDs**: Each entry gets a unique, always-increasing `id` so clients
//!   can request "everything since ID N" without gaps.
//! - **Zero-copy broadcast**: `log()` serializes the entry once and sends it through
//!   the existing `broadcast::Sender<Value>` — the WS event loop already forwards
//!   all broadcast messages to connected clients.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::Ordering;

use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{broadcast, RwLock};

use crate::atomic::AtomicU64;

/// Types of activities tracked by the journal.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, optional_fields))]
#[serde(rename_all = "snake_case")]
pub enum ActivityType {
    Exec,
    FileRead,
    FileWrite,
    FileList,
    SessionStart,
    SessionExec,
    SessionKill,
    SessionSignal,
    FileDelete,
    PlaybookList,
    PlaybookRead,
    PlaybookWrite,
    PlaybookDelete,
    WsConnect,
    WsDisconnect,
    TunnelConnect,
    TunnelDisconnect,
    TransferStart,
    TransferComplete,
}

/// Where the request originated.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, optional_fields))]
#[serde(rename_all = "snake_case")]
pub enum ActivitySource {
    Mcp,
    Ws,
    Rest,
    Tunnel,
    Unknown,
}

/// A single activity journal entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, optional_fields))]
pub struct ActivityEntry {
    pub id: u64,
    pub timestamp: u64,
    pub activity_type: ActivityType,
    pub source: ActivitySource,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(type = "unknown"))]
    pub detail: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

impl ActivityType {
    /// Parse from the serde rename value (e.g. `"exec"`, `"file_read"`).
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "exec" => Some(Self::Exec),
            "file_read" => Some(Self::FileRead),
            "file_write" => Some(Self::FileWrite),
            "file_list" => Some(Self::FileList),
            "file_delete" => Some(Self::FileDelete),
            "session_start" => Some(Self::SessionStart),
            "session_exec" => Some(Self::SessionExec),
            "session_kill" => Some(Self::SessionKill),
            "session_signal" => Some(Self::SessionSignal),
            "playbook_list" => Some(Self::PlaybookList),
            "playbook_read" => Some(Self::PlaybookRead),
            "playbook_write" => Some(Self::PlaybookWrite),
            "playbook_delete" => Some(Self::PlaybookDelete),
            "ws_connect" => Some(Self::WsConnect),
            "ws_disconnect" => Some(Self::WsDisconnect),
            "tunnel_connect" => Some(Self::TunnelConnect),
            "tunnel_disconnect" => Some(Self::TunnelDisconnect),
            "transfer_start" => Some(Self::TransferStart),
            "transfer_complete" => Some(Self::TransferComplete),
            _ => None,
        }
    }
}

impl ActivitySource {
    /// Parse from the serde rename value (e.g. `"mcp"`, `"ws"`, `"rest"`).
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "mcp" => Some(Self::Mcp),
            "ws" => Some(Self::Ws),
            "rest" => Some(Self::Rest),
            "tunnel" => Some(Self::Tunnel),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

/// The ring: its entries, oldest first, and which of them are refused
/// reads.
struct Ring {
    entries: VecDeque<ActivityEntry>,
    /// The ids of the refused-read entries still in the ring, oldest first.
    refused: VecDeque<u64>,
}

/// In-memory ring buffer of activity entries with broadcast support.
pub struct ActivityLog {
    entries: RwLock<Ring>,
    next_id: AtomicU64,
    max_entries: usize,
    broadcast_tx: broadcast::Sender<Value>,
}

impl ActivityLog {
    /// Create a new activity log that broadcasts via the given channel.
    pub fn new(max_entries: usize, broadcast_tx: broadcast::Sender<Value>) -> Self {
        Self {
            entries: RwLock::new(Ring {
                entries: VecDeque::with_capacity(max_entries),
                refused: VecDeque::new(),
            }),
            next_id: AtomicU64::new(1),
            max_entries,
            broadcast_tx,
        }
    }

    /// How many refused reads the ring keeps: a quarter of it. A refused
    /// read is one entry (ADR-011 decision 7), and refusals cost a caller
    /// nothing, so a flood of them must not push everything else out: 300
    /// refused reads once evicted all 200 entries, exec history included.
    /// Past this share, a new refusal takes the place of the oldest
    /// refusal, not of the oldest entry.
    pub fn max_refused(&self) -> usize {
        (self.max_entries / 4).max(1)
    }

    /// Append an entry, broadcast it, and return the assigned ID.
    pub async fn log(
        &self,
        activity_type: ActivityType,
        source: ActivitySource,
        summary: String,
        detail: Option<Value>,
        request_id: Option<String>,
    ) -> u64 {
        self.append(activity_type, source, summary, detail, request_id, false)
            .await
    }

    /// Append a refused read ([`crate::file_access::record`]): an entry
    /// like any other to a reader, kept within [`Self::max_refused`].
    pub async fn log_refusal(
        &self,
        activity_type: ActivityType,
        source: ActivitySource,
        summary: String,
        detail: Option<Value>,
        request_id: Option<String>,
    ) -> u64 {
        self.append(activity_type, source, summary, detail, request_id, true)
            .await
    }

    async fn append(
        &self,
        activity_type: ActivityType,
        source: ActivitySource,
        summary: String,
        detail: Option<Value>,
        request_id: Option<String>,
        refusal: bool,
    ) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let entry = ActivityEntry {
            id,
            timestamp,
            activity_type,
            source,
            summary,
            detail,
            request_id,
        };

        // Broadcast before acquiring the write lock (non-blocking for readers)
        let _ = self.broadcast_tx.send(
            crate::ws::messages::WsServerMsg::ActivityNew {
                entry: entry.clone(),
            }
            .to_value(),
        );

        let mut ring = self.entries.write().await;
        if refusal {
            // The oldest refusal makes room for this one: ids only grow, so
            // the entries are in id order.
            if ring.refused.len() >= self.max_refused() {
                if let Some(oldest) = ring.refused.pop_front() {
                    if let Ok(at) = ring.entries.binary_search_by_key(&oldest, |e| e.id) {
                        ring.entries.remove(at);
                    }
                }
            }
            ring.refused.push_back(id);
        }
        if ring.entries.len() >= self.max_entries {
            if let Some(dropped) = ring.entries.pop_front() {
                if ring.refused.front() == Some(&dropped.id) {
                    ring.refused.pop_front();
                }
            }
        }
        ring.entries.push_back(entry);

        id
    }

    /// Read entries with `id > since_id`, up to `limit`.
    pub async fn read_since(&self, since_id: u64, limit: usize) -> Vec<ActivityEntry> {
        let ring = self.entries.read().await;
        ring.entries
            .iter()
            .filter(|e| e.id > since_id)
            .take(limit)
            .cloned()
            .collect()
    }

    /// Read entries with optional filters (AND logic). All filters are optional.
    pub async fn read_since_filtered(
        &self,
        since_id: u64,
        limit: usize,
        activity_type: Option<ActivityType>,
        source: Option<ActivitySource>,
        session_id: Option<&str>,
    ) -> Vec<ActivityEntry> {
        let ring = self.entries.read().await;
        ring.entries
            .iter()
            .filter(|e| e.id > since_id)
            .filter(|e| activity_type.is_none_or(|t| e.activity_type == t))
            .filter(|e| source.is_none_or(|s| e.source == s))
            .filter(|e| {
                session_id.is_none_or(|sid| {
                    e.detail
                        .as_ref()
                        .and_then(|d| d["session_id"].as_str())
                        .is_some_and(|s| s == sid)
                })
            })
            .take(limit)
            .cloned()
            .collect()
    }
}

/// Determine the [`ActivitySource`] from HTTP request headers.
///
/// Checks the `X-Sctl-Client` header — `"mcp"` maps to [`ActivitySource::Mcp`],
/// anything else defaults to [`ActivitySource::Rest`].
pub fn source_from_headers(headers: &HeaderMap) -> ActivitySource {
    match headers.get("x-sctl-client").and_then(|v| v.to_str().ok()) {
        Some("mcp") => ActivitySource::Mcp,
        Some("tunnel") => ActivitySource::Tunnel,
        _ => ActivitySource::Rest,
    }
}

/// Extract `X-Request-Id` header value for correlation.
pub fn request_id_from_headers(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string)
}

// ---------------------------------------------------------------------------
// Exec results cache
// ---------------------------------------------------------------------------

/// Full exec result cached in memory, keyed by activity ID.
#[derive(Debug, Clone, Serialize)]
pub struct CachedExecResult {
    pub activity_id: u64,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    pub command: String,
    /// `"ok"`, `"timeout"`, or `"error"`.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

/// FIFO cache of recent exec results, keyed by activity ID.
pub struct ExecResultsCache {
    inner: RwLock<ExecResultsCacheInner>,
    max_entries: usize,
}

struct ExecResultsCacheInner {
    order: VecDeque<u64>,
    map: HashMap<u64, CachedExecResult>,
}

impl ExecResultsCache {
    pub fn new(max_entries: usize) -> Self {
        Self {
            inner: RwLock::new(ExecResultsCacheInner {
                order: VecDeque::with_capacity(max_entries),
                map: HashMap::with_capacity(max_entries),
            }),
            max_entries,
        }
    }

    /// Store a result, evicting the oldest entry if at capacity.
    pub async fn store(&self, result: CachedExecResult) {
        let mut inner = self.inner.write().await;
        if inner.order.len() >= self.max_entries {
            if let Some(old_id) = inner.order.pop_front() {
                inner.map.remove(&old_id);
            }
        }
        inner.order.push_back(result.activity_id);
        inner.map.insert(result.activity_id, result);
    }

    /// Retrieve a cached result by activity ID, if it hasn't been evicted.
    pub async fn get(&self, activity_id: u64) -> Option<CachedExecResult> {
        let inner = self.inner.read().await;
        inner.map.get(&activity_id).cloned()
    }
}

/// Truncate a string to `max` chars, appending "..." if truncated.
///
/// Collapses newlines and extra whitespace into single spaces for clean display.
pub fn truncate_str(s: &str, max: usize) -> String {
    // Collapse whitespace/newlines for clean one-line display
    let cleaned: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let char_count = cleaned.chars().count();
    if char_count <= max {
        cleaned
    } else {
        let mut result = cleaned
            .chars()
            .take(max.saturating_sub(3))
            .collect::<String>();
        result.push_str("...");
        result
    }
}

#[cfg(test)]
mod ring_tests {
    use super::*;

    fn log_of(max_entries: usize) -> ActivityLog {
        let (tx, _) = broadcast::channel(16);
        ActivityLog::new(max_entries, tx)
    }

    async fn exec(log: &ActivityLog, n: usize) {
        log.log(
            ActivityType::Exec,
            ActivitySource::Rest,
            format!("exec {n}"),
            None,
            None,
        )
        .await;
    }

    async fn refused(log: &ActivityLog, n: usize) -> u64 {
        log.log_refusal(
            ActivityType::FileRead,
            ActivitySource::Rest,
            format!("read refused: /tmp/fifo-{n}"),
            None,
            None,
        )
        .await
    }

    /// A flood of refused reads costs a caller nothing, and once evicted
    /// the whole ring, exec history included. Refusals hold at most a
    /// quarter of it: past that share a new refusal takes the oldest
    /// refusal's place.
    #[tokio::test]
    async fn a_flood_of_refused_reads_does_not_evict_the_rest_of_the_ring() {
        let log = log_of(200);
        assert_eq!(log.max_refused(), 50);
        for n in 0..200 {
            exec(&log, n).await;
        }
        let mut last = 0;
        for n in 0..300 {
            last = refused(&log, n).await;
        }
        let entries = log.read_since(0, 1000).await;
        assert_eq!(entries.len(), 200, "the ring is full, and no fuller");
        let execs: Vec<_> = entries
            .iter()
            .filter(|e| e.activity_type == ActivityType::Exec)
            .collect();
        let refusals: Vec<_> = entries
            .iter()
            .filter(|e| e.activity_type == ActivityType::FileRead)
            .collect();
        assert_eq!(execs.len(), 150, "the flood took a quarter, no more");
        assert_eq!(refusals.len(), 50);
        // The newest of each are what is kept.
        assert_eq!(execs.first().unwrap().summary, "exec 50");
        assert_eq!(execs.last().unwrap().summary, "exec 199");
        assert_eq!(
            refusals.first().unwrap().summary,
            "read refused: /tmp/fifo-250"
        );
        assert_eq!(refusals.last().unwrap().id, last);
        // Ids still only grow along the ring, so `since` reads as before.
        assert!(entries.windows(2).all(|w| w[0].id < w[1].id));
        assert_eq!(log.read_since(last - 1, 10).await.len(), 1);
    }

    /// Other entries still push refusals out in their turn: the share is a
    /// ceiling, not a reservation.
    #[tokio::test]
    async fn refused_reads_age_out_like_any_entry() {
        let log = log_of(8);
        assert_eq!(log.max_refused(), 2);
        for n in 0..3 {
            refused(&log, n).await;
        }
        let kept = log.read_since(0, 100).await;
        assert_eq!(kept.len(), 2, "two refusals at most");
        assert_eq!(kept[0].summary, "read refused: /tmp/fifo-1");
        for n in 0..8 {
            exec(&log, n).await;
        }
        let kept = log.read_since(0, 100).await;
        assert_eq!(kept.len(), 8);
        assert!(kept.iter().all(|e| e.activity_type == ActivityType::Exec));
        // The share starts afresh once they are gone.
        refused(&log, 9).await;
        refused(&log, 10).await;
        let kept = log.read_since(0, 100).await;
        assert_eq!(
            kept.iter()
                .filter(|e| e.activity_type == ActivityType::FileRead)
                .count(),
            2
        );
        assert_eq!(kept.len(), 8);
    }
}
