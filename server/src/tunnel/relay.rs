//! Tunnel relay — accepts device registrations and proxies client requests.
//!
//! When `tunnel.relay = true`, the relay:
//! 1. Listens for device WS connections at `/api/tunnel/register`
//! 2. Exposes REST + WS proxy at `/d/{serial}/api/*`
//! 3. Translates client requests to tunnel messages over the device WS

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::{
    body::Body,
    extract::{Path as AxumPath, Query, State, WebSocketUpgrade},
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, watch, Mutex, RwLock};
use tracing::{info, info_span, warn, Instrument};

use super::{decode_binary_frame, encode_binary_frame, TunnelMessage, TunnelResponse};

/// Maximum number of connection sessions to retain in history.
const MAX_CONNECTION_HISTORY: usize = 100;
/// Max time to wait to enqueue a request onto a device's tunnel queue.
const DEVICE_QUEUE_SEND_TIMEOUT_SECS: u64 = 5;

/// A recorded device connection session (connect → disconnect).
#[derive(Clone, Debug)]
pub struct ConnectionSession {
    pub serial: String,
    pub connected_at: u64,
    pub disconnected_at: Option<u64>,
    pub reason: Option<String>,
    /// Age of last heartbeat at disconnect time (ms). Low = sudden death, high = gradual.
    pub last_heartbeat_age_ms: Option<u64>,
}

/// Ring buffer of device connection sessions for the relay dashboard.
pub struct RelayConnectionHistory {
    sessions: tokio::sync::Mutex<VecDeque<ConnectionSession>>,
}

impl RelayConnectionHistory {
    #[must_use]
    pub fn new() -> Self {
        Self {
            sessions: tokio::sync::Mutex::new(VecDeque::with_capacity(MAX_CONNECTION_HISTORY)),
        }
    }

    /// Record a new device connection.
    pub async fn record_connect(&self, serial: &str) {
        let mut sessions = self.sessions.lock().await;
        if sessions.len() >= MAX_CONNECTION_HISTORY {
            sessions.pop_front();
        }
        #[allow(clippy::cast_possible_truncation)]
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        sessions.push_back(ConnectionSession {
            serial: serial.to_string(),
            connected_at: now,
            disconnected_at: None,
            reason: None,
            last_heartbeat_age_ms: None,
        });
    }

    /// Record a device disconnection. Updates the most recent open session for the serial.
    pub async fn record_disconnect(
        &self,
        serial: &str,
        reason: &str,
        last_heartbeat_age_ms: Option<u64>,
    ) {
        let mut sessions = self.sessions.lock().await;
        #[allow(clippy::cast_possible_truncation)]
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        // Find the most recent open session for this serial (reverse search)
        for session in sessions.iter_mut().rev() {
            if session.serial == serial && session.disconnected_at.is_none() {
                session.disconnected_at = Some(now);
                session.reason = Some(reason.to_string());
                session.last_heartbeat_age_ms = last_heartbeat_age_ms;
                return;
            }
        }
    }

    /// Snapshot all sessions for the health endpoint.
    pub async fn snapshot(&self) -> Vec<ConnectionSession> {
        self.sessions.lock().await.iter().cloned().collect()
    }

    /// Seed history from journald logs so connection data survives relay restarts.
    /// Parses the last 24h of logs from `_COMM=sctl` for connect/disconnect events.
    pub async fn seed_from_journal(&self) {
        let output = match tokio::process::Command::new("journalctl")
            .args([
                "_COMM=sctl",
                "--since",
                "24 hours ago",
                "--no-pager",
                "-o",
                "json",
                "--output-fields=MESSAGE,__REALTIME_TIMESTAMP",
            ])
            .output()
            .await
        {
            Ok(o) if o.status.success() => o,
            _ => return,
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut sessions: VecDeque<ConnectionSession> = VecDeque::new();
        // Track open session index per serial
        let mut open: HashMap<String, usize> = HashMap::new();

        for line in stdout.lines() {
            let Ok(entry) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let ts_secs = entry["__REALTIME_TIMESTAMP"]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0)
                / 1_000_000;
            // MESSAGE can be a string or a byte array (when tracing outputs ANSI colors)
            let msg_owned: String;
            let msg: &str = if let Some(s) = entry["MESSAGE"].as_str() {
                s
            } else if let Some(arr) = entry["MESSAGE"].as_array() {
                let bytes: Vec<u8> = arr
                    .iter()
                    .filter_map(|v| v.as_u64().map(|n| n as u8))
                    .collect();
                msg_owned = String::from_utf8_lossy(&bytes).into_owned();
                &msg_owned
            } else {
                continue;
            };

            if msg.contains("Device registered") {
                if let Some(serial) = extract_serial_from_log(msg) {
                    // Close any open session for this serial (replaced)
                    if let Some(idx) = open.remove(&serial) {
                        if let Some(s) = sessions.get_mut(idx) {
                            if s.disconnected_at.is_none() {
                                s.disconnected_at = Some(ts_secs);
                                s.reason = Some("replaced".to_string());
                            }
                        }
                    }
                    let idx = sessions.len();
                    sessions.push_back(ConnectionSession {
                        serial: serial.clone(),
                        connected_at: ts_secs,
                        disconnected_at: None,
                        reason: None,
                        last_heartbeat_age_ms: None,
                    });
                    open.insert(serial, idx);
                }
            } else if msg.contains("Evicted device (heartbeat timeout)") {
                if let Some(serial) = extract_serial_from_log(msg) {
                    if let Some(idx) = open.remove(&serial) {
                        if let Some(s) = sessions.get_mut(idx) {
                            s.disconnected_at = Some(ts_secs);
                            s.reason = Some("heartbeat_timeout".to_string());
                        }
                    }
                }
            } else if msg.contains("Evicted device (broadcast send failed)") {
                if let Some(serial) = extract_serial_from_log(msg) {
                    if let Some(idx) = open.remove(&serial) {
                        if let Some(s) = sessions.get_mut(idx) {
                            s.disconnected_at = Some(ts_secs);
                            s.reason = Some("send_failed".to_string());
                        }
                    }
                }
            } else if msg.contains("Device disconnected") {
                if let Some(serial) = extract_serial_from_log(msg) {
                    if let Some(idx) = open.remove(&serial) {
                        if let Some(s) = sessions.get_mut(idx) {
                            s.disconnected_at = Some(ts_secs);
                            s.reason = Some("disconnected".to_string());
                        }
                    }
                }
            } else if msg.contains("Shutting down") {
                for (_, idx) in open.drain() {
                    if let Some(s) = sessions.get_mut(idx) {
                        if s.disconnected_at.is_none() {
                            s.disconnected_at = Some(ts_secs);
                            s.reason = Some("relay_shutdown".to_string());
                        }
                    }
                }
            }
        }

        // Trim to capacity
        while sessions.len() > MAX_CONNECTION_HISTORY {
            sessions.pop_front();
        }

        let count = sessions.len();
        *self.sessions.lock().await = sessions;
        if count > 0 {
            info!("Seeded {count} connection sessions from journal");
        }
    }
}

/// Strip ANSI escape sequences from a string.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip until end of escape sequence (letter)
            for c2 in chars.by_ref() {
                if c2.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Extract `serial=VALUE` from a log message (handles ANSI escape codes).
fn extract_serial_from_log(msg: &str) -> Option<String> {
    let clean = strip_ansi(msg);
    // Find the last occurrence of "serial=" (the structured field, not the span)
    let idx = clean.rfind("serial=")?;
    let rest = &clean[idx + 7..];
    let end = rest
        .find(|c: char| c.is_whitespace() || c == ',')
        .unwrap_or(rest.len());
    let serial = &rest[..end];
    if serial.is_empty() {
        None
    } else {
        Some(serial.to_string())
    }
}

impl Default for RelayConnectionHistory {
    fn default() -> Self {
        Self::new()
    }
}

/// Snapshot of last-known device state, persisted across disconnects and relay restarts.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceSnapshot {
    pub serial: String,
    pub last_lte_signal: Option<Value>,
    pub last_gps_fix: Option<Value>,
    pub last_watchdog: Option<Value>,
    pub last_seen: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct LiveDeviceStatus {
    pub serial: String,
    pub connected: bool,
    pub connected_at: u64,
    pub connected_since_ms: u64,
    pub last_heartbeat_age_ms: u64,
    pub pending_requests_count: usize,
    pub session_subscription_count: usize,
    pub subscribed_client_count: usize,
    pub client_count: usize,
    pub dropped_messages: u64,
    pub last_gps_fix: Option<Value>,
    pub last_lte_signal: Option<Value>,
}

/// Maximum age of a snapshot before it gets pruned (7 days).
const SNAPSHOT_MAX_AGE_SECS: u64 = 7 * 24 * 3600;

/// State shared across all relay handlers.
#[derive(Clone)]
pub struct RelayState {
    /// Connected devices keyed by serial number.
    pub devices: Arc<RwLock<HashMap<String, ConnectedDevice>>>,
    /// The shared tunnel key for device registration auth.
    pub tunnel_key: String,
    /// Seconds before a device is evicted for missed heartbeat (default 20).
    pub heartbeat_timeout_secs: u64,
    /// Default proxy request timeout in seconds (default 60).
    pub tunnel_proxy_timeout_secs: u64,
    /// Process epoch for lock-free heartbeat timestamps.
    pub epoch: Instant,
    /// Connection history ring buffer for relay dashboard.
    pub history: Arc<RelayConnectionHistory>,
    /// Last-known device state, survives disconnects and relay restarts.
    pub device_snapshots: Arc<RwLock<HashMap<String, DeviceSnapshot>>>,
    /// Monotonic connection generation counter used to fence stale handlers.
    pub next_connection_id: Arc<AtomicU64>,
    /// Dirty flag for debounced snapshot persistence.
    pub snapshots_dirty: Arc<AtomicBool>,
    /// Path to snapshot persistence file (None if no data_dir configured).
    pub snapshots_path: Option<PathBuf>,
}

/// A device connected to the relay via its outbound WS tunnel.
pub struct ConnectedDevice {
    pub connection_id: u64,
    pub serial: String,
    pub api_key: String,
    /// Send messages to the device over the tunnel WS.
    pub device_tx: mpsc::Sender<TunnelMessage>,
    /// Pending REST-over-WS requests awaiting responses, keyed by `request_id`.
    pub pending_requests: Arc<Mutex<HashMap<String, oneshot::Sender<TunnelResponse>>>>,
    /// Connected WS clients, keyed by `client_id`.
    pub clients: Arc<RwLock<HashMap<String, mpsc::Sender<Value>>>>,
    /// Session subscriptions: `session_id` -> set of `client_ids` watching output.
    pub session_subscriptions: Arc<RwLock<HashMap<String, HashSet<String>>>>,
    /// Last heartbeat timestamp as ms since relay epoch (lock-free).
    pub last_heartbeat_ms: Arc<AtomicU64>,
    /// When this device connected.
    pub connected_since: Instant,
    /// Count of messages dropped due to client backpressure.
    pub dropped_messages: Arc<AtomicU64>,
    /// Signal old device handler to shut down on duplicate serial reconnect.
    pub shutdown_tx: watch::Sender<bool>,
    /// Latest GPS fix broadcast from device.
    pub last_gps_fix: Arc<RwLock<Option<Value>>>,
    /// Latest LTE signal broadcast from device.
    pub last_lte_signal: Arc<RwLock<Option<Value>>>,
}

/// Drain all pending requests for a device, sending error responses on each oneshot.
/// Also notifies all connected WS clients that the device disconnected.
async fn drain_device(device: &ConnectedDevice, reason: &str) {
    // Drain pending REST-over-WS requests
    let mut pending = device.pending_requests.lock().await;
    let count = pending.len();
    for (_, sender) in pending.drain() {
        let _ = sender.send(TunnelResponse::Json(json!({
            "type": "error",
            "status": 502,
            "body": {"error": reason, "code": "DEVICE_DISCONNECTED"},
        })));
    }
    if count > 0 {
        info!(
            serial = %device.serial,
            count,
            "Drained {count} pending requests: {reason}"
        );
    }

    // Notify all connected WS clients
    let clients = device.clients.read().await;
    if !clients.is_empty() {
        let disconnect_msg = json!({
            "type": "tunnel.device_disconnected",
            "serial": device.serial,
            "reason": reason,
        });
        for (_, client_tx) in clients.iter() {
            let _ = client_tx.try_send(disconnect_msg.clone());
        }
    }
}

impl RelayState {
    pub fn new(
        tunnel_key: String,
        heartbeat_timeout_secs: u64,
        tunnel_proxy_timeout_secs: u64,
        data_dir: Option<&str>,
    ) -> Self {
        let snapshots_path = data_dir.map(|d| Path::new(d).join("relay_snapshots.json"));
        let snapshots = snapshots_path
            .as_ref()
            .map_or_else(HashMap::new, |p| load_snapshots(p));
        if !snapshots.is_empty() {
            info!("Loaded {} device snapshot(s) from disk", snapshots.len());
        }
        Self {
            devices: Arc::new(RwLock::new(HashMap::new())),
            tunnel_key,
            heartbeat_timeout_secs,
            tunnel_proxy_timeout_secs,
            epoch: Instant::now(),
            history: Arc::new(RelayConnectionHistory::new()),
            device_snapshots: Arc::new(RwLock::new(snapshots)),
            next_connection_id: Arc::new(AtomicU64::new(1)),
            snapshots_dirty: Arc::new(AtomicBool::new(false)),
            snapshots_path,
        }
    }

    /// Evict devices whose heartbeat is older than `heartbeat_timeout_secs`.
    /// Returns the serials of evicted devices.
    ///
    /// Uses a single write-lock pass with atomic heartbeat reads to avoid
    /// TOCTOU races (device could send heartbeat between read-lock and write-lock).
    pub async fn sweep_dead_devices(&self) -> Vec<String> {
        let timeout_ms = self.heartbeat_timeout_secs * 1000;
        #[allow(clippy::cast_possible_truncation)]
        let now_ms = self.epoch.elapsed().as_millis() as u64;

        let mut devices = self.devices.write().await;
        let mut dead_serials = Vec::new();

        // Collect serials first to avoid borrow conflict
        let serials: Vec<String> = devices.keys().cloned().collect();
        for serial in serials {
            if let Some(device) = devices.get(&serial) {
                let last_hb = device.last_heartbeat_ms.load(Ordering::Relaxed);
                if now_ms.saturating_sub(last_hb) > timeout_ms {
                    let hb_age = Some(now_ms.saturating_sub(last_hb));
                    drain_device(device, "heartbeat timeout").await;
                    devices.remove(&serial);
                    self.history
                        .record_disconnect(&serial, "heartbeat_timeout", hb_age)
                        .await;
                    warn!(serial = %serial, "Evicted device (heartbeat timeout)");
                    dead_serials.push(serial);
                }
            }
        }

        dead_serials
    }

    /// Send a message to all connected devices (e.g., for relay shutdown).
    /// Devices that fail to receive are collected for eviction.
    pub async fn broadcast_to_devices(&self, msg: Value) {
        let mut dead_serials = Vec::new();
        {
            let devices = self.devices.read().await;
            for (serial, device) in devices.iter() {
                if device
                    .device_tx
                    .send(TunnelMessage::Text(msg.clone()))
                    .await
                    .is_err()
                {
                    warn!(serial = %serial, "Failed to send broadcast to device, marking for eviction");
                    dead_serials.push(serial.clone());
                }
            }
        }
        // Evict devices with dead WS connections
        if !dead_serials.is_empty() {
            let mut devices = self.devices.write().await;
            for serial in &dead_serials {
                if let Some(device) = devices.get(serial) {
                    drain_device(device, "broadcast send failed").await;
                }
                devices.remove(serial);
                self.history
                    .record_disconnect(serial, "send_failed", None)
                    .await;
                warn!(serial = %serial, "Evicted device (broadcast send failed)");
            }
        }
    }

    /// Drain all devices and clear state (used during relay shutdown).
    pub async fn drain_all(&self) {
        let mut devices = self.devices.write().await;
        for (serial, device) in devices.iter() {
            drain_device(device, "relay shutting down").await;
            self.history
                .record_disconnect(serial, "relay_shutdown", None)
                .await;
            info!(serial = %serial, "Drained device for relay shutdown");
        }
        devices.clear();
    }

    /// Touch a device's snapshot `last_seen` timestamp (e.g. on device registration).
    pub async fn touch_snapshot(&self, serial: &str) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut snapshots = self.device_snapshots.write().await;
        if let Some(snap) = snapshots.get_mut(serial) {
            snap.last_seen = now;
            self.snapshots_dirty.store(true, Ordering::Relaxed);
        }
    }

    /// Update a device's snapshot with telemetry data.
    pub async fn update_snapshot(&self, serial: &str, field: &str, value: &Value) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut snapshots = self.device_snapshots.write().await;
        let snap = snapshots
            .entry(serial.to_string())
            .or_insert_with(|| DeviceSnapshot {
                serial: serial.to_string(),
                last_lte_signal: None,
                last_gps_fix: None,
                last_watchdog: None,
                last_seen: now,
            });
        match field {
            "lte.signal" => snap.last_lte_signal = Some(value.clone()),
            "gps.fix" => snap.last_gps_fix = Some(value.clone()),
            "lte.watchdog" => snap.last_watchdog = Some(value.clone()),
            _ => {}
        }
        snap.last_seen = now;
        self.snapshots_dirty.store(true, Ordering::Relaxed);
    }

    /// Save snapshots to disk if dirty. Returns true if a write occurred.
    pub async fn save_snapshots(&self) -> bool {
        if !self.snapshots_dirty.swap(false, Ordering::Relaxed) {
            return false;
        }
        let Some(ref path) = self.snapshots_path else {
            return false;
        };
        let snapshots = self.device_snapshots.read().await;
        save_snapshots(path, &snapshots);
        true
    }

    /// Snapshot currently connected devices for health/status surfaces.
    pub async fn live_device_statuses(&self) -> Vec<LiveDeviceStatus> {
        #[allow(clippy::cast_possible_truncation)]
        let now_ms = self.epoch.elapsed().as_millis() as u64;
        let devices = self.devices.read().await;
        let mut list = Vec::with_capacity(devices.len());

        for device in devices.values() {
            let last_hb_ms = device.last_heartbeat_ms.load(Ordering::Relaxed);
            let last_heartbeat_age_ms = now_ms.saturating_sub(last_hb_ms);
            #[allow(clippy::cast_possible_truncation)]
            let connected_since_ms = device.connected_since.elapsed().as_millis() as u64;
            let pending_requests_count = device.pending_requests.lock().await.len();
            let client_count = device.clients.read().await.len();
            let subs = device.session_subscriptions.read().await;
            let session_subscription_count = subs.len();
            let subscribed_client_count = subs.values().map(HashSet::len).sum();
            let last_gps_fix = device.last_gps_fix.read().await.clone();
            let last_lte_signal = device.last_lte_signal.read().await.clone();

            let connected_at = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                .saturating_sub(connected_since_ms / 1000);

            list.push(LiveDeviceStatus {
                serial: device.serial.clone(),
                connected: true,
                connected_at,
                connected_since_ms,
                last_heartbeat_age_ms,
                pending_requests_count,
                session_subscription_count,
                subscribed_client_count,
                client_count,
                dropped_messages: device.dropped_messages.load(Ordering::Relaxed),
                last_gps_fix,
                last_lte_signal,
            });
        }

        list.sort_by(|a, b| a.serial.cmp(&b.serial));
        list
    }
}

/// Load snapshots from disk, pruning entries older than 7 days.
fn load_snapshots(path: &Path) -> HashMap<String, DeviceSnapshot> {
    let Ok(data) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    let mut map: HashMap<String, DeviceSnapshot> = match serde_json::from_str(&data) {
        Ok(m) => m,
        Err(e) => {
            warn!("Failed to parse relay_snapshots.json: {e}");
            return HashMap::new();
        }
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    map.retain(|_, snap| now.saturating_sub(snap.last_seen) < SNAPSHOT_MAX_AGE_SECS);
    map
}

/// Atomically write snapshots to disk (write to .tmp, then rename).
fn save_snapshots(path: &Path, snapshots: &HashMap<String, DeviceSnapshot>) {
    let tmp = path.with_extension("json.tmp");
    let Ok(data) = serde_json::to_string_pretty(snapshots) else {
        warn!("Failed to serialize snapshots");
        return;
    };
    if let Err(e) = std::fs::write(&tmp, &data) {
        warn!("Failed to write snapshot tmp file: {e}");
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        warn!("Failed to rename snapshot file: {e}");
    }
}

/// Build the relay router with all tunnel endpoints.
pub fn relay_router(relay_state: RelayState) -> Router {
    // Tunnel management endpoints (authenticated with tunnel_key)
    let tunnel_admin = Router::new()
        .route("/api/tunnel/register", get(device_register_ws))
        .route("/api/tunnel/devices", get(list_devices));

    // Device proxy endpoints: /d/{serial}/api/*
    let device_proxy = Router::new()
        .route("/d/{serial}/api/health", get(proxy_health))
        .route("/d/{serial}/api/info", get(proxy_info))
        .route("/d/{serial}/api/diagnostics", get(proxy_diagnostics))
        .route("/d/{serial}/api/exec", post(proxy_exec))
        .route("/d/{serial}/api/exec/batch", post(proxy_exec_batch))
        .route(
            "/d/{serial}/api/files",
            get(proxy_file_read)
                .put(proxy_file_write)
                .delete(proxy_file_delete),
        )
        // gawdxfer STP proxy endpoints (replaces old /api/files/raw and /api/files/upload proxy)
        .route(
            "/d/{serial}/api/stp/download",
            post(proxy_stp_download_init),
        )
        .route("/d/{serial}/api/stp/upload", post(proxy_stp_upload_init))
        .route(
            "/d/{serial}/api/stp/chunk/{xfer}/{idx}",
            get(proxy_stp_download_chunk).post(proxy_stp_upload_chunk),
        )
        .route("/d/{serial}/api/stp/resume/{xfer}", post(proxy_stp_resume))
        .route("/d/{serial}/api/stp/status/{xfer}", get(proxy_stp_status))
        .route("/d/{serial}/api/stp/transfers", get(proxy_stp_list))
        .route("/d/{serial}/api/stp/{xfer}", delete(proxy_stp_abort))
        .route("/d/{serial}/api/activity", get(proxy_activity))
        .route(
            "/d/{serial}/api/activity/{id}/result",
            get(proxy_exec_result),
        )
        .route("/d/{serial}/api/sessions", get(proxy_sessions))
        .route(
            "/d/{serial}/api/sessions/{id}",
            delete(proxy_session_kill).patch(proxy_session_patch),
        )
        .route(
            "/d/{serial}/api/sessions/{id}/signal",
            post(proxy_session_signal),
        )
        .route("/d/{serial}/api/shells", get(proxy_shells))
        .route("/d/{serial}/api/playbooks", get(proxy_playbooks_list))
        .route(
            "/d/{serial}/api/playbooks/{name}",
            get(proxy_playbook_get)
                .put(proxy_playbook_put)
                .delete(proxy_playbook_delete),
        )
        .route("/d/{serial}/api/gps", get(proxy_gps))
        .route("/d/{serial}/api/lte", get(proxy_lte))
        .route("/d/{serial}/api/lte/bands", post(proxy_lte_bands))
        .route("/d/{serial}/api/lte/scan", post(proxy_lte_scan))
        .route("/d/{serial}/api/lte/speedtest", post(proxy_lte_speedtest))
        // Infra monitoring proxy endpoints
        .route(
            "/d/{serial}/api/infra/config",
            post(proxy_infra_config_push).delete(proxy_infra_config_delete),
        )
        .route("/d/{serial}/api/infra/results", get(proxy_infra_results))
        .route("/d/{serial}/api/infra/discover", post(proxy_infra_discover))
        .route(
            "/d/{serial}/api/infra/discover/progress",
            get(proxy_infra_discover_progress),
        )
        .route(
            "/d/{serial}/api/infra/check/{target_id}",
            post(proxy_infra_check),
        )
        .route("/d/{serial}/api/ws", get(proxy_ws));

    tunnel_admin.merge(device_proxy).with_state(relay_state)
}

// ─── Device Registration ─────────────────────────────────────────────────────

/// Query params for the device registration WS.
#[derive(Deserialize)]
struct RegisterQuery {
    token: String,
    serial: String,
}

/// Validate serial format: alphanumeric, dash, underscore, dot, 1-64 chars.
fn is_valid_serial(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// Maximum concurrent WS clients per device.
const MAX_CLIENTS_PER_DEVICE: usize = 32;

/// `GET /api/tunnel/register?token=<tunnel_key>&serial=<serial>` — device WS registration.
async fn device_register_ws(
    State(state): State<RelayState>,
    Query(query): Query<RegisterQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    if !crate::auth::constant_time_eq(state.tunnel_key.as_bytes(), query.token.as_bytes()) {
        return (StatusCode::FORBIDDEN, "Invalid tunnel key").into_response();
    }

    if !is_valid_serial(&query.serial) {
        return (StatusCode::BAD_REQUEST, "Invalid serial format").into_response();
    }

    let serial = query.serial.clone();
    info!(serial = %serial, "Device connecting...");

    ws.on_upgrade(move |socket| {
        handle_device_ws(socket, state, serial.clone())
            .instrument(info_span!("tunnel_device", serial = %serial))
    })
}

/// Handle a registered device's WebSocket connection.
#[allow(clippy::too_many_lines)]
async fn handle_device_ws(socket: axum::extract::ws::WebSocket, state: RelayState, serial: String) {
    let (mut ws_sink, mut ws_stream) = socket.split();
    let (device_tx, mut device_rx) = mpsc::channel::<TunnelMessage>(256);
    // Priority channel for ping/pong — bypasses the main device_tx queue so
    // control messages aren't delayed behind request bursts from sctlin/MCP.
    let (priority_tx, mut priority_rx) = mpsc::channel::<TunnelMessage>(8);

    // Wait for the registration message which contains the api_key
    let Some(Ok(axum::extract::ws::Message::Text(text))) = ws_stream.next().await else {
        warn!(serial = %serial, "Device disconnected before registration");
        return;
    };
    let api_key = match serde_json::from_str::<Value>(&text) {
        Ok(msg) if msg["type"].as_str() == Some("tunnel.register") => {
            msg["api_key"].as_str().unwrap_or("").to_string()
        }
        _ => {
            warn!(serial = %serial, "Device sent invalid registration");
            return;
        }
    };

    if api_key.is_empty() {
        warn!(serial = %serial, "Device registered with empty api_key");
        return;
    }

    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

    #[allow(clippy::cast_possible_truncation)]
    let now_ms = state.epoch.elapsed().as_millis() as u64;
    // Reuse the old device's clients + session_subscriptions Arcs (if any).
    // When a device reconnects (LTE flap, etc.), WS clients are still connected
    // to the relay. By sharing the same Arcs, client handlers' references stay
    // valid — cleanup (remove on disconnect) works regardless of tunnel reconnects.
    let (shared_clients, shared_subs, shared_gps, shared_lte) = {
        let devices = state.devices.read().await;
        if let Some(old_device) = devices.get(&serial) {
            let clients = old_device.clients.clone();
            let subs = old_device.session_subscriptions.clone();
            let gps = old_device.last_gps_fix.clone();
            let lte = old_device.last_lte_signal.clone();
            let n = clients.read().await.len();
            if n > 0 {
                info!(
                    serial = %serial,
                    clients = n,
                    "Preserving {n} WS clients across device reconnect"
                );
            }
            (clients, subs, gps, lte)
        } else {
            (
                Arc::new(RwLock::new(HashMap::new())),
                Arc::new(RwLock::new(HashMap::new())),
                Arc::new(RwLock::new(None)),
                Arc::new(RwLock::new(None)),
            )
        }
    };

    let connection_id = state.next_connection_id.fetch_add(1, Ordering::Relaxed);
    let pong_count = Arc::new(AtomicU64::new(0));
    let device = ConnectedDevice {
        connection_id,
        serial: serial.clone(),
        api_key,
        device_tx: device_tx.clone(),
        pending_requests: Arc::new(Mutex::new(HashMap::new())),
        clients: shared_clients,
        session_subscriptions: shared_subs,
        last_heartbeat_ms: Arc::new(AtomicU64::new(now_ms)),
        connected_since: Instant::now(),
        dropped_messages: Arc::new(AtomicU64::new(0)),
        shutdown_tx,
        last_gps_fix: shared_gps,
        last_lte_signal: shared_lte,
    };

    let pending_requests = device.pending_requests.clone();
    let clients = device.clients.clone();
    let session_subs = device.session_subscriptions.clone();
    let heartbeat_ms = device.last_heartbeat_ms.clone();
    let relay_epoch = state.epoch;
    let dropped_messages = device.dropped_messages.clone();
    let last_gps_fix = device.last_gps_fix.clone();
    let last_lte_signal = device.last_lte_signal.clone();

    // Handle duplicate serial: signal old handler to shut down, drain pending
    // REST requests, then replace. Don't notify WS clients — they were migrated above.
    {
        let mut devices = state.devices.write().await;
        if let Some(old_device) = devices.get(&serial) {
            warn!(
                serial = %serial,
                "Device re-registering while stale connection exists, evicting old"
            );
            let _ = old_device.shutdown_tx.send(true);
            // Only drain pending REST requests — clients were already migrated
            let mut pending = old_device.pending_requests.lock().await;
            let count = pending.len();
            for (_, sender) in pending.drain() {
                let _ = sender.send(TunnelResponse::Json(json!({
                    "type": "error",
                    "status": 502,
                    "body": {"error": "device reconnecting", "code": "DEVICE_RECONNECTING"},
                })));
            }
            if count > 0 {
                info!(serial = %serial, count, "Drained {count} pending REST requests");
            }
        }
        devices.insert(serial.clone(), device);
    }
    state.history.record_connect(&serial).await;
    info!(serial = %serial, "Device registered");

    // Send ack
    let ack = json!({"type": "tunnel.register.ack", "serial": &serial});
    let _ = ws_sink
        .send(axum::extract::ws::Message::Text(
            serde_json::to_string(&ack).unwrap().into(),
        ))
        .await;

    // Forward device_tx messages to the WS sink.
    // Writer exit notification: if ws_sink.send() fails, notify main loop immediately
    // so it can break instead of reading forever with a dead write path.
    let (writer_exit_tx, writer_exit_rx) = tokio::sync::oneshot::channel::<()>();
    let writer_serial = serial.clone();
    let send_task = tokio::spawn(async move {
        loop {
            // Priority-first: always drain priority_rx before device_rx.
            // This ensures pong messages bypass request queue depth, so the
            // device's pong watchdog doesn't fire during sctlin request bursts.
            let msg = tokio::select! {
                biased;
                msg = priority_rx.recv() => msg,
                msg = device_rx.recv() => msg,
            };
            let Some(msg) = msg else { break };
            let ws_msg = match msg {
                TunnelMessage::Text(val) => {
                    let text = match serde_json::to_string(&val) {
                        Ok(t) => t,
                        Err(e) => {
                            tracing::error!(serial = %writer_serial, "JSON serialize failed in writer: {e}");
                            continue;
                        }
                    };
                    axum::extract::ws::Message::Text(text.into())
                }
                TunnelMessage::Binary(data) => axum::extract::ws::Message::Binary(data.into()),
            };
            // 10s timeout on WS send: if the TCP send buffer is full and the
            // kernel can't drain it (dead write path), we detect it here instead
            // of blocking the writer indefinitely.
            match tokio::time::timeout(Duration::from_secs(10), ws_sink.send(ws_msg)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    warn!(serial = %writer_serial, error = %e, "Relay writer: WS send failed, exiting");
                    break;
                }
                Err(_) => {
                    warn!(serial = %writer_serial, "Relay writer: WS send timed out (10s), exiting — write path stuck");
                    break;
                }
            }
        }
        let _ = writer_exit_tx.send(());
    });
    let mut writer_exit_rx = writer_exit_rx;

    // Relay-side active ping: send tunnel.ping via priority channel so pings
    // bypass the request queue and reach the device promptly.
    let ping_tx = priority_tx.clone();
    let mut ping_shutdown_rx = shutdown_rx.clone();
    let ping_serial = serial.clone();
    let ping_pong_count = pong_count.clone();
    // Relay-side bidirectional liveness: track whether the device responds to
    // OUR pings. If we send 3 pings (30s) with no pong back, the write path is dead
    // (data goes into TCP buffer but never reaches the device). Close the
    // connection so the device can reconnect with a fresh TCP session.
    let (relay_pong_timeout_tx, relay_pong_timeout_rx) = tokio::sync::oneshot::channel::<()>();
    let relay_ping_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(10));
        interval.tick().await; // skip immediate first tick
        let mut pings_sent_since_pong: u32 = 0;
        let mut last_pong_count: u64 = 0;
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    // Check if device responded to our pings (bidirectional liveness)
                    let current_pong_count = ping_pong_count.load(Ordering::Relaxed);
                    if current_pong_count > last_pong_count {
                        pings_sent_since_pong = 0;
                        last_pong_count = current_pong_count;
                    } else {
                        pings_sent_since_pong += 1;
                        if pings_sent_since_pong >= 3 {
                            // 3 pings (30s) with no pong — write path is dead
                            warn!(serial = %ping_serial, pings = pings_sent_since_pong, "Relay: write path dead (no pong from device), closing connection");
                            let _ = relay_pong_timeout_tx.send(());
                            break;
                        }
                    }

                    if ping_tx.send(TunnelMessage::Text(json!({"type": "tunnel.ping"}))).await.is_err() {
                        info!(serial = %ping_serial, "Relay ping: device_tx closed, exiting");
                        break;
                    }
                }
                _ = ping_shutdown_rx.changed() => break,
            }
        }
    });

    // Process messages from the device
    let mut disconnect_reason = "ws_close"; // default: stream ended or close frame
    let mut relay_pong_timeout_rx = relay_pong_timeout_rx;
    loop {
        let msg = tokio::select! {
            msg = ws_stream.next() => {
                let Some(Ok(msg)) = msg else { break };
                msg
            }
            _ = shutdown_rx.changed() => {
                info!(serial = %serial, "Device handler shutting down (replaced by new connection)");
                disconnect_reason = "replaced";
                break;
            }
            _ = &mut writer_exit_rx => {
                warn!(serial = %serial, "Relay writer task exited — write path dead, closing device connection");
                disconnect_reason = "writer_failed";
                break;
            }
            _ = &mut relay_pong_timeout_rx => {
                warn!(serial = %serial, "Relay: closing connection — device not responding to relay pings (write path dead)");
                disconnect_reason = "write_path_dead";
                break;
            }
        };
        match msg {
            axum::extract::ws::Message::Text(text) => {
                let Ok(parsed) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                let msg_type = parsed["type"].as_str().unwrap_or("");

                // Fast path: session output (hot path, bulk of traffic).
                // These come from the device's tunnel_subscriber_task and are NEVER
                // tagged with client_id prefixes — the subscriber doesn't know about
                // client_ids. So we skip the request_id untag check entirely.
                if matches!(
                    msg_type,
                    "session.stdout" | "session.stderr" | "session.system"
                ) {
                    if let Some(session_id) = parsed["session_id"].as_str() {
                        let session_id_owned = session_id.to_string();
                        let subs = session_subs.read().await;
                        if let Some(client_ids) = subs.get(session_id) {
                            let clients_read = clients.read().await;
                            let count = client_ids.len();
                            if count == 1 {
                                // Single-subscriber fast path: move instead of clone
                                if let Some(cid) = client_ids.iter().next() {
                                    if let Some(client_tx) = clients_read.get(cid) {
                                        if client_tx.try_send(parsed).is_err() {
                                            dropped_messages.fetch_add(1, Ordering::Relaxed);
                                            warn!(
                                                serial = %serial,
                                                session_id = %session_id_owned,
                                                client_id = %cid,
                                                "Dropped session output (backpressure)"
                                            );
                                            // Notify client about the gap so it can re-attach
                                            let _ = client_tx.try_send(json!({
                                                "type": "session.gap",
                                                "session_id": session_id_owned,
                                                "reason": "backpressure",
                                            }));
                                        }
                                    }
                                }
                            } else {
                                for cid in client_ids {
                                    if let Some(client_tx) = clients_read.get(cid) {
                                        if client_tx.try_send(parsed.clone()).is_err() {
                                            dropped_messages.fetch_add(1, Ordering::Relaxed);
                                            warn!(
                                                serial = %serial,
                                                session_id = %session_id_owned,
                                                client_id = %cid,
                                                "Dropped session output (backpressure)"
                                            );
                                            let _ = client_tx.try_send(json!({
                                                "type": "session.gap",
                                                "session_id": session_id_owned,
                                                "reason": "backpressure",
                                            }));
                                        }
                                    }
                                }
                            }
                        }
                    }
                    continue;
                }

                match msg_type {
                    "tunnel.ping" => {
                        // Device heartbeat — respond and update timestamp (lock-free)
                        #[allow(clippy::cast_possible_truncation)]
                        let now_ms = relay_epoch.elapsed().as_millis() as u64;
                        heartbeat_ms.store(now_ms, Ordering::Relaxed);
                        // Device heartbeat arriving proves the connection is alive.
                        // Count it as a "pong" for the relay's bidirectional liveness
                        // check. This prevents false positives when relay→device pings
                        // are queued behind sctlin request bursts in device_tx.
                        // The device's OWN pong watchdog handles relay→device failures.
                        pong_count.fetch_add(1, Ordering::Relaxed);
                        // Send pong via priority channel — bypasses request queue
                        // so device receives it promptly.
                        match priority_tx
                            .try_send(TunnelMessage::Text(json!({"type": "tunnel.pong"})))
                        {
                            Ok(()) => {
                                tracing::debug!(serial = %serial, "Relay: pong queued");
                            }
                            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                warn!(serial = %serial, "Relay: pong dropped (channel full, writer stuck?)");
                            }
                            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                                warn!(serial = %serial, "Relay: pong send failed (writer dead), closing connection");
                                break;
                            }
                        }
                    }
                    "tunnel.pong" => {
                        // Response to relay-initiated ping — update heartbeat timestamp
                        #[allow(clippy::cast_possible_truncation)]
                        let now_ms = relay_epoch.elapsed().as_millis() as u64;
                        heartbeat_ms.store(now_ms, Ordering::Relaxed);
                        pong_count.fetch_add(1, Ordering::Relaxed);
                    }
                    // Response routing: matches .result (REST responses) and .ack (gx.chunk.ack, etc.)
                    // GUARD: New message types with non-.result/.ack suffixes need explicit handling.
                    #[allow(clippy::case_sensitive_file_extension_comparisons)]
                    t if t.ends_with(".result") || t.ends_with(".ack") => {
                        if let Some(request_id) = parsed["request_id"].as_str() {
                            // Check if this is a client-tagged request (contains ':')
                            if let Some(colon_pos) = request_id.find(':') {
                                let client_id = &request_id[..colon_pos];
                                let original_rid = &request_id[colon_pos + 1..];
                                info!(
                                    serial = %serial,
                                    client_id,
                                    msg_type = t,
                                    request_id = original_rid,
                                    "Relay WS response routed to client"
                                );

                                // Route to the specific client
                                let clients_read = clients.read().await;
                                if let Some(client_tx) = clients_read.get(client_id) {
                                    let mut response = parsed.clone();
                                    response["request_id"] = json!(original_rid);
                                    let _ = client_tx.send(response).await;
                                }
                            } else {
                                // REST proxy request — resolve the oneshot
                                let mut pending = pending_requests.lock().await;
                                if let Some(sender) = pending.remove(request_id) {
                                    let _ = sender.send(TunnelResponse::Json(parsed));
                                } else {
                                    warn!(
                                        serial = %serial,
                                        request_id,
                                        msg_type = t,
                                        "Response arrived for timed-out or unknown request (dropped)"
                                    );
                                }
                            }
                        }
                    }
                    // Session lifecycle broadcasts — forward to ALL clients of this device
                    "session.started"
                    | "session.created"
                    | "session.destroyed"
                    | "session.closed"
                    | "session.exited"
                    | "session.renamed"
                    | "session.ai_status_changed"
                    | "session.ai_permission_changed"
                    | "session.exec.ack"
                    | "session.signal.ack"
                    | "session.resize.ack"
                    | "session.attached"
                    | "session.listed"
                    | "session.allow_ai.ack"
                    | "session.ai_status.ack"
                    | "session.rename.ack"
                    | "shell.listed"
                    | "activity.new"
                    | "gx.progress"
                    | "gx.complete"
                    | "gx.error"
                    | "error" => {
                        // Clean up session subscriptions when session is destroyed/closed
                        if msg_type == "session.destroyed" || msg_type == "session.closed" {
                            if let Some(sid) = parsed["session_id"].as_str() {
                                session_subs.write().await.remove(sid);
                            }
                        }

                        // Auto-subscribe client to session output on session.started
                        if msg_type == "session.started" {
                            if let (Some(rid), Some(sid)) =
                                (parsed["request_id"].as_str(), parsed["session_id"].as_str())
                            {
                                if let Some(colon_pos) = rid.find(':') {
                                    let client_id = &rid[..colon_pos];
                                    session_subs
                                        .write()
                                        .await
                                        .entry(sid.to_string())
                                        .or_default()
                                        .insert(client_id.to_string());
                                }
                            }
                        }

                        // Check for client-tagged request_id
                        if let Some(rid) = parsed["request_id"].as_str() {
                            if let Some(colon_pos) = rid.find(':') {
                                let client_id = &rid[..colon_pos];
                                let original_rid = &rid[colon_pos + 1..];
                                info!(
                                    serial = %serial,
                                    client_id,
                                    msg_type,
                                    request_id = original_rid,
                                    session_id = parsed["session_id"].as_str().unwrap_or(""),
                                    "Relay WS lifecycle message routed to client"
                                );
                                let clients_read = clients.read().await;
                                if let Some(client_tx) = clients_read.get(client_id) {
                                    let mut msg = parsed.clone();
                                    msg["request_id"] = json!(original_rid);
                                    let _ = client_tx.send(msg).await;
                                }
                                continue;
                            }
                        }

                        // No client tag — broadcast to all clients (backpressure-aware)
                        let clients_read = clients.read().await;
                        for (cid, client_tx) in clients_read.iter() {
                            if client_tx.try_send(parsed.clone()).is_err() {
                                dropped_messages.fetch_add(1, Ordering::Relaxed);
                                warn!(
                                    serial = %serial,
                                    client_id = %cid,
                                    "Dropped broadcast message (client backpressure)"
                                );
                            }
                        }
                    }
                    // Device telemetry broadcasts — store latest and forward to WS clients
                    "gps.fix" | "lte.signal" | "lte.watchdog" => {
                        match msg_type {
                            "gps.fix" => *last_gps_fix.write().await = Some(parsed.clone()),
                            "lte.signal" => *last_lte_signal.write().await = Some(parsed.clone()),
                            _ => {} // lte.watchdog has no ConnectedDevice field
                        }
                        // Update persistent snapshot
                        state.update_snapshot(&serial, msg_type, &parsed).await;
                        let clients_read = clients.read().await;
                        for (_, client_tx) in clients_read.iter() {
                            if client_tx.try_send(parsed.clone()).is_err() {
                                dropped_messages.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    _ => {
                        warn!(serial = %serial, msg_type, "Unknown message from device");
                    }
                }
            }
            axum::extract::ws::Message::Binary(data) => {
                // Binary frame from device — decode header and route to pending request
                if let Some((header, payload)) = decode_binary_frame(&data) {
                    if let Some(request_id) = header["request_id"].as_str() {
                        let mut pending = pending_requests.lock().await;
                        if let Some(sender) = pending.remove(request_id) {
                            let _ = sender.send(TunnelResponse::Binary {
                                header,
                                data: payload.to_vec(),
                            });
                        }
                    }
                }
            }
            axum::extract::ws::Message::Close(_) => break,
            _ => {}
        }
    }

    // Check if this handler was replaced by a new connection, regardless of
    // which select! branch fired. The new handler sends `true` on shutdown_tx
    // before inserting itself, but stale timeout branches can still race with
    // reconnect. Fence cleanup by connection_id so an old handler never drains
    // the newly-registered device for the same serial.
    let map_points_to_newer_connection = {
        let devices = state.devices.read().await;
        devices
            .get(&serial)
            .is_some_and(|d| d.connection_id != connection_id)
    };
    let replaced = shutdown_rx.has_changed().unwrap_or(false)
        || disconnect_reason == "replaced"
        || map_points_to_newer_connection;
    if replaced {
        state
            .history
            .record_disconnect(&serial, "replaced", None)
            .await;
        info!(serial = %serial, "Device handler exiting (replaced, skipping cleanup)");
    } else {
        // Compute heartbeat age before removing device from map
        #[allow(clippy::cast_possible_truncation)]
        let hb_age = {
            let devices = state.devices.read().await;
            devices.get(&serial).map(|d| {
                let now_ms = state.epoch.elapsed().as_millis() as u64;
                let last_hb = d.last_heartbeat_ms.load(Ordering::Relaxed);
                now_ms.saturating_sub(last_hb)
            })
        };
        // Single write lock: remove then drain. Avoids TOCTOU between read→write
        // and prevents holding a read lock across the async drain_device call.
        let removed = {
            let mut devices = state.devices.write().await;
            if devices
                .get(&serial)
                .is_some_and(|d| d.connection_id == connection_id)
            {
                devices.remove(&serial)
            } else {
                None
            }
        };
        if let Some(device) = removed {
            drain_device(&device, "device disconnected").await;
        }
        state
            .history
            .record_disconnect(&serial, disconnect_reason, hb_age)
            .await;
        info!(serial = %serial, reason = disconnect_reason, "Device disconnected");
    }
    send_task.abort();
    relay_ping_task.abort();
}

/// `GET /api/tunnel/devices` — list connected devices (admin, requires `tunnel_key`).
#[derive(Deserialize)]
struct DevicesQuery {
    token: String,
}

async fn list_devices(
    State(state): State<RelayState>,
    Query(query): Query<DevicesQuery>,
) -> Response {
    if !crate::auth::constant_time_eq(state.tunnel_key.as_bytes(), query.token.as_bytes()) {
        return (StatusCode::FORBIDDEN, "Invalid tunnel key").into_response();
    }

    let devices = state.devices.read().await;
    let mut list: Vec<Value> = Vec::with_capacity(devices.len());

    #[allow(clippy::cast_possible_truncation)]
    let now_ms = state.epoch.elapsed().as_millis() as u64;
    for d in devices.values() {
        let last_hb_ms = d.last_heartbeat_ms.load(Ordering::Relaxed);
        let hb_ago_ms = now_ms.saturating_sub(last_hb_ms);
        let pending_count = d.pending_requests.lock().await.len();
        let clients_read = d.clients.read().await;
        let client_ids: Vec<&String> = clients_read.keys().collect();
        let subs = d.session_subscriptions.read().await;
        let subs_map: HashMap<&String, Vec<&String>> = subs
            .iter()
            .map(|(sid, cids)| (sid, cids.iter().collect()))
            .collect();
        #[allow(clippy::cast_possible_truncation)]
        let connected_ms = d.connected_since.elapsed().as_millis() as u64;

        list.push(json!({
            "serial": d.serial,
            "clients": client_ids,
            "client_count": client_ids.len(),
            "last_heartbeat_ago_ms": hb_ago_ms,
            "pending_requests_count": pending_count,
            "session_subscriptions": subs_map,
            "connected_since_ms": connected_ms,
            "dropped_messages": d.dropped_messages.load(Ordering::Relaxed),
            "last_gps_fix": *d.last_gps_fix.read().await,
            "last_lte_signal": *d.last_lte_signal.read().await,
        }));
    }

    Json(json!({"devices": list})).into_response()
}

// ─── REST Proxy Helpers ──────────────────────────────────────────────────────

/// Send a tunnel request to a device and await the response.
pub async fn tunnel_request(
    state: &RelayState,
    serial: &str,
    msg: Value,
    timeout_secs: u64,
) -> Result<TunnelResponse, (StatusCode, Json<Value>)> {
    let devices = state.devices.read().await;
    let device = devices.get(serial).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("Device '{serial}' not connected"), "code": "DEVICE_NOT_FOUND"})),
        )
    })?;

    let request_id = msg["request_id"].as_str().unwrap_or("").to_string();

    // Cap pending requests to prevent unbounded growth from slow devices
    let pending = device.pending_requests.clone();
    let (tx, rx) = oneshot::channel();
    {
        let mut guard = pending.lock().await;
        if guard.len() >= 256 {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(
                    json!({"error": "Device has too many pending requests", "code": "OVERLOADED"}),
                ),
            ));
        }
        guard.insert(request_id.clone(), tx);
    }

    match tokio::time::timeout(
        Duration::from_secs(DEVICE_QUEUE_SEND_TIMEOUT_SECS),
        device.device_tx.send(TunnelMessage::Text(msg)),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            pending.lock().await.remove(&request_id);
            return Err((
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": "Failed to send to device", "code": "DEVICE_SEND_FAILED"})),
            ));
        }
        Err(_) => {
            pending.lock().await.remove(&request_id);
            return Err((
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "error": "Device request queue stalled",
                    "code": "DEVICE_QUEUE_STALLED"
                })),
            ));
        }
    }

    drop(devices); // Release read lock while waiting

    // Wait for response with timeout
    match tokio::time::timeout(Duration::from_secs(timeout_secs), rx).await {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(_)) => Err((
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": "Device connection lost", "code": "DEVICE_DISCONNECTED"})),
        )),
        Err(_) => {
            // Timeout — clean up unconditionally via stored Arc
            pending.lock().await.remove(&request_id);
            Err((
                StatusCode::GATEWAY_TIMEOUT,
                Json(json!({"error": "Device did not respond in time", "code": "TIMEOUT"})),
            ))
        }
    }
}

/// Send a tunnel request expecting a JSON response.
pub async fn tunnel_request_json(
    state: &RelayState,
    serial: &str,
    msg: Value,
    timeout_secs: u64,
) -> Result<Value, (StatusCode, Json<Value>)> {
    let response = tunnel_request(state, serial, msg, timeout_secs).await?;
    match response {
        TunnelResponse::Json(v) => Ok(v),
        TunnelResponse::Binary { .. } => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(
                json!({"error": "Expected JSON response, got binary", "code": "UNEXPECTED_BINARY"}),
            ),
        )),
    }
}

/// Send a binary tunnel request to a device and await the response.
pub async fn tunnel_request_binary(
    state: &RelayState,
    serial: &str,
    msg: TunnelMessage,
    request_id: &str,
    timeout_secs: u64,
) -> Result<TunnelResponse, (StatusCode, Json<Value>)> {
    let devices = state.devices.read().await;
    let device = devices.get(serial).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("Device '{serial}' not connected"), "code": "DEVICE_NOT_FOUND"})),
        )
    })?;

    let (tx, rx) = oneshot::channel();
    {
        let mut pending = device.pending_requests.lock().await;
        if pending.len() >= 256 {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(
                    json!({"error": "Device has too many pending requests", "code": "OVERLOADED"}),
                ),
            ));
        }
        pending.insert(request_id.to_string(), tx);
    }

    match tokio::time::timeout(
        Duration::from_secs(DEVICE_QUEUE_SEND_TIMEOUT_SECS),
        device.device_tx.send(msg),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            device.pending_requests.lock().await.remove(request_id);
            return Err((
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": "Failed to send to device", "code": "DEVICE_SEND_FAILED"})),
            ));
        }
        Err(_) => {
            device.pending_requests.lock().await.remove(request_id);
            return Err((
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "error": "Device request queue stalled",
                    "code": "DEVICE_QUEUE_STALLED"
                })),
            ));
        }
    }

    drop(devices);

    match tokio::time::timeout(Duration::from_secs(timeout_secs), rx).await {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(_)) => Err((
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": "Device connection lost", "code": "DEVICE_DISCONNECTED"})),
        )),
        Err(_) => {
            if let Some(device) = state.devices.read().await.get(serial) {
                device.pending_requests.lock().await.remove(request_id);
            }
            Err((
                StatusCode::GATEWAY_TIMEOUT,
                Json(json!({"error": "Device did not respond in time", "code": "TIMEOUT"})),
            ))
        }
    }
}

/// Validate device API key from Authorization header.
fn validate_device_auth<'a>(
    devices: &'a HashMap<String, ConnectedDevice>,
    serial: &str,
    auth_header: Option<&str>,
) -> Result<&'a ConnectedDevice, (StatusCode, Json<Value>)> {
    let device = devices.get(serial).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("Device '{serial}' not connected"), "code": "DEVICE_NOT_FOUND"})),
        )
    })?;

    let provided_key = match auth_header {
        Some(h) if h.starts_with("Bearer ") => &h[7..],
        _ => {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "Missing or invalid Authorization header"})),
            ));
        }
    };

    if !crate::auth::constant_time_eq(device.api_key.as_bytes(), provided_key.as_bytes()) {
        return Err((
            StatusCode::FORBIDDEN,
            Json(json!({"error": "Invalid API key"})),
        ));
    }

    Ok(device)
}

// ─── REST Proxy Endpoints ────────────────────────────────────────────────────

/// `GET /d/{serial}/api/health` — proxied health check (no auth).
async fn proxy_health(
    State(state): State<RelayState>,
    AxumPath(serial): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let request_id = uuid::Uuid::new_v4().to_string();
    let msg = json!({
        "type": "tunnel.health",
        "request_id": request_id,
    });

    let response = tunnel_request_json(&state, &serial, msg, 10).await?;
    let status = response["status"].as_u64().unwrap_or(200);
    let body = response["body"].clone();

    if status == 200 {
        Ok(Json(body))
    } else {
        #[allow(clippy::cast_possible_truncation)]
        Err((
            StatusCode::from_u16(status as u16).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(body),
        ))
    }
}

/// `GET /d/{serial}/api/info` — proxied system info.
#[derive(Deserialize)]
struct InfoProxyQuery {
    groups: Option<String>,
}

fn parse_info_groups_csv(groups: Option<&str>) -> Vec<String> {
    let mut parsed = groups
        .unwrap_or("core,interfaces,disk,tunnel,gps,lte")
        .split(',')
        .map(str::trim)
        .filter(|g| !g.is_empty())
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    if parsed.is_empty() || parsed.iter().any(|g| g == "all") {
        return vec![
            "core".to_string(),
            "interfaces".to_string(),
            "disk".to_string(),
            "tunnel".to_string(),
            "gps".to_string(),
            "lte".to_string(),
        ];
    }
    parsed.sort();
    parsed.dedup();
    parsed
}

async fn proxy_info(
    State(state): State<RelayState>,
    AxumPath(serial): AxumPath<String>,
    Query(query): Query<InfoProxyQuery>,
    request: Request<Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    {
        let devices = state.devices.read().await;
        validate_device_auth(&devices, &serial, auth_header.as_deref())?;
    }

    let groups = parse_info_groups_csv(query.groups.as_deref());
    let mut merged = serde_json::Map::new();

    // Fan out per-group info requests in parallel. Each group is a separate
    // tunnel.info message so a slow group doesn't block the others.
    let mut futures = Vec::with_capacity(groups.len());
    for group in &groups {
        let request_id = uuid::Uuid::new_v4().to_string();
        let msg = json!({
            "type": "tunnel.info",
            "request_id": request_id,
            "groups": [group],
        });
        futures.push(tunnel_request_json(&state, &serial, msg, 10));
    }

    let results = futures::future::join_all(futures).await;
    for result in results {
        let response = result?;
        let status = response["status"].as_u64().unwrap_or(200);
        let body = response["body"].clone();
        if status != 200 {
            #[allow(clippy::cast_possible_truncation)]
            return Err((
                StatusCode::from_u16(status as u16).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(body),
            ));
        }
        let body_obj = body.as_object().ok_or_else(|| {
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": "Invalid device info response", "code": "INVALID_DEVICE_RESPONSE"})),
            )
        })?;
        for (key, value) in body_obj {
            merged.insert(key.clone(), value.clone());
        }
    }

    Ok(Json(Value::Object(merged)))
}

/// Query parameters for the diagnostics proxy endpoint.
#[derive(Deserialize)]
struct DiagnosticsProxyQuery {
    log_lines: Option<u64>,
    log_since: Option<String>,
}

/// `GET /d/{serial}/api/diagnostics` — proxied server diagnostics.
async fn proxy_diagnostics(
    State(state): State<RelayState>,
    AxumPath(serial): AxumPath<String>,
    Query(query): Query<DiagnosticsProxyQuery>,
    request: Request<Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    {
        let devices = state.devices.read().await;
        validate_device_auth(&devices, &serial, auth_header.as_deref())?;
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let mut msg = json!({
        "type": "tunnel.diagnostics",
        "request_id": request_id,
    });
    if let Some(n) = query.log_lines {
        msg["log_lines"] = json!(n);
    }
    if let Some(ref s) = query.log_since {
        msg["log_since"] = json!(s);
    }

    let response =
        tunnel_request_json(&state, &serial, msg, state.tunnel_proxy_timeout_secs).await?;
    proxy_response_to_http(&response)
}

/// `POST /d/{serial}/api/exec` — proxied command execution.
async fn proxy_exec(
    State(state): State<RelayState>,
    AxumPath(serial): AxumPath<String>,
    request: Request<Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);
    let sctl_client = request
        .headers()
        .get("x-sctl-client")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    let body_bytes = axum::body::to_bytes(request.into_body(), 10 * 1024 * 1024)
        .await
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Failed to read request body"})),
            )
        })?;

    let payload: Value = serde_json::from_slice(&body_bytes).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Invalid JSON"})),
        )
    })?;

    {
        let devices = state.devices.read().await;
        validate_device_auth(&devices, &serial, auth_header.as_deref())?;
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    // Derive timeout: command timeout_ms + 5s margin, or config default
    let timeout_secs = payload["timeout_ms"]
        .as_u64()
        .map_or(state.tunnel_proxy_timeout_secs, |ms| ms / 1000 + 5);
    let mut msg = payload;
    msg["type"] = json!("tunnel.exec");
    msg["request_id"] = json!(request_id);
    if let Some(ref client) = sctl_client {
        msg["_source"] = json!(client);
    }

    let response = tunnel_request_json(&state, &serial, msg, timeout_secs).await?;
    proxy_response_to_http(&response)
}

/// `POST /d/{serial}/api/exec/batch` — proxied batch execution.
async fn proxy_exec_batch(
    State(state): State<RelayState>,
    AxumPath(serial): AxumPath<String>,
    request: Request<Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);
    let sctl_client = request
        .headers()
        .get("x-sctl-client")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    let body_bytes = axum::body::to_bytes(request.into_body(), 10 * 1024 * 1024)
        .await
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Failed to read request body"})),
            )
        })?;

    let payload: Value = serde_json::from_slice(&body_bytes).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Invalid JSON"})),
        )
    })?;

    {
        let devices = state.devices.read().await;
        validate_device_auth(&devices, &serial, auth_header.as_deref())?;
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    // Sum command timeouts + 5s margin per command, or config default
    let timeout_secs =
        payload["commands"]
            .as_array()
            .map_or(state.tunnel_proxy_timeout_secs, |cmds| {
                let total_ms: u64 = cmds
                    .iter()
                    .map(|c| c["timeout_ms"].as_u64().unwrap_or(30_000))
                    .sum();
                total_ms / 1000 + 5 * cmds.len() as u64
            });
    let mut msg = payload;
    msg["type"] = json!("tunnel.exec_batch");
    msg["request_id"] = json!(request_id);
    if let Some(ref client) = sctl_client {
        msg["_source"] = json!(client);
    }

    let response = tunnel_request_json(&state, &serial, msg, timeout_secs).await?;
    proxy_response_to_http(&response)
}

/// `GET /d/{serial}/api/files` — proxied file read/list.
#[derive(Deserialize)]
struct FilesProxyQuery {
    path: String,
    #[serde(default)]
    list: bool,
}

async fn proxy_file_read(
    State(state): State<RelayState>,
    AxumPath(serial): AxumPath<String>,
    Query(query): Query<FilesProxyQuery>,
    request: Request<Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);
    let sctl_client = request
        .headers()
        .get("x-sctl-client")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    {
        let devices = state.devices.read().await;
        validate_device_auth(&devices, &serial, auth_header.as_deref())?;
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let mut msg = json!({
        "type": "tunnel.file.read",
        "request_id": request_id,
        "path": query.path,
        "list": query.list,
    });
    if let Some(ref client) = sctl_client {
        msg["_source"] = json!(client);
    }

    let response =
        tunnel_request_json(&state, &serial, msg, state.tunnel_proxy_timeout_secs).await?;
    proxy_response_to_http(&response)
}

/// `PUT /d/{serial}/api/files` — proxied file write.
async fn proxy_file_write(
    State(state): State<RelayState>,
    AxumPath(serial): AxumPath<String>,
    request: Request<Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);
    let sctl_client = request
        .headers()
        .get("x-sctl-client")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    let body_bytes = axum::body::to_bytes(request.into_body(), 10 * 1024 * 1024)
        .await
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Failed to read request body"})),
            )
        })?;

    let payload: Value = serde_json::from_slice(&body_bytes).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Invalid JSON"})),
        )
    })?;

    {
        let devices = state.devices.read().await;
        validate_device_auth(&devices, &serial, auth_header.as_deref())?;
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let mut msg = payload;
    msg["type"] = json!("tunnel.file.write");
    msg["request_id"] = json!(request_id);
    if let Some(ref client) = sctl_client {
        msg["_source"] = json!(client);
    }

    let response =
        tunnel_request_json(&state, &serial, msg, state.tunnel_proxy_timeout_secs).await?;
    proxy_response_to_http(&response)
}

/// `DELETE /d/{serial}/api/files` — proxied file delete.
async fn proxy_file_delete(
    State(state): State<RelayState>,
    AxumPath(serial): AxumPath<String>,
    request: Request<Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);
    let sctl_client = request
        .headers()
        .get("x-sctl-client")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    let body_bytes = axum::body::to_bytes(request.into_body(), 10 * 1024 * 1024)
        .await
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Failed to read request body"})),
            )
        })?;

    let payload: Value = serde_json::from_slice(&body_bytes).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Invalid JSON"})),
        )
    })?;

    {
        let devices = state.devices.read().await;
        validate_device_auth(&devices, &serial, auth_header.as_deref())?;
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let mut msg = json!({
        "type": "tunnel.file.delete",
        "request_id": request_id,
        "path": payload["path"],
    });
    if let Some(ref client) = sctl_client {
        msg["_source"] = json!(client);
    }

    let response =
        tunnel_request_json(&state, &serial, msg, state.tunnel_proxy_timeout_secs).await?;
    proxy_response_to_http(&response)
}

/// Convert a tunnel response (with status + body) to an HTTP response.
pub fn proxy_response_to_http(response: &Value) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let status = response["status"].as_u64().unwrap_or(200);
    let body = response["body"].clone();

    if (200..300).contains(&status) {
        Ok(Json(body))
    } else {
        #[allow(clippy::cast_possible_truncation)]
        Err((
            StatusCode::from_u16(status as u16).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(body),
        ))
    }
}

/// `GET /d/{serial}/api/activity` — proxied activity journal.
async fn proxy_activity(
    State(state): State<RelayState>,
    AxumPath(serial): AxumPath<String>,
    Query(query): Query<ActivityProxyQuery>,
    request: Request<Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    {
        let devices = state.devices.read().await;
        validate_device_auth(&devices, &serial, auth_header.as_deref())?;
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let msg = json!({
        "type": "tunnel.activity",
        "request_id": request_id,
        "since_id": query.since_id,
        "limit": query.limit,
    });

    let response =
        tunnel_request_json(&state, &serial, msg, state.tunnel_proxy_timeout_secs).await?;
    proxy_response_to_http(&response)
}

/// Query parameters for the activity proxy endpoint.
#[derive(Deserialize)]
struct ActivityProxyQuery {
    #[serde(default)]
    since_id: u64,
    #[serde(default = "default_activity_limit")]
    limit: usize,
}

fn default_activity_limit() -> usize {
    50
}

/// `GET /d/{serial}/api/activity/{id}/result` — proxied exec result lookup.
async fn proxy_exec_result(
    State(state): State<RelayState>,
    AxumPath((serial, id)): AxumPath<(String, u64)>,
    request: Request<Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    {
        let devices = state.devices.read().await;
        validate_device_auth(&devices, &serial, auth_header.as_deref())?;
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let msg = json!({
        "type": "tunnel.exec_result",
        "request_id": request_id,
        "activity_id": id,
    });

    let response =
        tunnel_request_json(&state, &serial, msg, state.tunnel_proxy_timeout_secs).await?;
    proxy_response_to_http(&response)
}

/// `GET /d/{serial}/api/sessions` — proxied session list.
async fn proxy_sessions(
    State(state): State<RelayState>,
    AxumPath(serial): AxumPath<String>,
    request: Request<Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    {
        let devices = state.devices.read().await;
        validate_device_auth(&devices, &serial, auth_header.as_deref())?;
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let msg = json!({
        "type": "tunnel.sessions",
        "request_id": request_id,
    });

    let response =
        tunnel_request_json(&state, &serial, msg, state.tunnel_proxy_timeout_secs).await?;
    proxy_response_to_http(&response)
}

/// `GET /d/{serial}/api/shells` — proxied shell list.
async fn proxy_shells(
    State(state): State<RelayState>,
    AxumPath(serial): AxumPath<String>,
    request: Request<Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    {
        let devices = state.devices.read().await;
        validate_device_auth(&devices, &serial, auth_header.as_deref())?;
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let msg = json!({
        "type": "tunnel.shells",
        "request_id": request_id,
    });

    let response =
        tunnel_request_json(&state, &serial, msg, state.tunnel_proxy_timeout_secs).await?;
    proxy_response_to_http(&response)
}

// ─── Session Control Proxy Endpoints ──────────────────────────────────────────

/// `POST /d/{serial}/api/sessions/{id}/signal` — proxied session signal.
async fn proxy_session_signal(
    State(state): State<RelayState>,
    AxumPath((serial, id)): AxumPath<(String, String)>,
    request: Request<Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    let body_bytes = axum::body::to_bytes(request.into_body(), 1024)
        .await
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Failed to read request body"})),
            )
        })?;

    let payload: Value = serde_json::from_slice(&body_bytes).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Invalid JSON"})),
        )
    })?;

    {
        let devices = state.devices.read().await;
        validate_device_auth(&devices, &serial, auth_header.as_deref())?;
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let msg = json!({
        "type": "tunnel.session.signal",
        "request_id": request_id,
        "session_id": id,
        "signal": payload["signal"],
    });

    let response =
        tunnel_request_json(&state, &serial, msg, state.tunnel_proxy_timeout_secs).await?;
    proxy_response_to_http(&response)
}

/// `DELETE /d/{serial}/api/sessions/{id}` — proxied session kill.
async fn proxy_session_kill(
    State(state): State<RelayState>,
    AxumPath((serial, id)): AxumPath<(String, String)>,
    request: Request<Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    {
        let devices = state.devices.read().await;
        validate_device_auth(&devices, &serial, auth_header.as_deref())?;
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let msg = json!({
        "type": "tunnel.session.kill",
        "request_id": request_id,
        "session_id": id,
    });

    let response =
        tunnel_request_json(&state, &serial, msg, state.tunnel_proxy_timeout_secs).await?;
    proxy_response_to_http(&response)
}

/// `PATCH /d/{serial}/api/sessions/{id}` — proxied session patch.
async fn proxy_session_patch(
    State(state): State<RelayState>,
    AxumPath((serial, id)): AxumPath<(String, String)>,
    request: Request<Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    let body_bytes = axum::body::to_bytes(request.into_body(), 10 * 1024)
        .await
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Failed to read request body"})),
            )
        })?;

    let payload: Value = serde_json::from_slice(&body_bytes).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Invalid JSON"})),
        )
    })?;

    {
        let devices = state.devices.read().await;
        validate_device_auth(&devices, &serial, auth_header.as_deref())?;
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let mut msg = payload;
    msg["type"] = json!("tunnel.session.patch");
    msg["request_id"] = json!(request_id);
    msg["session_id"] = json!(id);

    let response =
        tunnel_request_json(&state, &serial, msg, state.tunnel_proxy_timeout_secs).await?;
    proxy_response_to_http(&response)
}

// ─── Playbook Proxy Endpoints ─────────────────────────────────────────────────

/// `GET /d/{serial}/api/playbooks` -- proxied playbook list.
async fn proxy_playbooks_list(
    State(state): State<RelayState>,
    AxumPath(serial): AxumPath<String>,
    request: Request<Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);
    let sctl_client = request
        .headers()
        .get("x-sctl-client")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    {
        let devices = state.devices.read().await;
        validate_device_auth(&devices, &serial, auth_header.as_deref())?;
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let mut msg = json!({
        "type": "tunnel.playbooks.list",
        "request_id": request_id,
    });
    if let Some(ref client) = sctl_client {
        msg["_source"] = json!(client);
    }

    let response =
        tunnel_request_json(&state, &serial, msg, state.tunnel_proxy_timeout_secs).await?;
    proxy_response_to_http(&response)
}

/// `GET /d/{serial}/api/playbooks/:name` -- proxied playbook get.
async fn proxy_playbook_get(
    State(state): State<RelayState>,
    AxumPath((serial, name)): AxumPath<(String, String)>,
    request: Request<Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);
    let sctl_client = request
        .headers()
        .get("x-sctl-client")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    {
        let devices = state.devices.read().await;
        validate_device_auth(&devices, &serial, auth_header.as_deref())?;
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let mut msg = json!({
        "type": "tunnel.playbooks.get",
        "request_id": request_id,
        "name": name,
    });
    if let Some(ref client) = sctl_client {
        msg["_source"] = json!(client);
    }

    let response =
        tunnel_request_json(&state, &serial, msg, state.tunnel_proxy_timeout_secs).await?;
    proxy_response_to_http(&response)
}

/// `PUT /d/{serial}/api/playbooks/:name` -- proxied playbook write.
async fn proxy_playbook_put(
    State(state): State<RelayState>,
    AxumPath((serial, name)): AxumPath<(String, String)>,
    request: Request<Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);
    let sctl_client = request
        .headers()
        .get("x-sctl-client")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    let body_bytes = axum::body::to_bytes(request.into_body(), 10 * 1024 * 1024)
        .await
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Failed to read request body"})),
            )
        })?;

    let content = String::from_utf8(body_bytes.to_vec()).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Invalid UTF-8"})),
        )
    })?;

    {
        let devices = state.devices.read().await;
        validate_device_auth(&devices, &serial, auth_header.as_deref())?;
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let mut msg = json!({
        "type": "tunnel.playbooks.put",
        "request_id": request_id,
        "name": name,
        "content": content,
    });
    if let Some(ref client) = sctl_client {
        msg["_source"] = json!(client);
    }

    let response =
        tunnel_request_json(&state, &serial, msg, state.tunnel_proxy_timeout_secs).await?;
    proxy_response_to_http(&response)
}

/// `DELETE /d/{serial}/api/playbooks/:name` -- proxied playbook delete.
async fn proxy_playbook_delete(
    State(state): State<RelayState>,
    AxumPath((serial, name)): AxumPath<(String, String)>,
    request: Request<Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);
    let sctl_client = request
        .headers()
        .get("x-sctl-client")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    {
        let devices = state.devices.read().await;
        validate_device_auth(&devices, &serial, auth_header.as_deref())?;
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let mut msg = json!({
        "type": "tunnel.playbooks.delete",
        "request_id": request_id,
        "name": name,
    });
    if let Some(ref client) = sctl_client {
        msg["_source"] = json!(client);
    }

    let response =
        tunnel_request_json(&state, &serial, msg, state.tunnel_proxy_timeout_secs).await?;
    proxy_response_to_http(&response)
}

// ─── WS Proxy ────────────────────────────────────────────────────────────────

/// Query params for client WS proxy.
#[derive(Deserialize)]
struct WsProxyQuery {
    token: String,
}

/// `GET /d/{serial}/api/gps` — proxied GPS data.
async fn proxy_gps(
    State(state): State<RelayState>,
    AxumPath(serial): AxumPath<String>,
    request: Request<Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    {
        let devices = state.devices.read().await;
        validate_device_auth(&devices, &serial, auth_header.as_deref())?;
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let msg = json!({
        "type": "tunnel.gps",
        "request_id": request_id,
    });

    let response =
        tunnel_request_json(&state, &serial, msg, state.tunnel_proxy_timeout_secs).await?;
    proxy_response_to_http(&response)
}

/// `GET /d/{serial}/api/lte` — proxied LTE signal data.
async fn proxy_lte(
    State(state): State<RelayState>,
    AxumPath(serial): AxumPath<String>,
    request: Request<Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    {
        let devices = state.devices.read().await;
        validate_device_auth(&devices, &serial, auth_header.as_deref())?;
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let msg = json!({
        "type": "tunnel.lte",
        "request_id": request_id,
    });

    let response =
        tunnel_request_json(&state, &serial, msg, state.tunnel_proxy_timeout_secs).await?;
    proxy_response_to_http(&response)
}

/// `POST /d/{serial}/api/lte/bands` — proxied band mode control.
async fn proxy_lte_bands(
    State(state): State<RelayState>,
    AxumPath(serial): AxumPath<String>,
    request: Request<Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    let body_bytes = axum::body::to_bytes(request.into_body(), 10 * 1024)
        .await
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Failed to read request body"})),
            )
        })?;

    let payload: Value = serde_json::from_slice(&body_bytes).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Invalid JSON"})),
        )
    })?;

    {
        let devices = state.devices.read().await;
        validate_device_auth(&devices, &serial, auth_header.as_deref())?;
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let mut msg = payload;
    msg["type"] = json!("tunnel.lte.bands");
    msg["request_id"] = json!(request_id);

    // Longer timeout — band changes involve registration wait
    let response = tunnel_request_json(&state, &serial, msg, 45).await?;
    proxy_response_to_http(&response)
}

/// `POST /d/{serial}/api/lte/scan` — proxied band scan start.
async fn proxy_lte_scan(
    State(state): State<RelayState>,
    AxumPath(serial): AxumPath<String>,
    request: Request<Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    let body_bytes = axum::body::to_bytes(request.into_body(), 10 * 1024)
        .await
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Failed to read request body"})),
            )
        })?;

    let payload: Value = serde_json::from_slice(&body_bytes).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Invalid JSON"})),
        )
    })?;

    {
        let devices = state.devices.read().await;
        validate_device_auth(&devices, &serial, auth_header.as_deref())?;
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let mut msg = payload;
    msg["type"] = json!("tunnel.lte.scan");
    msg["request_id"] = json!(request_id);

    let response =
        tunnel_request_json(&state, &serial, msg, state.tunnel_proxy_timeout_secs).await?;
    proxy_response_to_http(&response)
}

/// `POST /d/{serial}/api/lte/speedtest` — proxied speed test (no body needed).
async fn proxy_lte_speedtest(
    State(state): State<RelayState>,
    AxumPath(serial): AxumPath<String>,
    request: Request<Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    {
        let devices = state.devices.read().await;
        validate_device_auth(&devices, &serial, auth_header.as_deref())?;
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let msg = json!({
        "type": "tunnel.lte.speedtest",
        "request_id": request_id,
    });

    // Longer timeout — speed test takes ~20s (10s download + 10s upload)
    let response = tunnel_request_json(
        &state,
        &serial,
        msg,
        state.tunnel_proxy_timeout_secs.max(30),
    )
    .await?;
    proxy_response_to_http(&response)
}

// ─── Infra Monitoring Proxy Endpoints ────────────────────────────────────────

/// `GET /d/{serial}/api/infra/results` — proxied infra monitoring results.
async fn proxy_infra_results(
    State(state): State<RelayState>,
    AxumPath(serial): AxumPath<String>,
    request: Request<Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    {
        let devices = state.devices.read().await;
        validate_device_auth(&devices, &serial, auth_header.as_deref())?;
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let msg = json!({
        "type": "tunnel.infra.results",
        "request_id": request_id,
    });

    let response =
        tunnel_request_json(&state, &serial, msg, state.tunnel_proxy_timeout_secs).await?;
    proxy_response_to_http(&response)
}

/// `GET /d/{serial}/api/infra/discover/progress` — poll scan progress.
async fn proxy_infra_discover_progress(
    State(state): State<RelayState>,
    AxumPath(serial): AxumPath<String>,
    request: Request<Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    {
        let devices = state.devices.read().await;
        validate_device_auth(&devices, &serial, auth_header.as_deref())?;
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let response = tunnel_request_json(
        &state,
        &serial,
        json!({
            "type": "tunnel.infra.discover.progress",
            "request_id": request_id,
        }),
        10,
    )
    .await?;
    proxy_response_to_http(&response)
}

/// `POST /d/{serial}/api/infra/discover` — trigger LAN discovery scan.
async fn proxy_infra_discover(
    State(state): State<RelayState>,
    AxumPath(serial): AxumPath<String>,
    request: Request<Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    let body_bytes = axum::body::to_bytes(request.into_body(), 10 * 1024 * 1024)
        .await
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Failed to read request body"})),
            )
        })?;

    let payload: Value = serde_json::from_slice(&body_bytes).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Invalid JSON"})),
        )
    })?;

    {
        let devices = state.devices.read().await;
        validate_device_auth(&devices, &serial, auth_header.as_deref())?;
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let mut msg = payload;
    msg["type"] = json!("tunnel.infra.discover");
    msg["request_id"] = json!(request_id);

    // Discovery can run nmap ping sweep + port probe — allow 120s
    let response = tunnel_request_json(&state, &serial, msg, 120).await?;
    proxy_response_to_http(&response)
}

/// `POST /d/{serial}/api/infra/config` — push monitoring config to device.
async fn proxy_infra_config_push(
    State(state): State<RelayState>,
    AxumPath(serial): AxumPath<String>,
    request: Request<Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    let body_bytes = axum::body::to_bytes(request.into_body(), 10 * 1024 * 1024)
        .await
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Failed to read request body"})),
            )
        })?;

    let payload: Value = serde_json::from_slice(&body_bytes).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Invalid JSON"})),
        )
    })?;

    {
        let devices = state.devices.read().await;
        validate_device_auth(&devices, &serial, auth_header.as_deref())?;
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let mut msg = payload;
    msg["type"] = json!("tunnel.infra.config");
    msg["request_id"] = json!(request_id);

    let response =
        tunnel_request_json(&state, &serial, msg, state.tunnel_proxy_timeout_secs).await?;
    proxy_response_to_http(&response)
}

/// `DELETE /d/{serial}/api/infra/config` — stop monitoring, remove config.
async fn proxy_infra_config_delete(
    State(state): State<RelayState>,
    AxumPath(serial): AxumPath<String>,
    request: Request<Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    {
        let devices = state.devices.read().await;
        validate_device_auth(&devices, &serial, auth_header.as_deref())?;
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let msg = json!({
        "type": "tunnel.infra.config.delete",
        "request_id": request_id,
    });

    let response =
        tunnel_request_json(&state, &serial, msg, state.tunnel_proxy_timeout_secs).await?;
    proxy_response_to_http(&response)
}

/// `POST /d/{serial}/api/infra/check/{target_id}` — on-demand check for one target.
async fn proxy_infra_check(
    State(state): State<RelayState>,
    AxumPath((serial, target_id)): AxumPath<(String, String)>,
    request: Request<Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    {
        let devices = state.devices.read().await;
        validate_device_auth(&devices, &serial, auth_header.as_deref())?;
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let msg = json!({
        "type": "tunnel.infra.check",
        "request_id": request_id,
        "target_id": target_id,
    });

    // Single check — 30s timeout
    let response = tunnel_request_json(&state, &serial, msg, 30).await?;
    proxy_response_to_http(&response)
}

/// `GET /d/{serial}/api/ws?token=<api_key>` — WS proxy to device.
async fn proxy_ws(
    State(state): State<RelayState>,
    AxumPath(serial): AxumPath<String>,
    Query(query): Query<WsProxyQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    // Validate token against device's api_key
    let devices = state.devices.read().await;
    let Some(device) = devices.get(&serial) else {
        return (StatusCode::NOT_FOUND, "Device not connected").into_response();
    };

    if !crate::auth::constant_time_eq(device.api_key.as_bytes(), query.token.as_bytes()) {
        return (StatusCode::FORBIDDEN, "Invalid API key").into_response();
    }

    // Enforce per-device client connection limit
    if device.clients.read().await.len() >= MAX_CLIENTS_PER_DEVICE {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "Too many clients for this device",
        )
            .into_response();
    }

    let device_tx = device.device_tx.clone();
    let clients = device.clients.clone();
    let session_subs = device.session_subscriptions.clone();
    drop(devices);

    ws.on_upgrade(move |socket| {
        let span = info_span!("tunnel_client", serial = %serial);
        handle_client_ws(socket, state, serial, device_tx, clients, session_subs).instrument(span)
    })
}

/// Handle a client's WS connection proxied to a device.
async fn handle_client_ws(
    socket: axum::extract::ws::WebSocket,
    _state: RelayState,
    serial: String,
    device_tx: mpsc::Sender<TunnelMessage>,
    clients: Arc<RwLock<HashMap<String, mpsc::Sender<Value>>>>,
    session_subs: Arc<RwLock<HashMap<String, HashSet<String>>>>,
) {
    let (mut ws_sink, mut ws_stream) = socket.split();
    let client_id = uuid::Uuid::new_v4().to_string();
    let (client_tx, mut client_rx) = mpsc::channel::<Value>(256);

    // Register this client
    clients.write().await.insert(client_id.clone(), client_tx);

    info!(client_id = %client_id, serial = %serial, "Client connected to device");

    // Forward client_rx messages to WS sink
    let send_task = tokio::spawn(async move {
        while let Some(msg) = client_rx.recv().await {
            let text = match serde_json::to_string(&msg) {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!("Client WS serialize failed: {e}");
                    continue;
                }
            };
            if ws_sink
                .send(axum::extract::ws::Message::Text(text.into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // Process messages from the client
    while let Some(Ok(msg)) = ws_stream.next().await {
        match msg {
            axum::extract::ws::Message::Text(text) => {
                let Ok(mut parsed) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };

                let msg_type = parsed["type"].as_str().unwrap_or("").to_string();

                let original_rid = parsed["request_id"].as_str().unwrap_or("").to_string();

                // Tag request_id with client_id for routing responses back
                let tagged_rid = format!("{client_id}:{original_rid}");
                parsed["request_id"] = json!(tagged_rid);
                info!(
                    serial = %serial,
                    client_id = %client_id,
                    msg_type = %msg_type,
                    request_id = %original_rid,
                    session_id = parsed["session_id"].as_str().unwrap_or(""),
                    "Relay WS client request forwarded to device"
                );

                // Track session subscriptions for output routing
                match msg_type.as_str() {
                    "session.attach" => {
                        if let Some(sid) = parsed["session_id"].as_str() {
                            session_subs
                                .write()
                                .await
                                .entry(sid.to_string())
                                .or_default()
                                .insert(client_id.clone());
                        }
                    }
                    "session.kill" => {
                        if let Some(sid) = parsed["session_id"].as_str() {
                            session_subs
                                .write()
                                .await
                                .entry(sid.to_string())
                                .or_default()
                                .remove(&client_id);
                        }
                    }
                    _ => {}
                }

                // Forward to device
                match tokio::time::timeout(
                    Duration::from_secs(DEVICE_QUEUE_SEND_TIMEOUT_SECS),
                    device_tx.send(TunnelMessage::Text(parsed)),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) => break, // Device disconnected
                    Err(_) => {
                        warn!(
                            serial = %serial,
                            client_id = %client_id,
                            msg_type = %msg_type,
                            "Relay WS client forward timed out waiting for device queue"
                        );
                        break;
                    }
                }
            }
            axum::extract::ws::Message::Close(_) => break,
            _ => {}
        }
    }

    // Client disconnected — cleanup
    info!(client_id = %client_id, serial = %serial, "Client disconnected from device");

    // Remove from clients map
    clients.write().await.remove(&client_id);

    // Collect sessions this client was subscribed to, then remove from subscriptions
    let detach_sessions: Vec<String>;
    {
        let mut subs = session_subs.write().await;
        detach_sessions = subs
            .iter()
            .filter(|(_, ids)| ids.contains(&client_id))
            .map(|(sid, _)| sid.clone())
            .collect();
        for (_, client_ids) in subs.iter_mut() {
            client_ids.remove(&client_id);
        }
        // Remove empty subscription sets
        subs.retain(|_, v| !v.is_empty());
    }

    // Tell the device to detach sessions that no longer have any subscribers.
    // After `retain` above, sessions with zero subscribers were removed from the
    // map entirely, so `get()` returns None. `map_or(true, ...)` means:
    //   None (removed = no subscribers left) → true → detach
    //   Some(non-empty set) → false → other clients still watching, keep alive
    for session_id in &detach_sessions {
        let should_detach = session_subs
            .read()
            .await
            .get(session_id)
            .is_none_or(HashSet::is_empty);
        if should_detach {
            match tokio::time::timeout(
                Duration::from_secs(DEVICE_QUEUE_SEND_TIMEOUT_SECS),
                device_tx.send(TunnelMessage::Text(json!({
                    "type": "session.detach",
                    "session_id": session_id,
                }))),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(_)) => {
                    warn!(
                        serial = %serial,
                        client_id = %client_id,
                        session_id = %session_id,
                        "Relay WS detach failed: device disconnected"
                    );
                }
                Err(_) => {
                    warn!(
                        serial = %serial,
                        client_id = %client_id,
                        session_id = %session_id,
                        "Relay WS detach timed out waiting for device queue"
                    );
                }
            }
        }
    }

    send_task.abort();
}

// ─── STP (gawdxfer) Proxy Endpoints ──────────────────────────────────────────

/// `POST /d/{serial}/api/stp/download` — proxied download init.
async fn proxy_stp_download_init(
    State(state): State<RelayState>,
    AxumPath(serial): AxumPath<String>,
    request: Request<Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    let body_bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024)
        .await
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Failed to read request body"})),
            )
        })?;

    let payload: Value = serde_json::from_slice(&body_bytes).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Invalid JSON"})),
        )
    })?;

    {
        let devices = state.devices.read().await;
        validate_device_auth(&devices, &serial, auth_header.as_deref())?;
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let msg = json!({
        "type": "gx.download.init",
        "request_id": request_id,
        "path": payload["path"],
        "chunk_size": payload["chunk_size"],
    });

    let response = tunnel_request_json(&state, &serial, msg, 30).await?;
    proxy_response_to_http(&response)
}

/// `POST /d/{serial}/api/stp/upload` — proxied upload init.
async fn proxy_stp_upload_init(
    State(state): State<RelayState>,
    AxumPath(serial): AxumPath<String>,
    request: Request<Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    let body_bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024)
        .await
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Failed to read request body"})),
            )
        })?;

    let payload: Value = serde_json::from_slice(&body_bytes).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Invalid JSON"})),
        )
    })?;

    {
        let devices = state.devices.read().await;
        validate_device_auth(&devices, &serial, auth_header.as_deref())?;
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let mut msg = payload;
    msg["type"] = json!("gx.upload.init");
    msg["request_id"] = json!(request_id);

    let response = tunnel_request_json(&state, &serial, msg, 30).await?;
    proxy_response_to_http(&response)
}

/// `GET /d/{serial}/api/stp/chunk/{xfer}/{idx}` — proxy download chunk.
async fn proxy_stp_download_chunk(
    State(state): State<RelayState>,
    AxumPath((serial, xfer, idx)): AxumPath<(String, String, u32)>,
    request: Request<Body>,
) -> Result<Response, (StatusCode, Json<Value>)> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    {
        let devices = state.devices.read().await;
        validate_device_auth(&devices, &serial, auth_header.as_deref())?;
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let msg = json!({
        "type": "gx.chunk.request",
        "request_id": request_id,
        "transfer_id": xfer,
        "chunk_index": idx,
    });

    let response = tunnel_request(&state, &serial, msg, 60).await?;

    match response {
        TunnelResponse::Binary { header, data } => {
            let chunk_hash = header["chunk_hash"].as_str().unwrap_or("");
            #[allow(clippy::cast_possible_truncation)]
            let chunk_index = header["chunk_index"].as_u64().unwrap_or(0) as u32;
            let transfer_id = header["transfer_id"].as_str().unwrap_or("");

            Ok(Response::builder()
                .header("Content-Type", "application/octet-stream")
                .header("X-Gx-Chunk-Hash", chunk_hash)
                .header("X-Gx-Chunk-Index", chunk_index.to_string())
                .header("X-Gx-Transfer-Id", transfer_id)
                .header("Content-Length", data.len())
                .body(Body::from(data))
                .unwrap())
        }
        TunnelResponse::Json(v) => {
            let status = v["status"].as_u64().unwrap_or(500);
            let body = v["body"].clone();
            #[allow(clippy::cast_possible_truncation)]
            Err((
                StatusCode::from_u16(status as u16).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(body),
            ))
        }
    }
}

/// `POST /d/{serial}/api/stp/chunk/{xfer}/{idx}` — proxy upload chunk.
async fn proxy_stp_upload_chunk(
    State(state): State<RelayState>,
    AxumPath((serial, xfer, idx)): AxumPath<(String, String, u32)>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let auth_header = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    {
        let devices = state.devices.read().await;
        validate_device_auth(&devices, &serial, auth_header.as_deref())?;
    }

    let chunk_hash = headers
        .get("X-Gx-Chunk-Hash")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    if chunk_hash.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Missing X-Gx-Chunk-Hash header", "code": "INVALID_REQUEST"})),
        ));
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let header = json!({
        "type": "gx.chunk",
        "request_id": request_id,
        "transfer_id": xfer,
        "chunk_index": idx,
        "chunk_hash": chunk_hash,
    });
    let frame = encode_binary_frame(&header, &body);

    let response = tunnel_request_binary(
        &state,
        &serial,
        TunnelMessage::Binary(frame),
        &request_id,
        60,
    )
    .await?;

    match response {
        TunnelResponse::Json(v) => {
            let status = v["status"].as_u64().unwrap_or(200);
            if status >= 400 {
                let body = v["body"].clone();
                #[allow(clippy::cast_possible_truncation)]
                return Err((
                    StatusCode::from_u16(status as u16)
                        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                    Json(body),
                ));
            }
            let body = v.get("body").cloned().unwrap_or(json!({"ok": true}));
            Ok(Json(body))
        }
        TunnelResponse::Binary { .. } => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "Unexpected binary response for chunk upload"})),
        )),
    }
}

/// `POST /d/{serial}/api/stp/resume/{xfer}` — proxied resume.
async fn proxy_stp_resume(
    State(state): State<RelayState>,
    AxumPath((serial, xfer)): AxumPath<(String, String)>,
    request: Request<Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    {
        let devices = state.devices.read().await;
        validate_device_auth(&devices, &serial, auth_header.as_deref())?;
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let msg = json!({
        "type": "gx.resume",
        "request_id": request_id,
        "transfer_id": xfer,
    });

    let response =
        tunnel_request_json(&state, &serial, msg, state.tunnel_proxy_timeout_secs).await?;
    proxy_response_to_http(&response)
}

/// `GET /d/{serial}/api/stp/status/{xfer}` — proxied status.
async fn proxy_stp_status(
    State(state): State<RelayState>,
    AxumPath((serial, xfer)): AxumPath<(String, String)>,
    request: Request<Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    {
        let devices = state.devices.read().await;
        validate_device_auth(&devices, &serial, auth_header.as_deref())?;
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let msg = json!({
        "type": "gx.status",
        "request_id": request_id,
        "transfer_id": xfer,
    });

    let response =
        tunnel_request_json(&state, &serial, msg, state.tunnel_proxy_timeout_secs).await?;
    proxy_response_to_http(&response)
}

/// `GET /d/{serial}/api/stp/transfers` — proxied transfer list.
async fn proxy_stp_list(
    State(state): State<RelayState>,
    AxumPath(serial): AxumPath<String>,
    request: Request<Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    {
        let devices = state.devices.read().await;
        validate_device_auth(&devices, &serial, auth_header.as_deref())?;
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let msg = json!({
        "type": "gx.list",
        "request_id": request_id,
    });

    let response =
        tunnel_request_json(&state, &serial, msg, state.tunnel_proxy_timeout_secs).await?;
    proxy_response_to_http(&response)
}

/// `DELETE /d/{serial}/api/stp/{xfer}` — proxied abort.
async fn proxy_stp_abort(
    State(state): State<RelayState>,
    AxumPath((serial, xfer)): AxumPath<(String, String)>,
    request: Request<Body>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    {
        let devices = state.devices.read().await;
        validate_device_auth(&devices, &serial, auth_header.as_deref())?;
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let msg = json!({
        "type": "gx.abort",
        "request_id": request_id,
        "transfer_id": xfer,
        "reason": "client abort",
    });

    let response =
        tunnel_request_json(&state, &serial, msg, state.tunnel_proxy_timeout_secs).await?;
    proxy_response_to_http(&response)
}
