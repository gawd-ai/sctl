//! Tunnel relay — accepts device registrations and proxies client requests.
//!
//! When `tunnel.relay = true`, the relay:
//! 1. Listens for device WS connections at `/api/tunnel/register`
//! 2. Exposes REST + WS proxy at `/d/{serial}/api/*`
//! 3. Translates client requests to tunnel messages over the device WS

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    body::Body,
    extract::{Path as AxumPath, Query, State, WebSocketUpgrade},
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, Mutex, RwLock};
use tracing::{info, info_span, warn, Instrument};

/// State shared across all relay handlers.
#[derive(Clone)]
pub struct RelayState {
    /// Connected devices keyed by serial number.
    pub devices: Arc<RwLock<HashMap<String, ConnectedDevice>>>,
    /// The shared tunnel key for device registration auth.
    pub tunnel_key: String,
    /// Seconds before a device is evicted for missed heartbeat (default 90).
    pub heartbeat_timeout_secs: u64,
    /// Default proxy request timeout in seconds (default 60).
    pub tunnel_proxy_timeout_secs: u64,
}

/// A device connected to the relay via its outbound WS tunnel.
pub struct ConnectedDevice {
    pub serial: String,
    pub api_key: String,
    /// Send messages to the device over the tunnel WS.
    pub device_tx: mpsc::Sender<Value>,
    /// Pending REST-over-WS requests awaiting responses, keyed by `request_id`.
    pub pending_requests: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>,
    /// Connected WS clients, keyed by `client_id`.
    pub clients: Arc<RwLock<HashMap<String, mpsc::Sender<Value>>>>,
    /// Session subscriptions: `session_id` -> set of `client_ids` watching output.
    pub session_subscriptions: Arc<RwLock<HashMap<String, HashSet<String>>>>,
    /// Last heartbeat from device.
    pub last_heartbeat: Arc<Mutex<Instant>>,
    /// When this device connected.
    pub connected_since: Instant,
    /// Count of messages dropped due to client backpressure.
    pub dropped_messages: Arc<AtomicU64>,
}

/// Drain all pending requests for a device, sending error responses on each oneshot.
/// Also notifies all connected WS clients that the device disconnected.
async fn drain_device(device: &ConnectedDevice, reason: &str) {
    // Drain pending REST-over-WS requests
    let mut pending = device.pending_requests.lock().await;
    let count = pending.len();
    for (_, sender) in pending.drain() {
        let _ = sender.send(json!({
            "type": "error",
            "status": 502,
            "body": {"error": reason, "code": "DEVICE_DISCONNECTED"},
        }));
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
    ) -> Self {
        Self {
            devices: Arc::new(RwLock::new(HashMap::new())),
            tunnel_key,
            heartbeat_timeout_secs,
            tunnel_proxy_timeout_secs,
        }
    }

    /// Evict devices whose heartbeat is older than `heartbeat_timeout_secs`.
    /// Returns the serials of evicted devices.
    pub async fn sweep_dead_devices(&self) -> Vec<String> {
        let timeout = Duration::from_secs(self.heartbeat_timeout_secs);
        let now = Instant::now();

        // First pass: identify dead devices and drain them (read lock)
        let mut dead_serials = Vec::new();
        {
            let devices = self.devices.read().await;
            for (serial, device) in devices.iter() {
                let last_hb = *device.last_heartbeat.lock().await;
                if now.duration_since(last_hb) > timeout {
                    drain_device(device, "heartbeat timeout").await;
                    dead_serials.push(serial.clone());
                }
            }
        }

        // Second pass: remove dead devices (write lock)
        if !dead_serials.is_empty() {
            let mut devices = self.devices.write().await;
            for serial in &dead_serials {
                devices.remove(serial);
                warn!(serial = %serial, "Evicted device (heartbeat timeout)");
            }
        }

        dead_serials
    }

    /// Send a message to all connected devices (e.g., for relay shutdown).
    pub async fn broadcast_to_devices(&self, msg: Value) {
        let devices = self.devices.read().await;
        for (serial, device) in devices.iter() {
            if device.device_tx.send(msg.clone()).await.is_err() {
                warn!(serial = %serial, "Failed to send broadcast to device");
            }
        }
    }

    /// Drain all devices and clear state (used during relay shutdown).
    pub async fn drain_all(&self) {
        let mut devices = self.devices.write().await;
        for (serial, device) in devices.iter() {
            drain_device(device, "relay shutting down").await;
            info!(serial = %serial, "Drained device for relay shutdown");
        }
        devices.clear();
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
        .route("/d/{serial}/api/exec", post(proxy_exec))
        .route("/d/{serial}/api/exec/batch", post(proxy_exec_batch))
        .route(
            "/d/{serial}/api/files",
            get(proxy_file_read).put(proxy_file_write),
        )
        .route("/d/{serial}/api/activity", get(proxy_activity))
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

/// `GET /api/tunnel/register?token=<tunnel_key>&serial=<serial>` — device WS registration.
async fn device_register_ws(
    State(state): State<RelayState>,
    Query(query): Query<RegisterQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    if !crate::auth::constant_time_eq(state.tunnel_key.as_bytes(), query.token.as_bytes()) {
        return (StatusCode::FORBIDDEN, "Invalid tunnel key").into_response();
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
    let (device_tx, mut device_rx) = mpsc::channel::<Value>(256);

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

    let device = ConnectedDevice {
        serial: serial.clone(),
        api_key,
        device_tx: device_tx.clone(),
        pending_requests: Arc::new(Mutex::new(HashMap::new())),
        clients: Arc::new(RwLock::new(HashMap::new())),
        session_subscriptions: Arc::new(RwLock::new(HashMap::new())),
        last_heartbeat: Arc::new(Mutex::new(Instant::now())),
        connected_since: Instant::now(),
        dropped_messages: Arc::new(AtomicU64::new(0)),
    };

    let pending_requests = device.pending_requests.clone();
    let clients = device.clients.clone();
    let session_subs = device.session_subscriptions.clone();
    let heartbeat = device.last_heartbeat.clone();
    let dropped_messages = device.dropped_messages.clone();

    // Handle duplicate serial: drain stale connection before replacing
    {
        let mut devices = state.devices.write().await;
        if let Some(old_device) = devices.get(&serial) {
            warn!(
                serial = %serial,
                "Device re-registering while stale connection exists, evicting old"
            );
            drain_device(old_device, "replaced by new connection").await;
        }
        devices.insert(serial.clone(), device);
    }
    info!(serial = %serial, "Device registered");

    // Send ack
    let ack = json!({"type": "tunnel.register.ack", "serial": &serial});
    let _ = ws_sink
        .send(axum::extract::ws::Message::Text(
            serde_json::to_string(&ack).unwrap().into(),
        ))
        .await;

    // Forward device_tx messages to the WS sink
    let send_task = tokio::spawn(async move {
        while let Some(msg) = device_rx.recv().await {
            let text = serde_json::to_string(&msg).expect("Value serializes");
            if ws_sink
                .send(axum::extract::ws::Message::Text(text.into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // Process messages from the device
    while let Some(Ok(msg)) = ws_stream.next().await {
        match msg {
            axum::extract::ws::Message::Text(text) => {
                let Ok(parsed) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                let msg_type = parsed["type"].as_str().unwrap_or("");

                match msg_type {
                    "tunnel.ping" => {
                        // Device heartbeat — respond and update timestamp
                        *heartbeat.lock().await = Instant::now();
                        let _ = device_tx.send(json!({"type": "tunnel.pong"})).await;
                    }
                    t if t.ends_with(".result") => {
                        // Response to a REST-over-WS request
                        if let Some(request_id) = parsed["request_id"].as_str() {
                            // Check if this is a client-tagged request (contains ':')
                            if let Some(colon_pos) = request_id.find(':') {
                                let client_id = &request_id[..colon_pos];
                                let original_rid = &request_id[colon_pos + 1..];

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
                                    let _ = sender.send(parsed);
                                }
                            }
                        }
                    }
                    // Session output messages — route to subscribed clients (backpressure-aware)
                    "session.stdout" | "session.stderr" | "session.system" => {
                        if let Some(session_id) = parsed["session_id"].as_str() {
                            let subs = session_subs.read().await;
                            if let Some(client_ids) = subs.get(session_id) {
                                let clients_read = clients.read().await;
                                for cid in client_ids {
                                    if let Some(client_tx) = clients_read.get(cid) {
                                        // Restore original request_id if tagged
                                        let mut msg = parsed.clone();
                                        if let Some(rid) = msg["request_id"].as_str() {
                                            if let Some(colon_pos) = rid.find(':') {
                                                msg["request_id"] = json!(&rid[colon_pos + 1..]);
                                            }
                                        }
                                        if client_tx.try_send(msg).is_err() {
                                            dropped_messages.fetch_add(1, Ordering::Relaxed);
                                            warn!(
                                                serial = %serial,
                                                client_id = %cid,
                                                "Dropped session output message (client backpressure)"
                                            );
                                        }
                                    }
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
                    | "error" => {
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
                    _ => {
                        warn!(serial = %serial, msg_type, "Unknown message from device");
                    }
                }
            }
            axum::extract::ws::Message::Close(_) => break,
            _ => {}
        }
    }

    // Device disconnected — drain pending requests and notify clients before removing
    {
        let devices = state.devices.read().await;
        if let Some(device) = devices.get(&serial) {
            drain_device(device, "device disconnected").await;
        }
    }
    info!(serial = %serial, "Device disconnected");
    state.devices.write().await.remove(&serial);
    send_task.abort();
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

    let now = Instant::now();
    let devices = state.devices.read().await;
    let mut list: Vec<Value> = Vec::with_capacity(devices.len());

    for d in devices.values() {
        let last_hb = *d.last_heartbeat.lock().await;
        #[allow(clippy::cast_possible_truncation)]
        let hb_ago_ms = now.duration_since(last_hb).as_millis() as u64;
        let pending_count = d.pending_requests.lock().await.len();
        let clients_read = d.clients.read().await;
        let client_ids: Vec<&String> = clients_read.keys().collect();
        let subs = d.session_subscriptions.read().await;
        let subs_map: HashMap<&String, Vec<&String>> = subs
            .iter()
            .map(|(sid, cids)| (sid, cids.iter().collect()))
            .collect();
        #[allow(clippy::cast_possible_truncation)]
        let connected_ms = now.duration_since(d.connected_since).as_millis() as u64;

        list.push(json!({
            "serial": d.serial,
            "clients": client_ids,
            "client_count": client_ids.len(),
            "last_heartbeat_ago_ms": hb_ago_ms,
            "pending_requests_count": pending_count,
            "session_subscriptions": subs_map,
            "connected_since_ms": connected_ms,
            "dropped_messages": d.dropped_messages.load(Ordering::Relaxed),
        }));
    }

    Json(json!({"devices": list})).into_response()
}

// ─── REST Proxy Helpers ──────────────────────────────────────────────────────

/// Send a tunnel request to a device and await the response.
async fn tunnel_request(
    state: &RelayState,
    serial: &str,
    msg: Value,
    timeout_secs: u64,
) -> Result<Value, (StatusCode, Json<Value>)> {
    let devices = state.devices.read().await;
    let device = devices.get(serial).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("Device '{serial}' not connected"), "code": "DEVICE_NOT_FOUND"})),
        )
    })?;

    let request_id = msg["request_id"].as_str().unwrap_or("").to_string();

    let (tx, rx) = oneshot::channel();
    device
        .pending_requests
        .lock()
        .await
        .insert(request_id.clone(), tx);

    if device.device_tx.send(msg).await.is_err() {
        device.pending_requests.lock().await.remove(&request_id);
        return Err((
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": "Failed to send to device", "code": "DEVICE_SEND_FAILED"})),
        ));
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
            // Timeout — remove pending request
            if let Some(device) = state.devices.read().await.get(serial) {
                device.pending_requests.lock().await.remove(&request_id);
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

    let response = tunnel_request(&state, &serial, msg, 10).await?;
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
async fn proxy_info(
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
        "type": "tunnel.info",
        "request_id": request_id,
    });

    let response = tunnel_request(&state, &serial, msg, state.tunnel_proxy_timeout_secs).await?;
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

    let response = tunnel_request(&state, &serial, msg, timeout_secs).await?;
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

    let response = tunnel_request(&state, &serial, msg, timeout_secs).await?;
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

    {
        let devices = state.devices.read().await;
        validate_device_auth(&devices, &serial, auth_header.as_deref())?;
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let msg = json!({
        "type": "tunnel.file.read",
        "request_id": request_id,
        "path": query.path,
        "list": query.list,
    });

    let response = tunnel_request(&state, &serial, msg, state.tunnel_proxy_timeout_secs).await?;
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

    let response = tunnel_request(&state, &serial, msg, state.tunnel_proxy_timeout_secs).await?;
    proxy_response_to_http(&response)
}

/// Convert a tunnel response (with status + body) to an HTTP response.
fn proxy_response_to_http(response: &Value) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
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

    let response = tunnel_request(&state, &serial, msg, state.tunnel_proxy_timeout_secs).await?;
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

// ─── WS Proxy ────────────────────────────────────────────────────────────────

/// Query params for client WS proxy.
#[derive(Deserialize)]
struct WsProxyQuery {
    token: String,
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
    device_tx: mpsc::Sender<Value>,
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
            let text = serde_json::to_string(&msg).expect("Value serializes");
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
                if device_tx.send(parsed).await.is_err() {
                    break; // Device disconnected
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

    // Remove from all session subscriptions
    let mut subs = session_subs.write().await;
    for (_, client_ids) in subs.iter_mut() {
        client_ids.remove(&client_id);
    }
    // Remove empty subscription sets
    subs.retain(|_, v| !v.is_empty());
    drop(subs);

    send_task.abort();
}
