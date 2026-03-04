//! Shared application state passed to every handler via Axum's `State` extractor.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{broadcast, Mutex, Notify};
use tracing::warn;

use crate::activity::{ActivityLog, ExecResultsCache};
use crate::config::Config;
use crate::gawdxfer::manager::TransferManager;
use crate::gps::GpsState;
use crate::lte::LteState;
use crate::modem::Modem;
use crate::sessions::SessionManager;
use crate::tunnel::relay::{DeviceSnapshot, RelayConnectionHistory};

/// Shared application state for the sctl server.
#[derive(Clone)]
pub struct AppState {
    /// Immutable configuration loaded at startup.
    pub config: Arc<Config>,
    /// Monotonic instant when the server started (for uptime calculation).
    pub start_time: Instant,
    /// Manages the pool of interactive WebSocket shell sessions.
    pub session_manager: SessionManager,
    /// Broadcast channel for session lifecycle events (created/destroyed/renamed).
    /// All connected WebSocket clients subscribe to receive real-time updates.
    pub session_events: broadcast::Sender<Value>,
    /// In-memory activity journal for REST/WS operation tracking.
    pub activity_log: Arc<ActivityLog>,
    /// In-memory cache of full exec results, keyed by activity ID.
    pub exec_results_cache: Arc<ExecResultsCache>,
    /// Tunnel connection stats and event history.
    pub tunnel_stats: Arc<TunnelStats>,
    /// Chunked file transfer manager (gawdxfer).
    pub transfer_manager: Arc<TransferManager>,
    /// Current number of SSE connections (for connection limiting).
    pub sse_connections: Arc<AtomicU32>,
    /// GPS state (None when `[gps]` not configured).
    pub gps_state: Option<Arc<Mutex<GpsState>>>,
    /// LTE signal state (None when `[lte]` not configured).
    pub lte_state: Option<Arc<Mutex<LteState>>>,
    /// LTE modem handle (None when `[lte]` not configured).
    pub modem: Option<Modem>,
    /// Notify to trigger an on-demand LTE signal poll (None when `[lte]` not configured).
    pub lte_poll_notify: Option<Arc<Notify>>,
    /// Relay connection history (None when not in relay mode).
    pub relay_history: Option<Arc<RelayConnectionHistory>>,
    /// Device snapshots (relay mode only) — last-known telemetry for offline devices.
    pub device_snapshots: Option<Arc<tokio::sync::RwLock<HashMap<String, DeviceSnapshot>>>>,
}

/// Tunnel connection event types.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum TunnelEventType {
    Connected,
    Disconnected,
    PongTimeout,
    WriterFailed,
    ReconnectAttempt,
    WatchdogAction,
}

impl TunnelEventType {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Disconnected => "disconnected",
            Self::PongTimeout => "pong_timeout",
            Self::WriterFailed => "writer_failed",
            Self::ReconnectAttempt => "reconnect_attempt",
            Self::WatchdogAction => "watchdog_action",
        }
    }
}

/// A tunnel lifecycle event for observability.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConnectionEvent {
    /// Unix timestamp (seconds since epoch).
    pub timestamp: u64,
    pub event_type: TunnelEventType,
    pub detail: String,
}

/// Maximum number of recent events to retain.
const MAX_TUNNEL_EVENTS: usize = 200;

/// Events older than this are pruned on load.
const EVENT_MAX_AGE_SECS: u64 = 48 * 3600;

/// Maximum number of pong RTT samples to keep for quality tracking.
const MAX_RTT_SAMPLES: usize = 20;

/// Tunnel connection statistics — atomics for lock-free hot-path updates,
/// Mutex only for event log and RTT samples (cold path).
pub struct TunnelStats {
    pub connected: AtomicBool,
    /// True while the tunnel client is actively attempting a connection (DNS/TCP/TLS/handshake).
    /// Used by watchdog to avoid disrupting in-progress reconnection attempts.
    pub reconnecting: AtomicBool,
    pub reconnects: AtomicU64,
    pub messages_sent: AtomicU64,
    pub messages_received: AtomicU64,
    pub last_pong_age_ms: AtomicU64,
    pub current_uptime_ms: AtomicU64,
    pub dropped_outbound: AtomicU64,
    /// Epoch for computing relative timestamps in events.
    pub epoch: Instant,
    pub events: Mutex<VecDeque<ConnectionEvent>>,
    /// Rolling window of pong RTT samples (ms).
    pub rtt_samples: Mutex<VecDeque<u64>>,
    /// Path to persist events on disk (None = persistence disabled).
    pub events_path: Option<PathBuf>,
    /// Dirty flag for debounced persistence.
    pub events_dirty: AtomicBool,
}

impl TunnelStats {
    #[must_use]
    pub fn new() -> Self {
        Self {
            connected: AtomicBool::new(false),
            reconnecting: AtomicBool::new(false),
            reconnects: AtomicU64::new(0),
            messages_sent: AtomicU64::new(0),
            messages_received: AtomicU64::new(0),
            last_pong_age_ms: AtomicU64::new(0),
            current_uptime_ms: AtomicU64::new(0),
            dropped_outbound: AtomicU64::new(0),
            epoch: Instant::now(),
            events: Mutex::new(VecDeque::with_capacity(MAX_TUNNEL_EVENTS)),
            rtt_samples: Mutex::new(VecDeque::with_capacity(MAX_RTT_SAMPLES)),
            events_path: None,
            events_dirty: AtomicBool::new(false),
        }
    }

    /// Push a connection event, evicting oldest if at capacity.
    pub async fn push_event(&self, event_type: TunnelEventType, detail: String) {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut events = self.events.lock().await;
        if events.len() >= MAX_TUNNEL_EVENTS {
            events.pop_front();
        }
        events.push_back(ConnectionEvent {
            timestamp,
            event_type,
            detail,
        });
        drop(events);
        self.events_dirty.store(true, Ordering::Relaxed);
    }

    /// Record a pong RTT sample.
    pub async fn record_rtt(&self, rtt_ms: u64) {
        let mut samples = self.rtt_samples.lock().await;
        if samples.len() >= MAX_RTT_SAMPLES {
            samples.pop_front();
        }
        samples.push_back(rtt_ms);
    }

    /// Compute median and p95 RTT from samples. Returns (median, p95) or None if empty.
    pub async fn rtt_stats(&self) -> Option<(u64, u64)> {
        let samples = self.rtt_samples.lock().await;
        if samples.is_empty() {
            return None;
        }
        let mut sorted: Vec<u64> = samples.iter().copied().collect();
        sorted.sort_unstable();
        let median = sorted[sorted.len() / 2];
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let p95_idx = (sorted.len() as f64 * 0.95).ceil() as usize;
        let p95 = sorted[p95_idx.min(sorted.len() - 1)];
        Some((median, p95))
    }

    /// Load persisted events from disk, pruning entries older than 48h.
    pub fn load_events(path: &Path) -> VecDeque<ConnectionEvent> {
        let Ok(data) = std::fs::read_to_string(path) else {
            return VecDeque::new();
        };
        let events: Vec<ConnectionEvent> = match serde_json::from_str(&data) {
            Ok(v) => v,
            Err(e) => {
                warn!("Failed to parse tunnel_events.json: {e}");
                return VecDeque::new();
            }
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        events
            .into_iter()
            .filter(|e| now.saturating_sub(e.timestamp) < EVENT_MAX_AGE_SECS)
            .collect()
    }

    /// Persist events to disk if dirty (atomic write via tmp + rename).
    pub async fn save_events(&self) -> bool {
        if !self.events_dirty.swap(false, Ordering::Relaxed) {
            return false;
        }
        let Some(ref path) = self.events_path else {
            return false;
        };
        let events = self.events.lock().await;
        let snapshot: Vec<&ConnectionEvent> = events.iter().collect();
        let tmp = path.with_extension("json.tmp");
        let Ok(data) = serde_json::to_string_pretty(&snapshot) else {
            warn!("Failed to serialize tunnel events");
            return false;
        };
        if let Err(e) = std::fs::write(&tmp, &data) {
            warn!("Failed to write tunnel_events tmp file: {e}");
            return false;
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            warn!("Failed to rename tunnel_events file: {e}");
            return false;
        }
        true
    }
}

impl Default for TunnelStats {
    fn default() -> Self {
        Self::new()
    }
}
