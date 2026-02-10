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

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{broadcast, RwLock};

/// Types of activities tracked by the journal.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
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
}

/// Where the request originated.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActivitySource {
    Mcp,
    Ws,
    Rest,
    Unknown,
}

/// A single activity journal entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityEntry {
    pub id: u64,
    pub timestamp: u64,
    pub activity_type: ActivityType,
    pub source: ActivitySource,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
}

/// In-memory ring buffer of activity entries with broadcast support.
pub struct ActivityLog {
    entries: RwLock<VecDeque<ActivityEntry>>,
    next_id: AtomicU64,
    max_entries: usize,
    broadcast_tx: broadcast::Sender<Value>,
}

impl ActivityLog {
    /// Create a new activity log that broadcasts via the given channel.
    pub fn new(max_entries: usize, broadcast_tx: broadcast::Sender<Value>) -> Self {
        Self {
            entries: RwLock::new(VecDeque::with_capacity(max_entries)),
            next_id: AtomicU64::new(1),
            max_entries,
            broadcast_tx,
        }
    }

    /// Append an entry, broadcast it, and return the assigned ID.
    pub async fn log(
        &self,
        activity_type: ActivityType,
        source: ActivitySource,
        summary: String,
        detail: Option<Value>,
    ) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        #[allow(clippy::cast_possible_truncation)]
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
        };

        // Broadcast before acquiring the write lock (non-blocking for readers)
        let _ = self.broadcast_tx.send(json!({
            "type": "activity.new",
            "entry": &entry,
        }));

        let mut entries = self.entries.write().await;
        if entries.len() >= self.max_entries {
            entries.pop_front();
        }
        entries.push_back(entry);

        id
    }

    /// Read entries with `id > since_id`, up to `limit`.
    pub async fn read_since(&self, since_id: u64, limit: usize) -> Vec<ActivityEntry> {
        let entries = self.entries.read().await;
        entries
            .iter()
            .filter(|e| e.id > since_id)
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
        _ => ActivitySource::Rest,
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
