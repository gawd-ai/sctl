//! Shared application state passed to every handler via Axum's `State` extractor.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64};
use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;
use tokio::sync::{broadcast, Mutex};

use crate::activity::{ActivityLog, ExecResultsCache};
use crate::config::Config;
use crate::gawdxfer::manager::TransferManager;
use crate::gps::GpsState;
use crate::lte::LteState;
use crate::sessions::SessionManager;

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
}

/// Tunnel connection event types.
#[derive(Clone, Debug)]
pub enum TunnelEventType {
    Connected,
    Disconnected,
    PongTimeout,
    WriterFailed,
    ReconnectAttempt,
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
        }
    }
}

/// A tunnel lifecycle event for observability.
#[derive(Clone, Debug)]
pub struct ConnectionEvent {
    pub timestamp: Instant,
    pub event_type: TunnelEventType,
    pub detail: String,
}

/// Maximum number of recent events to retain.
const MAX_TUNNEL_EVENTS: usize = 50;

/// Maximum number of pong RTT samples to keep for quality tracking.
const MAX_RTT_SAMPLES: usize = 20;

/// Tunnel connection statistics — atomics for lock-free hot-path updates,
/// Mutex only for event log and RTT samples (cold path).
pub struct TunnelStats {
    pub connected: AtomicBool,
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
}

impl TunnelStats {
    #[must_use]
    pub fn new() -> Self {
        Self {
            connected: AtomicBool::new(false),
            reconnects: AtomicU64::new(0),
            messages_sent: AtomicU64::new(0),
            messages_received: AtomicU64::new(0),
            last_pong_age_ms: AtomicU64::new(0),
            current_uptime_ms: AtomicU64::new(0),
            dropped_outbound: AtomicU64::new(0),
            epoch: Instant::now(),
            events: Mutex::new(VecDeque::with_capacity(MAX_TUNNEL_EVENTS)),
            rtt_samples: Mutex::new(VecDeque::with_capacity(MAX_RTT_SAMPLES)),
        }
    }

    /// Push a connection event, evicting oldest if at capacity.
    pub async fn push_event(&self, event_type: TunnelEventType, detail: String) {
        let mut events = self.events.lock().await;
        if events.len() >= MAX_TUNNEL_EVENTS {
            events.pop_front();
        }
        events.push_back(ConnectionEvent {
            timestamp: Instant::now(),
            event_type,
            detail,
        });
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
}

impl Default for TunnelStats {
    fn default() -> Self {
        Self::new()
    }
}
