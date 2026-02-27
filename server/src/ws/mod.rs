//! WebSocket transport for interactive shell sessions.
//!
//! ## Connection lifecycle
//!
//! 1. Client connects to `GET /api/ws?token=<api_key>` — token is validated
//!    before the upgrade completes.
//! 2. All messages are JSON objects with a `"type"` field. An optional
//!    `"request_id"` on any incoming message is echoed on the corresponding
//!    response(s), enabling correlation in async/multiplexed clients.
//! 3. On disconnect, non-persistent sessions are killed and persistent
//!    sessions are detached (output keeps buffering for later re-attach).
//!
//! ## Message types (client → server)
//!
//! | Type              | Fields                                                        | Response type(s)                |
//! |-------------------|---------------------------------------------------------------|---------------------------------|
//! | `ping`            | —                                                             | `pong`                          |
//! | `session.start`   | `working_dir?`, `persistent?`, `env?`, `shell?`, `pty?`, `rows?`, `cols?`, `idle_timeout?` | `session.started` or `error` |
//! | `session.exec`    | `session_id`, `command`                                       | `session.exec.ack` or `error`   |
//! | `session.stdin`   | `session_id`, `data`                                          | (none on success, `error` on failure) |
//! | `session.kill`    | `session_id`                                                  | `session.closed` or `error`     |
//! | `session.signal`  | `session_id`, `signal`                                        | `session.signal.ack` or `error` |
//! | `session.attach`  | `session_id`, `since?`                                        | `session.attached` or `error`   |
//! | `session.resize`  | `session_id`, `rows`, `cols`                                  | `session.resize.ack` or `error` |
//! | `session.list`    | —                                                             | `session.listed`                |
//! | `session.allow_ai`    | `session_id`, `allowed` (bool)                                | `session.allow_ai.ack` + broadcast `session.ai_permission_changed` |
//! | `session.ai_status`   | `session_id`, `working` (bool), `activity?`, `message?`       | `session.ai_status.ack` + broadcast `session.ai_status_changed` |
//! | `shell.list`      | —                                                             | `shell.listed`                  |
//!
//! ## Message types (server → client)
//!
//! | Type                 | Key fields                            |
//! |----------------------|---------------------------------------|
//! | `pong`               | —                                     |
//! | `session.started`    | `session_id`, `pid`, `pty`            |
//! | `session.exec.ack`   | `session_id`                          |
//! | `session.stdout`     | `session_id`, `data`, `seq`           |
//! | `session.stderr`     | `session_id`, `data`, `seq`           |
//! | `session.system`     | `session_id`, `data`, `seq`           |
//! | `session.exited`     | `session_id`, `exit_code`             |
//! | `session.closed`     | `session_id`, `reason`                |
//! | `session.signal.ack` | `session_id`                          |
//! | `session.attached`   | `session_id`, `entries[]`             |
//! | `session.resize.ack` | `session_id`, `rows`, `cols`          |
//! | `session.listed`     | `sessions[]` (incl. `status`, `idle`) |
//! | `shell.listed`       | `shells[]`, `default_shell`           |
//! | `error`              | `code`, `message`, `session_id?`      |

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Query, State, WebSocketUpgrade},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{mpsc, Mutex};
use tracing::{error, info};

use crate::activity::{ActivitySource, ActivityType};
use crate::sessions::buffer::{OutputBuffer, OutputEntry, OutputStream};
use crate::AppState;

/// Query parameters for the WebSocket upgrade request.
#[derive(Deserialize)]
pub struct WsQuery {
    /// API key passed as a query parameter (since HTTP headers aren't available
    /// during a browser WebSocket upgrade).
    pub token: String,
}

/// `GET /api/ws?token=<key>` — WebSocket upgrade handler.
///
/// Validates the token before upgrading. Returns `403 Forbidden` on auth
/// failure.
pub async fn ws_upgrade(
    State(state): State<AppState>,
    Query(query): Query<WsQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    if !crate::auth::constant_time_eq(state.config.auth.api_key.as_bytes(), query.token.as_bytes())
    {
        return (StatusCode::FORBIDDEN, "Forbidden").into_response();
    }

    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

/// Convert an [`OutputEntry`] to a WebSocket JSON message.
fn entry_to_ws_message(session_id: &str, entry: &OutputEntry) -> Value {
    match entry.stream {
        OutputStream::Stdout | OutputStream::Stderr | OutputStream::System => {
            json!({
                "type": format!("session.{}", entry.stream.as_str()),
                "session_id": session_id,
                "data": entry.data,
                "seq": entry.seq,
                "timestamp_ms": entry.timestamp_ms,
            })
        }
    }
}

/// Background task that reads from a session's [`OutputBuffer`] and forwards
/// entries as WebSocket messages. Dies when the WS sender closes.
async fn subscriber_task(
    session_id: String,
    buffer: Arc<Mutex<OutputBuffer>>,
    ws_tx: mpsc::Sender<Value>,
    since: u64,
) {
    let mut cursor = since;
    loop {
        let (entries, notify) = {
            let buf = buffer.lock().await;
            if buf.has_entries_since(cursor) {
                let (entries, _dropped) = buf.read_since(cursor);
                (entries, None)
            } else {
                (vec![], Some(buf.notifier()))
            }
        };
        for entry in &entries {
            let msg = entry_to_ws_message(&session_id, entry);
            if ws_tx.send(msg).await.is_err() {
                return; // WS closed
            }
            cursor = entry.seq;
        }
        if let Some(n) = notify {
            n.notified().await;
        }
    }
}

/// Main WebSocket event loop.
///
/// Splits the socket into a sink (outgoing) and stream (incoming). Outgoing
/// messages are funneled through an mpsc channel so session I/O tasks can send
/// without holding a reference to the socket.
///
/// Uses `tokio::select!` to concurrently process:
/// - Incoming WebSocket messages from the client
/// - Broadcast events (session lifecycle) from other connections
#[allow(clippy::too_many_lines)]
async fn handle_ws(socket: axum::extract::ws::WebSocket, state: AppState) {
    let (mut ws_sink, mut ws_stream) = socket.split();

    // Channel for sending messages back to the WebSocket
    let (tx, mut rx) = mpsc::channel::<Value>(256);

    // Subscribe to session lifecycle broadcasts
    let mut broadcast_rx = state.session_events.subscribe();

    // Log WS connect
    state
        .activity_log
        .log(
            ActivityType::WsConnect,
            ActivitySource::Ws,
            "Client connected".to_string(),
            None,
            None,
        )
        .await;

    // Track sessions created by this connection for cleanup on disconnect
    let mut connection_sessions: Vec<String> = Vec::new();

    // Track subscriber tasks so they can be aborted on disconnect
    let mut subscriber_tasks: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();

    // Task: forward channel messages to WebSocket sink
    let send_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let text = match serde_json::to_string(&msg) {
                Ok(t) => t,
                Err(e) => {
                    error!("WS send: failed to serialize message: {e}");
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

    // Process incoming messages and broadcast events concurrently
    loop {
        tokio::select! {
            ws_msg = ws_stream.next() => {
                let Some(Ok(msg)) = ws_msg else { break };
                match msg {
                    axum::extract::ws::Message::Text(text) => {
                        let Ok(parsed) = serde_json::from_str::<Value>(&text) else {
                            let _ = tx
                                .send(json!({
                                    "type": "error",
                                    "code": "INVALID_JSON",
                                    "message": "Failed to parse JSON message"
                                }))
                                .await;
                            continue;
                        };

                        let msg_type = parsed["type"].as_str().unwrap_or("");
                        let request_id = parsed["request_id"].as_str().map(ToString::to_string);

                        match msg_type {
                            "ping" => {
                                let mut resp = json!({"type": "pong"});
                                if let Some(ref rid) = request_id {
                                    resp["request_id"] = json!(rid);
                                }
                                let _ = tx.send(resp).await;
                            }
                            "session.start" => {
                                let working_dir = parsed["working_dir"].as_str().map(ToString::to_string);
                                let persistent = parsed["persistent"].as_bool().unwrap_or(false);
                                let env: Option<HashMap<String, String>> = parsed
                                    .get("env")
                                    .and_then(|v| serde_json::from_value(v.clone()).ok());
                                let shell = parsed["shell"].as_str().map(ToString::to_string);
                                let use_pty = parsed["pty"].as_bool().unwrap_or(false);
                                let name = parsed["name"].as_str().map(ToString::to_string);
                                let user_allows_ai = parsed["user_allows_ai"].as_bool();
                                #[allow(clippy::cast_possible_truncation)]
                                let rows = parsed["rows"]
                                    .as_u64()
                                    .unwrap_or(u64::from(state.config.server.default_terminal_rows))
                                    as u16;
                                #[allow(clippy::cast_possible_truncation)]
                                let cols = parsed["cols"]
                                    .as_u64()
                                    .unwrap_or(u64::from(state.config.server.default_terminal_cols))
                                    as u16;
                                let idle_timeout = parsed["idle_timeout"].as_u64().unwrap_or(0);

                                if let Some(session_id) = handle_session_start(
                                    &state,
                                    &tx,
                                    request_id.as_deref(),
                                    working_dir.as_deref(),
                                    persistent,
                                    env.as_ref(),
                                    shell.as_deref(),
                                    use_pty,
                                    rows,
                                    cols,
                                    idle_timeout,
                                    name.as_deref(),
                                    user_allows_ai,
                                )
                                .await
                                {
                                    // Spawn subscriber for the new session
                                    if let Some(buffer) =
                                        state.session_manager.get_buffer(&session_id).await
                                    {
                                        let task = tokio::spawn(subscriber_task(
                                            session_id.clone(),
                                            buffer,
                                            tx.clone(),
                                            0,
                                        ));
                                        subscriber_tasks.insert(session_id.clone(), task);
                                    }
                                    connection_sessions.push(session_id);
                                }
                            }
                            "session.exec" => {
                                let session_id = parsed["session_id"].as_str().unwrap_or("");
                                let command = parsed["command"].as_str().unwrap_or("");
                                if session_id.is_empty() || command.is_empty() {
                                    let mut resp = json!({
                                        "type": "error",
                                        "code": "MISSING_FIELD",
                                        "message": "session_id and command are required"
                                    });
                                    if let Some(ref rid) = request_id {
                                        resp["request_id"] = json!(rid);
                                    }
                                    let _ = tx.send(resp).await;
                                    continue;
                                }
                                state.session_manager.touch_ai_activity(session_id).await;
                                handle_session_exec(
                                    &state,
                                    &tx,
                                    session_id,
                                    command,
                                    request_id.as_deref(),
                                )
                                .await;
                            }
                            "session.stdin" => {
                                let session_id = parsed["session_id"].as_str().unwrap_or("");
                                let data = parsed["data"].as_str().unwrap_or("");
                                if !session_id.is_empty() {
                                    state.session_manager.touch_ai_activity(session_id).await;
                                    handle_session_stdin(&state, &tx, session_id, data).await;
                                }
                            }
                            "session.kill" => {
                                let session_id = parsed["session_id"].as_str().unwrap_or("");
                                if !session_id.is_empty() {
                                    handle_session_kill(&state, &tx, session_id, request_id.as_deref())
                                        .await;
                                    // Broadcast session.destroyed to all clients
                                    let _ = state.session_events.send(json!({
                                        "type": "session.destroyed",
                                        "session_id": session_id,
                                        "reason": "killed",
                                    }));
                                    connection_sessions.retain(|id| id != session_id);
                                    // Abort the subscriber task
                                    if let Some(task) = subscriber_tasks.remove(session_id) {
                                        task.abort();
                                    }
                                }
                            }
                            "session.signal" => {
                                let session_id = parsed["session_id"].as_str().unwrap_or("");
                                let signal = parsed["signal"].as_i64().unwrap_or(0);
                                if session_id.is_empty() || signal == 0 {
                                    let mut resp = json!({
                                        "type": "error",
                                        "code": "MISSING_FIELD",
                                        "message": "session_id and signal are required"
                                    });
                                    if let Some(ref rid) = request_id {
                                        resp["request_id"] = json!(rid);
                                    }
                                    let _ = tx.send(resp).await;
                                    continue;
                                }
                                #[allow(clippy::cast_possible_truncation)]
                                let signal_i32 = signal as i32;
                                handle_session_signal(
                                    &state,
                                    &tx,
                                    session_id,
                                    signal_i32,
                                    request_id.as_deref(),
                                )
                                .await;
                            }
                            "session.attach" => {
                                let session_id = parsed["session_id"].as_str().unwrap_or("");
                                let since = parsed["since"].as_u64().unwrap_or(0);
                                if session_id.is_empty() {
                                    let mut resp = json!({
                                        "type": "error",
                                        "code": "MISSING_FIELD",
                                        "message": "session_id is required"
                                    });
                                    if let Some(ref rid) = request_id {
                                        resp["request_id"] = json!(rid);
                                    }
                                    let _ = tx.send(resp).await;
                                    continue;
                                }
                                handle_session_attach(
                                    &state,
                                    &tx,
                                    session_id,
                                    since,
                                    request_id.as_deref(),
                                    &mut subscriber_tasks,
                                    &mut connection_sessions,
                                )
                                .await;
                            }
                            "session.list" => {
                                let items = state.session_manager.list_sessions().await;
                                let sessions_json: Vec<Value> = items
                                    .iter()
                                    .map(|s| {
                                        let mut obj = json!({
                                            "session_id": s.session_id,
                                            "pid": s.pid,
                                            "persistent": s.persistent,
                                            "pty": s.pty,
                                            "attached": s.attached,
                                            "status": s.status,
                                            "idle": s.idle,
                                            "idle_timeout": s.idle_timeout,
                                            "created_at": s.created_at,
                                            "user_allows_ai": s.user_allows_ai,
                                            "ai_is_working": s.ai_is_working,
                                        });
                                        if let Some(exit_code) = s.exit_code {
                                            obj["exit_code"] = json!(exit_code);
                                        }
                                        if let Some(ref name) = s.name {
                                            obj["name"] = json!(name);
                                        }
                                        if let Some(ref activity) = s.ai_activity {
                                            obj["ai_activity"] = json!(activity);
                                        }
                                        if let Some(ref msg) = s.ai_status_message {
                                            obj["ai_status_message"] = json!(msg);
                                        }
                                        obj
                                    })
                                    .collect();
                                let mut resp = json!({
                                    "type": "session.listed",
                                    "sessions": sessions_json,
                                });
                                if let Some(ref rid) = request_id {
                                    resp["request_id"] = json!(rid);
                                }
                                let _ = tx.send(resp).await;
                            }
                            "session.resize" => {
                                let session_id = parsed["session_id"].as_str().unwrap_or("");
                                #[allow(clippy::cast_possible_truncation)]
                                let rows = parsed["rows"].as_u64().unwrap_or(0) as u16;
                                #[allow(clippy::cast_possible_truncation)]
                                let cols = parsed["cols"].as_u64().unwrap_or(0) as u16;
                                if session_id.is_empty() || rows == 0 || cols == 0 {
                                    let mut resp = json!({
                                        "type": "error",
                                        "code": "MISSING_FIELD",
                                        "message": "session_id, rows, and cols are required"
                                    });
                                    if let Some(ref rid) = request_id {
                                        resp["request_id"] = json!(rid);
                                    }
                                    let _ = tx.send(resp).await;
                                    continue;
                                }
                                handle_session_resize(
                                    &state,
                                    &tx,
                                    session_id,
                                    rows,
                                    cols,
                                    request_id.as_deref(),
                                )
                                .await;
                            }
                            "session.allow_ai" => {
                                let session_id = parsed["session_id"].as_str().unwrap_or("");
                                let allowed = parsed["allowed"].as_bool();
                                if session_id.is_empty() || allowed.is_none() {
                                    let mut resp = json!({
                                        "type": "error",
                                        "code": "MISSING_FIELD",
                                        "message": "session_id and allowed (bool) are required"
                                    });
                                    if let Some(ref rid) = request_id {
                                        resp["request_id"] = json!(rid);
                                    }
                                    let _ = tx.send(resp).await;
                                    continue;
                                }
                                let allowed = allowed.unwrap();
                                match state.session_manager.set_user_allows_ai(session_id, allowed).await {
                                    Ok(ai_cleared) => {
                                        let mut resp = json!({
                                            "type": "session.allow_ai.ack",
                                            "session_id": session_id,
                                            "allowed": allowed,
                                        });
                                        if let Some(ref rid) = request_id {
                                            resp["request_id"] = json!(rid);
                                        }
                                        let _ = tx.send(resp).await;
                                        // Broadcast permission change
                                        let _ = state.session_events.send(json!({
                                            "type": "session.ai_permission_changed",
                                            "session_id": session_id,
                                            "allowed": allowed,
                                        }));
                                        // If AI state was cleared, also broadcast status change
                                        if ai_cleared {
                                            let _ = state.session_events.send(json!({
                                                "type": "session.ai_status_changed",
                                                "session_id": session_id,
                                                "working": false,
                                            }));
                                        }
                                    }
                                    Err(e) => {
                                        let mut resp = json!({
                                            "type": "error",
                                            "code": "SESSION_NOT_FOUND",
                                            "session_id": session_id,
                                            "message": e,
                                        });
                                        if let Some(ref rid) = request_id {
                                            resp["request_id"] = json!(rid);
                                        }
                                        let _ = tx.send(resp).await;
                                    }
                                }
                            }
                            "session.ai_status" => {
                                let session_id = parsed["session_id"].as_str().unwrap_or("");
                                let working = parsed["working"].as_bool();
                                if session_id.is_empty() || working.is_none() {
                                    let mut resp = json!({
                                        "type": "error",
                                        "code": "MISSING_FIELD",
                                        "message": "session_id and working (bool) are required"
                                    });
                                    if let Some(ref rid) = request_id {
                                        resp["request_id"] = json!(rid);
                                    }
                                    let _ = tx.send(resp).await;
                                    continue;
                                }
                                let working = working.unwrap();
                                let activity = parsed["activity"].as_str();
                                let message = parsed["message"].as_str();
                                match state.session_manager.set_ai_status(session_id, working, activity, message).await {
                                    Ok(()) => {
                                        let mut resp = json!({
                                            "type": "session.ai_status.ack",
                                            "session_id": session_id,
                                            "working": working,
                                        });
                                        if let Some(a) = activity {
                                            resp["activity"] = json!(a);
                                        }
                                        if let Some(m) = message {
                                            resp["message"] = json!(m);
                                        }
                                        if let Some(ref rid) = request_id {
                                            resp["request_id"] = json!(rid);
                                        }
                                        let _ = tx.send(resp).await;
                                        // Broadcast status change
                                        let mut broadcast = json!({
                                            "type": "session.ai_status_changed",
                                            "session_id": session_id,
                                            "working": working,
                                        });
                                        if let Some(a) = activity {
                                            broadcast["activity"] = json!(a);
                                        }
                                        if let Some(m) = message {
                                            broadcast["message"] = json!(m);
                                        }
                                        let _ = state.session_events.send(broadcast);
                                    }
                                    Err(e) => {
                                        let mut resp = json!({
                                            "type": "error",
                                            "code": "AI_NOT_ALLOWED",
                                            "session_id": session_id,
                                            "message": e,
                                        });
                                        if let Some(ref rid) = request_id {
                                            resp["request_id"] = json!(rid);
                                        }
                                        let _ = tx.send(resp).await;
                                    }
                                }
                            }
                            "session.rename" => {
                                let session_id = parsed["session_id"].as_str().unwrap_or("");
                                let name = parsed["name"].as_str().unwrap_or("");
                                if session_id.is_empty() || name.is_empty() {
                                    let mut resp = json!({
                                        "type": "error",
                                        "code": "MISSING_FIELD",
                                        "message": "session_id and name are required"
                                    });
                                    if let Some(ref rid) = request_id {
                                        resp["request_id"] = json!(rid);
                                    }
                                    let _ = tx.send(resp).await;
                                    continue;
                                }
                                match state.session_manager.rename_session(session_id, name).await {
                                    Ok(()) => {
                                        let mut resp = json!({
                                            "type": "session.rename.ack",
                                            "session_id": session_id,
                                            "name": name,
                                        });
                                        if let Some(ref rid) = request_id {
                                            resp["request_id"] = json!(rid);
                                        }
                                        let _ = tx.send(resp).await;
                                        // Broadcast to all clients
                                        let _ = state.session_events.send(json!({
                                            "type": "session.renamed",
                                            "session_id": session_id,
                                            "name": name,
                                        }));
                                    }
                                    Err(e) => {
                                        let mut resp = json!({
                                            "type": "error",
                                            "code": "SESSION_NOT_FOUND",
                                            "session_id": session_id,
                                            "message": e,
                                        });
                                        if let Some(ref rid) = request_id {
                                            resp["request_id"] = json!(rid);
                                        }
                                        let _ = tx.send(resp).await;
                                    }
                                }
                            }
                            "shell.list" => {
                                let shells = crate::shell::detect_shells();
                                let mut resp = json!({
                                    "type": "shell.listed",
                                    "shells": shells,
                                    "default_shell": &state.config.shell.default_shell,
                                });
                                if let Some(ref rid) = request_id {
                                    resp["request_id"] = json!(rid);
                                }
                                let _ = tx.send(resp).await;
                            }
                            _ => {
                                let mut resp = json!({
                                    "type": "error",
                                    "code": "UNKNOWN_TYPE",
                                    "message": format!("Unknown message type: {msg_type}")
                                });
                                if let Some(ref rid) = request_id {
                                    resp["request_id"] = json!(rid);
                                }
                                let _ = tx.send(resp).await;
                            }
                        }
                    }
                    axum::extract::ws::Message::Close(_) => break,
                    _ => {}
                }
            }
            // Forward broadcast events to this WS client
            broadcast_msg = broadcast_rx.recv() => {
                if let Ok(event) = broadcast_msg {
                    let _ = tx.send(event).await;
                }
            }
        }
    }

    // Log WS disconnect
    state
        .activity_log
        .log(
            ActivityType::WsDisconnect,
            ActivitySource::Ws,
            format!(
                "Client disconnected ({} session{})",
                connection_sessions.len(),
                if connection_sessions.len() == 1 {
                    ""
                } else {
                    "s"
                }
            ),
            None,
            None,
        )
        .await;

    // Connection closed — handle cleanup based on persistence
    if !connection_sessions.is_empty() {
        info!(
            "WebSocket disconnected, cleaning up {} session(s)",
            connection_sessions.len()
        );
        for session_id in &connection_sessions {
            if state.session_manager.is_persistent(session_id).await {
                state.session_manager.detach(session_id).await;
            } else {
                state.session_manager.kill_session(session_id).await;
            }
        }
    }
    // Abort all subscriber tasks (they die when tx drops anyway, but be explicit)
    for (_, task) in subscriber_tasks {
        task.abort();
    }
    send_task.abort();
}

/// Handle `session.start` — spawn a new shell session.
///
/// Returns the `session_id` on success (used for connection-scoped cleanup).
#[allow(clippy::too_many_arguments)]
async fn handle_session_start(
    state: &AppState,
    tx: &mpsc::Sender<Value>,
    request_id: Option<&str>,
    working_dir: Option<&str>,
    persistent: bool,
    env: Option<&HashMap<String, String>>,
    shell: Option<&str>,
    use_pty: bool,
    rows: u16,
    cols: u16,
    idle_timeout: u64,
    name: Option<&str>,
    user_allows_ai: Option<bool>,
) -> Option<String> {
    let raw_dir = working_dir.unwrap_or(&state.config.shell.default_working_dir);
    let expanded = crate::util::expand_tilde(raw_dir);
    let dir = expanded.as_ref();
    let sh = shell.unwrap_or(&state.config.shell.default_shell);
    let allows_ai = user_allows_ai.unwrap_or(true);

    match state
        .session_manager
        .create_session_with_pty(
            sh,
            dir,
            env,
            persistent,
            use_pty,
            rows,
            cols,
            idle_timeout,
            name,
        )
        .await
    {
        Ok((session_id, pid)) => {
            // Override default AI permission if explicitly disabled
            if !allows_ai {
                let _ = state
                    .session_manager
                    .set_user_allows_ai(&session_id, false)
                    .await;
            }

            let mut resp = json!({
                "type": "session.started",
                "session_id": session_id,
                "pid": pid,
                "persistent": persistent,
                "pty": use_pty,
                "user_allows_ai": allows_ai,
                "created_at": crate::sessions::journal::now_ms(),
            });
            if let Some(n) = name {
                resp["name"] = json!(n);
            }
            if let Some(rid) = request_id {
                resp["request_id"] = json!(rid);
            }
            let _ = tx.send(resp).await;

            // Broadcast session.created to all connected clients
            let mut broadcast = json!({
                "type": "session.created",
                "session_id": session_id,
                "pid": pid,
                "pty": use_pty,
                "persistent": persistent,
                "user_allows_ai": allows_ai,
            });
            if let Some(n) = name {
                broadcast["name"] = json!(n);
            }
            let _ = state.session_events.send(broadcast);

            state
                .activity_log
                .log(
                    ActivityType::SessionStart,
                    ActivitySource::Ws,
                    format!("session {}", &session_id[..8.min(session_id.len())]),
                    Some(json!({
                        "session_id": session_id,
                        "pty": use_pty,
                        "persistent": persistent,
                    })),
                    None,
                )
                .await;

            Some(session_id)
        }
        Err(e) => {
            let mut resp = json!({
                "type": "error",
                "code": "SESSION_LIMIT",
                "message": e,
            });
            if let Some(rid) = request_id {
                resp["request_id"] = json!(rid);
            }
            let _ = tx.send(resp).await;
            None
        }
    }
}

/// Handle `session.exec` — write a command (plus newline) to session stdin.
async fn handle_session_exec(
    state: &AppState,
    tx: &mpsc::Sender<Value>,
    session_id: &str,
    command: &str,
    request_id: Option<&str>,
) {
    if let Err(e) = state
        .session_manager
        .exec_command(session_id, command)
        .await
    {
        let mut resp = json!({
            "type": "error",
            "code": "SESSION_ERROR",
            "session_id": session_id,
            "message": e,
        });
        if let Some(rid) = request_id {
            resp["request_id"] = json!(rid);
        }
        let _ = tx.send(resp).await;
    } else {
        let mut resp = json!({
            "type": "session.exec.ack",
            "session_id": session_id,
            "command": command,
        });
        if let Some(rid) = request_id {
            resp["request_id"] = json!(rid);
        }
        let _ = tx.send(resp).await;

        state
            .activity_log
            .log(
                ActivityType::SessionExec,
                ActivitySource::Ws,
                crate::activity::truncate_str(command, 80),
                Some(json!({ "session_id": session_id })),
                None,
            )
            .await;
    }
}

/// Handle `session.stdin` — write raw data to session stdin without newline.
async fn handle_session_stdin(
    state: &AppState,
    tx: &mpsc::Sender<Value>,
    session_id: &str,
    data: &str,
) {
    if let Err(e) = state
        .session_manager
        .send_to_session(session_id, data)
        .await
    {
        let _ = tx
            .send(json!({
                "type": "error",
                "code": "SESSION_ERROR",
                "session_id": session_id,
                "message": e,
            }))
            .await;
    }
}

/// Handle `session.kill` — terminate a session and remove it from the manager.
async fn handle_session_kill(
    state: &AppState,
    tx: &mpsc::Sender<Value>,
    session_id: &str,
    request_id: Option<&str>,
) {
    if state.session_manager.kill_session(session_id).await {
        let mut resp = json!({
            "type": "session.closed",
            "session_id": session_id,
            "reason": "killed",
        });
        if let Some(rid) = request_id {
            resp["request_id"] = json!(rid);
        }
        let _ = tx.send(resp).await;

        state
            .activity_log
            .log(
                ActivityType::SessionKill,
                ActivitySource::Ws,
                format!("session {}", &session_id[..8.min(session_id.len())]),
                Some(json!({ "session_id": session_id })),
                None,
            )
            .await;
    } else {
        let mut resp = json!({
            "type": "error",
            "code": "SESSION_NOT_FOUND",
            "session_id": session_id,
            "message": format!("Session {session_id} not found"),
        });
        if let Some(rid) = request_id {
            resp["request_id"] = json!(rid);
        }
        let _ = tx.send(resp).await;
    }
}

/// Handle `session.signal` — send a signal to the session's process group.
async fn handle_session_signal(
    state: &AppState,
    tx: &mpsc::Sender<Value>,
    session_id: &str,
    signal: i32,
    request_id: Option<&str>,
) {
    match state
        .session_manager
        .signal_session(session_id, signal)
        .await
    {
        Ok(()) => {
            let mut resp = json!({
                "type": "session.signal.ack",
                "session_id": session_id,
                "signal": signal,
            });
            if let Some(rid) = request_id {
                resp["request_id"] = json!(rid);
            }
            let _ = tx.send(resp).await;

            state
                .activity_log
                .log(
                    ActivityType::SessionSignal,
                    ActivitySource::Ws,
                    format!(
                        "signal {} → {}",
                        signal,
                        &session_id[..8.min(session_id.len())]
                    ),
                    Some(json!({ "session_id": session_id, "signal": signal })),
                    None,
                )
                .await;
        }
        Err(e) => {
            let mut resp = json!({
                "type": "error",
                "code": "SESSION_ERROR",
                "session_id": session_id,
                "message": e,
            });
            if let Some(rid) = request_id {
                resp["request_id"] = json!(rid);
            }
            let _ = tx.send(resp).await;
        }
    }
}

/// Handle `session.resize` — resize a PTY session's terminal.
async fn handle_session_resize(
    state: &AppState,
    tx: &mpsc::Sender<Value>,
    session_id: &str,
    rows: u16,
    cols: u16,
    request_id: Option<&str>,
) {
    match state
        .session_manager
        .resize_session(session_id, rows, cols)
        .await
    {
        Ok(()) => {
            let mut resp = json!({
                "type": "session.resize.ack",
                "session_id": session_id,
                "rows": rows,
                "cols": cols,
            });
            if let Some(rid) = request_id {
                resp["request_id"] = json!(rid);
            }
            let _ = tx.send(resp).await;
        }
        Err(e) => {
            let mut resp = json!({
                "type": "error",
                "code": "SESSION_ERROR",
                "session_id": session_id,
                "message": e,
            });
            if let Some(rid) = request_id {
                resp["request_id"] = json!(rid);
            }
            let _ = tx.send(resp).await;
        }
    }
}

/// Handle `session.attach` — re-attach to a detached session, replay missed
/// output, and start a subscriber.
async fn handle_session_attach(
    state: &AppState,
    tx: &mpsc::Sender<Value>,
    session_id: &str,
    since: u64,
    request_id: Option<&str>,
    subscriber_tasks: &mut HashMap<String, tokio::task::JoinHandle<()>>,
    connection_sessions: &mut Vec<String>,
) {
    // Abort any existing subscriber for this session
    if let Some(task) = subscriber_tasks.remove(session_id) {
        task.abort();
    }

    if let Some(buffer) = state.session_manager.attach(session_id).await {
        // Read missed entries
        let (entries, dropped) = {
            let buf = buffer.lock().await;
            buf.read_since(since)
        };

        let entries_json: Vec<Value> = entries
            .iter()
            .map(|e| entry_to_ws_message(session_id, e))
            .collect();

        let last_seq = entries.last().map_or(since, |e| e.seq);

        let mut resp = json!({
            "type": "session.attached",
            "session_id": session_id,
            "entries": entries_json,
            "dropped": dropped,
        });
        if let Some(rid) = request_id {
            resp["request_id"] = json!(rid);
        }
        let _ = tx.send(resp).await;

        // Start a new subscriber from the last replayed seq
        let task = tokio::spawn(subscriber_task(
            session_id.to_string(),
            buffer,
            tx.clone(),
            last_seq,
        ));
        subscriber_tasks.insert(session_id.to_string(), task);

        // Track the session on this connection if not already tracked
        if !connection_sessions.contains(&session_id.to_string()) {
            connection_sessions.push(session_id.to_string());
        }
    } else {
        let mut resp = json!({
            "type": "error",
            "code": "SESSION_NOT_FOUND",
            "session_id": session_id,
            "message": format!("Session {session_id} not found"),
        });
        if let Some(rid) = request_id {
            resp["request_id"] = json!(rid);
        }
        let _ = tx.send(resp).await;
    }
}
