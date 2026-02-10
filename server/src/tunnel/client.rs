//! Tunnel client — outbound WS connection from device to relay.
//!
//! Spawned on startup when `[tunnel] url` is configured. Maintains a persistent
//! WebSocket to the relay with exponential-backoff reconnect, heartbeat, and
//! handles proxied requests by calling local route handlers.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::config::TunnelConfig;
use crate::sessions::buffer::{OutputBuffer, OutputEntry};
use crate::AppState;

/// Type alias for the WS sink to reduce verbosity.
type WsSink = Arc<
    Mutex<
        futures_util::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            tokio_tungstenite::tungstenite::Message,
        >,
    >,
>;

/// Spawn the tunnel client task. Returns a `JoinHandle` that runs until cancelled.
pub fn spawn(state: AppState, tunnel_config: TunnelConfig) -> tokio::task::JoinHandle<()> {
    tokio::spawn(tunnel_client_loop(state, tunnel_config))
}

/// Main loop: connect, handle messages, reconnect on failure.
async fn tunnel_client_loop(state: AppState, config: TunnelConfig) {
    let relay_url = config
        .url
        .as_deref()
        .expect("tunnel.url must be set for client mode");
    let mut delay = Duration::from_secs(config.reconnect_delay_secs);
    let max_delay = Duration::from_secs(config.reconnect_max_delay_secs);
    let mut reconnects: u64 = 0;

    loop {
        info!("Tunnel: connecting to relay at {relay_url}");
        match connect_and_run(&state, &config, relay_url).await {
            Ok(DisconnectReason::RelayShutdown) => {
                // Relay sent intentional shutdown — skip backoff
                info!("Tunnel: relay shutting down, reconnecting immediately...");
                delay = Duration::from_secs(config.reconnect_delay_secs);
            }
            Ok(DisconnectReason::Clean) => {
                info!("Tunnel: connection closed cleanly, reconnecting...");
                delay = Duration::from_secs(config.reconnect_delay_secs);
            }
            Err(e) => {
                warn!(
                    "Tunnel: connection error: {e}, reconnecting in {}s",
                    delay.as_secs()
                );
            }
        }
        reconnects += 1;
        state
            .tunnel_reconnects
            .store(reconnects, std::sync::atomic::Ordering::Relaxed);
        state
            .tunnel_connected
            .store(false, std::sync::atomic::Ordering::Relaxed);
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(max_delay);
    }
}

/// Reason the tunnel connection ended.
enum DisconnectReason {
    /// Relay sent `tunnel.relay_shutdown` — intentional, skip backoff.
    RelayShutdown,
    /// Normal close frame or EOF.
    Clean,
}

/// A single connection attempt: connect, register, handle messages until disconnect.
#[allow(clippy::too_many_lines)]
async fn connect_and_run(
    state: &AppState,
    config: &TunnelConfig,
    relay_url: &str,
) -> Result<DisconnectReason, Box<dyn std::error::Error + Send + Sync>> {
    // Build the URL with auth query params
    let url = format!(
        "{}?token={}&serial={}",
        relay_url, config.tunnel_key, state.config.device.serial
    );

    let (ws_stream, _response) = tokio_tungstenite::connect_async(&url).await?;
    let (ws_sink, ws_stream) = ws_stream.split();
    let ws_sink = Arc::new(Mutex::new(ws_sink));

    info!("Tunnel: connected to relay, registering...");
    state
        .tunnel_connected
        .store(true, std::sync::atomic::Ordering::Relaxed);

    // Send registration message with our api_key
    {
        let mut sink = ws_sink.lock().await;
        let reg = json!({
            "type": "tunnel.register",
            "serial": state.config.device.serial,
            "api_key": state.config.auth.api_key,
        });
        sink.send(tokio_tungstenite::tungstenite::Message::Text(
            serde_json::to_string(&reg)?.into(),
        ))
        .await?;
    }

    // Subscribe to session lifecycle broadcasts so we can forward them
    let mut broadcast_rx = state.session_events.subscribe();

    // Track subscriber tasks for session output forwarding
    let subscriber_tasks: Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Heartbeat task
    let heartbeat_sink = ws_sink.clone();
    let heartbeat_interval = Duration::from_secs(config.heartbeat_interval_secs);
    let heartbeat_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(heartbeat_interval);
        loop {
            interval.tick().await;
            let mut sink = heartbeat_sink.lock().await;
            let msg = json!({"type": "tunnel.ping"});
            if sink
                .send(tokio_tungstenite::tungstenite::Message::Text(
                    serde_json::to_string(&msg).unwrap().into(),
                ))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let mut ws_stream = ws_stream;
    let mut disconnect_reason = DisconnectReason::Clean;

    // Re-subscribe to all running sessions after reconnect
    {
        let sessions = state.session_manager.list_sessions().await;
        for s in &sessions {
            if s.status == "running" {
                if let Some(buffer) = state.session_manager.get_buffer(&s.session_id).await {
                    let last_seq = {
                        let buf = buffer.lock().await;
                        buf.next_seq().saturating_sub(1)
                    };
                    let sink_clone = ws_sink.clone();
                    let sid = s.session_id.clone();
                    let task = tokio::spawn(tunnel_subscriber_task(
                        sid.clone(),
                        buffer,
                        sink_clone,
                        last_seq,
                    ));
                    subscriber_tasks.lock().await.insert(sid, task);
                }
            }
        }
        let count = subscriber_tasks.lock().await.len();
        if count > 0 {
            info!("Tunnel: re-subscribed to {count} running sessions");
        }
    }

    loop {
        tokio::select! {
            msg = ws_stream.next() => {
                let Some(msg) = msg else { break };
                let msg = msg?;
                match msg {
                    tokio_tungstenite::tungstenite::Message::Text(text) => {
                        let parsed: Value = serde_json::from_str(&text)?;
                        if parsed["type"].as_str() == Some("tunnel.relay_shutdown") {
                            info!("Tunnel: relay sent shutdown notification");
                            disconnect_reason = DisconnectReason::RelayShutdown;
                            break;
                        }
                        handle_relay_message(state, &ws_sink, &subscriber_tasks, parsed).await;
                    }
                    tokio_tungstenite::tungstenite::Message::Close(_) => break,
                    _ => {}
                }
            }
            broadcast_msg = broadcast_rx.recv() => {
                if let Ok(event) = broadcast_msg {
                    // Forward session lifecycle events to relay
                    let mut sink = ws_sink.lock().await;
                    let _ = sink.send(tokio_tungstenite::tungstenite::Message::Text(
                        serde_json::to_string(&event).unwrap().into(),
                    )).await;
                }
            }
        }
    }

    // Cleanup
    heartbeat_task.abort();
    let tasks = subscriber_tasks.lock().await;
    for (_, task) in tasks.iter() {
        task.abort();
    }

    Ok(disconnect_reason)
}

/// Handle a message from the relay (proxied client request or control message).
async fn handle_relay_message(
    state: &AppState,
    ws_sink: &WsSink,
    subscriber_tasks: &Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
    msg: Value,
) {
    let msg_type = msg["type"].as_str().unwrap_or("");
    let request_id = msg["request_id"].as_str().map(ToString::to_string);

    match msg_type {
        "tunnel.pong" => {
            // Heartbeat response, ignore
        }
        "tunnel.exec" => {
            handle_tunnel_exec(state, ws_sink, &msg, request_id.as_deref()).await;
        }
        "tunnel.exec_batch" => {
            handle_tunnel_exec_batch(state, ws_sink, &msg, request_id.as_deref()).await;
        }
        "tunnel.info" => {
            handle_tunnel_info(state, ws_sink, request_id.as_deref()).await;
        }
        "tunnel.health" => {
            handle_tunnel_health(state, ws_sink, request_id.as_deref()).await;
        }
        "tunnel.file.read" => {
            handle_tunnel_file_read(state, ws_sink, &msg, request_id.as_deref()).await;
        }
        "tunnel.file.write" => {
            handle_tunnel_file_write(state, ws_sink, &msg, request_id.as_deref()).await;
        }
        "tunnel.activity" => {
            handle_tunnel_activity(state, ws_sink, &msg, request_id.as_deref()).await;
        }
        // Forwarded session.* messages from clients via relay
        t if t.starts_with("session.") => {
            handle_forwarded_session_message(state, ws_sink, subscriber_tasks, &msg).await;
        }
        _ => {
            warn!(msg_type, "Unknown tunnel message type");
        }
    }
}

/// Send a JSON response back through the tunnel WS.
async fn send_response(ws_sink: &WsSink, msg: Value) {
    let mut sink = ws_sink.lock().await;
    let _ = sink
        .send(tokio_tungstenite::tungstenite::Message::Text(
            serde_json::to_string(&msg).unwrap().into(),
        ))
        .await;
}

/// Handle tunnel.exec — one-shot command execution
async fn handle_tunnel_exec(
    state: &AppState,
    ws_sink: &WsSink,
    msg: &Value,
    request_id: Option<&str>,
) {
    let command = msg["command"].as_str().unwrap_or("");
    let timeout_ms = msg["timeout_ms"]
        .as_u64()
        .unwrap_or(state.config.server.exec_timeout_ms);
    let shell = msg["shell"]
        .as_str()
        .unwrap_or(&state.config.shell.default_shell);
    let raw_dir = msg["working_dir"]
        .as_str()
        .unwrap_or(&state.config.shell.default_working_dir);
    let expanded_dir = crate::util::expand_tilde(raw_dir);
    let working_dir = expanded_dir.as_ref();
    let env: Option<HashMap<String, String>> = msg
        .get("env")
        .and_then(|v| serde_json::from_value(v.clone()).ok());

    let result = match Box::pin(crate::shell::process::exec_command(
        shell,
        working_dir,
        command,
        timeout_ms,
        env.as_ref(),
    ))
    .await
    {
        Ok(r) => json!({
            "type": "tunnel.exec.result",
            "request_id": request_id,
            "status": 200,
            "body": {
                "exit_code": r.exit_code,
                "stdout": r.stdout,
                "stderr": r.stderr,
                "duration_ms": r.duration_ms,
            }
        }),
        Err(crate::shell::process::ExecError::Timeout) => json!({
            "type": "tunnel.exec.result",
            "request_id": request_id,
            "status": 504,
            "body": {"error": "Command timed out", "code": "TIMEOUT"}
        }),
        Err(e) => json!({
            "type": "tunnel.exec.result",
            "request_id": request_id,
            "status": 500,
            "body": {"error": e.to_string(), "code": "EXEC_FAILED"}
        }),
    };

    send_response(ws_sink, result).await;
}

/// Handle `tunnel.exec_batch` — batch command execution
async fn handle_tunnel_exec_batch(
    state: &AppState,
    ws_sink: &WsSink,
    msg: &Value,
    request_id: Option<&str>,
) {
    let Some(commands) = msg["commands"].as_array() else {
        send_response(
            ws_sink,
            json!({
                "type": "tunnel.exec_batch.result",
                "request_id": request_id,
                "status": 400,
                "body": {"error": "commands array is required", "code": "INVALID_REQUEST"}
            }),
        )
        .await;
        return;
    };

    if commands.len() > state.config.server.max_batch_size {
        send_response(ws_sink, json!({
            "type": "tunnel.exec_batch.result",
            "request_id": request_id,
            "status": 400,
            "body": {
                "error": format!("Too many commands (max {})", state.config.server.max_batch_size),
                "code": "BATCH_TOO_LARGE"
            }
        }))
        .await;
        return;
    }

    let default_shell = msg["shell"]
        .as_str()
        .unwrap_or(&state.config.shell.default_shell);
    let default_dir = msg["working_dir"]
        .as_str()
        .unwrap_or(&state.config.shell.default_working_dir);
    let expanded_default_dir = crate::util::expand_tilde(default_dir);
    let batch_env: Option<HashMap<String, String>> = msg
        .get("env")
        .and_then(|v| serde_json::from_value(v.clone()).ok());

    let mut results = Vec::with_capacity(commands.len());
    for cmd in commands {
        let command = cmd["command"].as_str().unwrap_or("");
        let shell = cmd["shell"].as_str().unwrap_or(default_shell);
        let raw_cmd_dir = cmd["working_dir"].as_str().unwrap_or(&expanded_default_dir);
        let expanded_cmd_dir = crate::util::expand_tilde(raw_cmd_dir);
        let working_dir: &str = expanded_cmd_dir.as_ref();
        let timeout = cmd["timeout_ms"]
            .as_u64()
            .unwrap_or(state.config.server.exec_timeout_ms);

        let cmd_env: Option<HashMap<String, String>> = cmd
            .get("env")
            .and_then(|v| serde_json::from_value(v.clone()).ok());

        let merged_env = match (&batch_env, &cmd_env) {
            (None, None) => None,
            (Some(base), None) => Some(base.clone()),
            (None, Some(over)) => Some(over.clone()),
            (Some(base), Some(over)) => {
                let mut merged = base.clone();
                merged.extend(over.iter().map(|(k, v)| (k.clone(), v.clone())));
                Some(merged)
            }
        };

        match Box::pin(crate::shell::process::exec_command(
            shell,
            working_dir,
            command,
            timeout,
            merged_env.as_ref(),
        ))
        .await
        {
            Ok(r) => results.push(json!({
                "exit_code": r.exit_code,
                "stdout": r.stdout,
                "stderr": r.stderr,
                "duration_ms": r.duration_ms,
            })),
            Err(crate::shell::process::ExecError::Timeout) => results.push(json!({
                "exit_code": -1,
                "stdout": "",
                "stderr": "Command timed out",
                "duration_ms": timeout,
            })),
            Err(e) => results.push(json!({
                "exit_code": -1,
                "stdout": "",
                "stderr": e.to_string(),
                "duration_ms": 0,
            })),
        }
    }

    send_response(
        ws_sink,
        json!({
            "type": "tunnel.exec_batch.result",
            "request_id": request_id,
            "status": 200,
            "body": {"results": results}
        }),
    )
    .await;
}

/// Handle tunnel.info — system information
async fn handle_tunnel_info(state: &AppState, ws_sink: &WsSink, request_id: Option<&str>) {
    // Call the info handler directly — it returns JSON
    match crate::routes::info::info(axum::extract::State(state.clone())).await {
        Ok(axum::Json(body)) => {
            send_response(
                ws_sink,
                json!({
                    "type": "tunnel.info.result",
                    "request_id": request_id,
                    "status": 200,
                    "body": body,
                }),
            )
            .await;
        }
        Err(status) => {
            send_response(
                ws_sink,
                json!({
                    "type": "tunnel.info.result",
                    "request_id": request_id,
                    "status": status.as_u16(),
                    "body": {"error": "Failed to get info"},
                }),
            )
            .await;
        }
    }
}

/// Handle tunnel.health — health check
async fn handle_tunnel_health(state: &AppState, ws_sink: &WsSink, request_id: Option<&str>) {
    let axum::Json(body) = crate::routes::health::health(axum::extract::State(state.clone())).await;
    send_response(
        ws_sink,
        json!({
            "type": "tunnel.health.result",
            "request_id": request_id,
            "status": 200,
            "body": body,
        }),
    )
    .await;
}

/// Handle tunnel.file.read — file read or directory list
async fn handle_tunnel_file_read(
    state: &AppState,
    ws_sink: &WsSink,
    msg: &Value,
    request_id: Option<&str>,
) {
    let path = msg["path"].as_str().unwrap_or("");
    let list = msg["list"].as_bool().unwrap_or(false);

    let query = crate::routes::files::FilesQuery {
        path: path.to_string(),
        list,
    };

    match crate::routes::files::get_file(
        axum::extract::State(state.clone()),
        axum::http::HeaderMap::new(),
        axum::extract::Query(query),
    )
    .await
    {
        Ok(axum::Json(body)) => {
            send_response(
                ws_sink,
                json!({
                    "type": "tunnel.file.read.result",
                    "request_id": request_id,
                    "status": 200,
                    "body": body,
                }),
            )
            .await;
        }
        Err((status, axum::Json(body))) => {
            send_response(
                ws_sink,
                json!({
                    "type": "tunnel.file.read.result",
                    "request_id": request_id,
                    "status": status.as_u16(),
                    "body": body,
                }),
            )
            .await;
        }
    }
}

/// Handle tunnel.file.write — file write
async fn handle_tunnel_file_write(
    state: &AppState,
    ws_sink: &WsSink,
    msg: &Value,
    request_id: Option<&str>,
) {
    let path = msg["path"].as_str().unwrap_or("").to_string();
    let content = msg["content"].as_str().unwrap_or("").to_string();
    let create_dirs = msg["create_dirs"].as_bool().unwrap_or(false);
    let mode = msg["mode"].as_str().map(ToString::to_string);
    let encoding = msg["encoding"].as_str().map(ToString::to_string);

    let payload = crate::routes::files::FileWriteRequest {
        path,
        content,
        create_dirs,
        mode,
        encoding,
    };

    match crate::routes::files::put_file(
        axum::extract::State(state.clone()),
        axum::http::HeaderMap::new(),
        axum::Json(payload),
    )
    .await
    {
        Ok(axum::Json(body)) => {
            send_response(
                ws_sink,
                json!({
                    "type": "tunnel.file.write.result",
                    "request_id": request_id,
                    "status": 200,
                    "body": body,
                }),
            )
            .await;
        }
        Err((status, axum::Json(body))) => {
            send_response(
                ws_sink,
                json!({
                    "type": "tunnel.file.write.result",
                    "request_id": request_id,
                    "status": status.as_u16(),
                    "body": body,
                }),
            )
            .await;
        }
    }
}

/// Handle tunnel.activity — activity journal read
async fn handle_tunnel_activity(
    state: &AppState,
    ws_sink: &WsSink,
    msg: &Value,
    request_id: Option<&str>,
) {
    let since_id = msg["since_id"].as_u64().unwrap_or(0);
    let limit = usize::try_from(msg["limit"].as_u64().unwrap_or(50)).unwrap_or(50);
    let entries = state
        .activity_log
        .read_since(since_id, limit.min(200))
        .await;

    send_response(
        ws_sink,
        json!({
            "type": "tunnel.activity.result",
            "request_id": request_id,
            "status": 200,
            "body": { "entries": entries },
        }),
    )
    .await;
}

/// Handle forwarded `session.*` messages from clients through the relay.
///
/// These are the same message types as in `ws/mod.rs` but forwarded over the tunnel.
/// We dispatch to the `SessionManager` and send responses back through the tunnel.
#[allow(clippy::too_many_lines)]
async fn handle_forwarded_session_message(
    state: &AppState,
    ws_sink: &WsSink,
    subscriber_tasks: &Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
    msg: &Value,
) {
    let msg_type = msg["type"].as_str().unwrap_or("");
    let request_id = msg["request_id"].as_str().map(ToString::to_string);

    match msg_type {
        "session.start" => {
            let working_dir = msg["working_dir"].as_str().map(ToString::to_string);
            let persistent = msg["persistent"].as_bool().unwrap_or(false);
            let env: Option<HashMap<String, String>> = msg
                .get("env")
                .and_then(|v| serde_json::from_value(v.clone()).ok());
            let shell = msg["shell"].as_str().map(ToString::to_string);
            let use_pty = msg["pty"].as_bool().unwrap_or(false);
            let name = msg["name"].as_str().map(ToString::to_string);
            let user_allows_ai = msg["user_allows_ai"].as_bool();
            #[allow(clippy::cast_possible_truncation)]
            let rows = msg["rows"]
                .as_u64()
                .unwrap_or(u64::from(state.config.server.default_terminal_rows))
                as u16;
            #[allow(clippy::cast_possible_truncation)]
            let cols = msg["cols"]
                .as_u64()
                .unwrap_or(u64::from(state.config.server.default_terminal_cols))
                as u16;
            let idle_timeout = msg["idle_timeout"].as_u64().unwrap_or(0);

            let raw_dir = working_dir
                .as_deref()
                .unwrap_or(&state.config.shell.default_working_dir);
            let expanded = crate::util::expand_tilde(raw_dir);
            let dir = expanded.as_ref();
            let sh = shell
                .as_deref()
                .unwrap_or(&state.config.shell.default_shell);
            let allows_ai = user_allows_ai.unwrap_or(true);

            match state
                .session_manager
                .create_session_with_pty(
                    sh,
                    dir,
                    env.as_ref(),
                    persistent,
                    use_pty,
                    rows,
                    cols,
                    idle_timeout,
                    name.as_deref(),
                )
                .await
            {
                Ok((session_id, pid)) => {
                    if !allows_ai {
                        let _ = state
                            .session_manager
                            .set_user_allows_ai(&session_id, false)
                            .await;
                    }

                    // Start subscriber for output forwarding
                    if let Some(buffer) = state.session_manager.get_buffer(&session_id).await {
                        let sink_clone = ws_sink.clone();
                        let sid = session_id.clone();
                        let task = tokio::spawn(tunnel_subscriber_task(
                            sid.clone(),
                            buffer,
                            sink_clone,
                            0,
                        ));
                        subscriber_tasks.lock().await.insert(sid, task);
                    }

                    let mut resp = json!({
                        "type": "session.started",
                        "session_id": session_id,
                        "pid": pid,
                        "persistent": persistent,
                        "pty": use_pty,
                        "user_allows_ai": allows_ai,
                    });
                    if let Some(n) = name.as_deref() {
                        resp["name"] = json!(n);
                    }
                    if let Some(ref rid) = request_id {
                        resp["request_id"] = json!(rid);
                    }
                    send_response(ws_sink, resp).await;

                    // Broadcast
                    let mut broadcast = json!({
                        "type": "session.created",
                        "session_id": session_id,
                        "pid": pid,
                        "pty": use_pty,
                        "persistent": persistent,
                        "user_allows_ai": allows_ai,
                    });
                    if let Some(n) = name.as_deref() {
                        broadcast["name"] = json!(n);
                    }
                    let _ = state.session_events.send(broadcast);
                }
                Err(e) => {
                    let mut resp = json!({
                        "type": "error",
                        "code": "SESSION_LIMIT",
                        "message": e,
                    });
                    if let Some(ref rid) = request_id {
                        resp["request_id"] = json!(rid);
                    }
                    send_response(ws_sink, resp).await;
                }
            }
        }
        "session.exec" => {
            let session_id = msg["session_id"].as_str().unwrap_or("");
            let command = msg["command"].as_str().unwrap_or("");
            state.session_manager.touch_ai_activity(session_id).await;
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
                if let Some(ref rid) = request_id {
                    resp["request_id"] = json!(rid);
                }
                send_response(ws_sink, resp).await;
            } else {
                let mut resp = json!({
                    "type": "session.exec.ack",
                    "session_id": session_id,
                });
                if let Some(ref rid) = request_id {
                    resp["request_id"] = json!(rid);
                }
                send_response(ws_sink, resp).await;
            }
        }
        "session.stdin" => {
            let session_id = msg["session_id"].as_str().unwrap_or("");
            let data = msg["data"].as_str().unwrap_or("");
            if !session_id.is_empty() {
                state.session_manager.touch_ai_activity(session_id).await;
                if let Err(e) = state
                    .session_manager
                    .send_to_session(session_id, data)
                    .await
                {
                    send_response(
                        ws_sink,
                        json!({
                            "type": "error",
                            "code": "SESSION_ERROR",
                            "session_id": session_id,
                            "message": e,
                        }),
                    )
                    .await;
                }
            }
        }
        "session.kill" => {
            let session_id = msg["session_id"].as_str().unwrap_or("");
            if !session_id.is_empty() {
                let found = state.session_manager.kill_session(session_id).await;
                if found {
                    let mut resp = json!({
                        "type": "session.closed",
                        "session_id": session_id,
                        "reason": "killed",
                    });
                    if let Some(ref rid) = request_id {
                        resp["request_id"] = json!(rid);
                    }
                    send_response(ws_sink, resp).await;
                    let _ = state.session_events.send(json!({
                        "type": "session.destroyed",
                        "session_id": session_id,
                        "reason": "killed",
                    }));
                    // Abort subscriber
                    if let Some(task) = subscriber_tasks.lock().await.remove(session_id) {
                        task.abort();
                    }
                } else {
                    let mut resp = json!({
                        "type": "error",
                        "code": "SESSION_NOT_FOUND",
                        "session_id": session_id,
                        "message": format!("Session {session_id} not found"),
                    });
                    if let Some(ref rid) = request_id {
                        resp["request_id"] = json!(rid);
                    }
                    send_response(ws_sink, resp).await;
                }
            }
        }
        "session.signal" => {
            let session_id = msg["session_id"].as_str().unwrap_or("");
            let signal = msg["signal"].as_i64().unwrap_or(0);
            if !session_id.is_empty() && signal != 0 {
                #[allow(clippy::cast_possible_truncation)]
                let signal_i32 = signal as i32;
                match state
                    .session_manager
                    .signal_session(session_id, signal_i32)
                    .await
                {
                    Ok(()) => {
                        let mut resp = json!({
                            "type": "session.signal.ack",
                            "session_id": session_id,
                            "signal": signal,
                        });
                        if let Some(ref rid) = request_id {
                            resp["request_id"] = json!(rid);
                        }
                        send_response(ws_sink, resp).await;
                    }
                    Err(e) => {
                        let mut resp = json!({
                            "type": "error",
                            "code": "SESSION_ERROR",
                            "session_id": session_id,
                            "message": e,
                        });
                        if let Some(ref rid) = request_id {
                            resp["request_id"] = json!(rid);
                        }
                        send_response(ws_sink, resp).await;
                    }
                }
            }
        }
        "session.attach" => {
            let session_id = msg["session_id"].as_str().unwrap_or("");
            let since = msg["since"].as_u64().unwrap_or(0);
            if !session_id.is_empty() {
                // Abort any existing subscriber for this session
                if let Some(task) = subscriber_tasks.lock().await.remove(session_id) {
                    task.abort();
                }

                if let Some(buffer) = state.session_manager.attach(session_id).await {
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
                    if let Some(ref rid) = request_id {
                        resp["request_id"] = json!(rid);
                    }
                    send_response(ws_sink, resp).await;

                    // Start subscriber
                    let sink_clone = ws_sink.clone();
                    let sid = session_id.to_string();
                    let task = tokio::spawn(tunnel_subscriber_task(
                        sid.clone(),
                        buffer,
                        sink_clone,
                        last_seq,
                    ));
                    subscriber_tasks.lock().await.insert(sid, task);
                } else {
                    let mut resp = json!({
                        "type": "error",
                        "code": "SESSION_NOT_FOUND",
                        "session_id": session_id,
                        "message": format!("Session {session_id} not found"),
                    });
                    if let Some(ref rid) = request_id {
                        resp["request_id"] = json!(rid);
                    }
                    send_response(ws_sink, resp).await;
                }
            }
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
                        "user_allows_ai": s.user_allows_ai,
                        "ai_is_working": s.ai_is_working,
                    });
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
            send_response(ws_sink, resp).await;
        }
        "session.resize" => {
            let session_id = msg["session_id"].as_str().unwrap_or("");
            #[allow(clippy::cast_possible_truncation)]
            let rows = msg["rows"].as_u64().unwrap_or(0) as u16;
            #[allow(clippy::cast_possible_truncation)]
            let cols = msg["cols"].as_u64().unwrap_or(0) as u16;
            if !session_id.is_empty() && rows > 0 && cols > 0 {
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
                        if let Some(ref rid) = request_id {
                            resp["request_id"] = json!(rid);
                        }
                        send_response(ws_sink, resp).await;
                    }
                    Err(e) => {
                        let mut resp = json!({
                            "type": "error",
                            "code": "SESSION_ERROR",
                            "session_id": session_id,
                            "message": e,
                        });
                        if let Some(ref rid) = request_id {
                            resp["request_id"] = json!(rid);
                        }
                        send_response(ws_sink, resp).await;
                    }
                }
            }
        }
        "session.allow_ai" => {
            let session_id = msg["session_id"].as_str().unwrap_or("");
            let allowed = msg["allowed"].as_bool();
            if !session_id.is_empty() {
                if let Some(allowed) = allowed {
                    match state
                        .session_manager
                        .set_user_allows_ai(session_id, allowed)
                        .await
                    {
                        Ok(ai_cleared) => {
                            let mut resp = json!({
                                "type": "session.allow_ai.ack",
                                "session_id": session_id,
                                "allowed": allowed,
                            });
                            if let Some(ref rid) = request_id {
                                resp["request_id"] = json!(rid);
                            }
                            send_response(ws_sink, resp).await;
                            let _ = state.session_events.send(json!({
                                "type": "session.ai_permission_changed",
                                "session_id": session_id,
                                "allowed": allowed,
                            }));
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
                            send_response(ws_sink, resp).await;
                        }
                    }
                }
            }
        }
        "session.ai_status" => {
            let session_id = msg["session_id"].as_str().unwrap_or("");
            let working = msg["working"].as_bool();
            if !session_id.is_empty() {
                if let Some(working) = working {
                    let activity = msg["activity"].as_str();
                    let message = msg["message"].as_str();
                    match state
                        .session_manager
                        .set_ai_status(session_id, working, activity, message)
                        .await
                    {
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
                            send_response(ws_sink, resp).await;
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
                            send_response(ws_sink, resp).await;
                        }
                    }
                }
            }
        }
        "session.rename" => {
            let session_id = msg["session_id"].as_str().unwrap_or("");
            let name = msg["name"].as_str().unwrap_or("");
            if !session_id.is_empty() && !name.is_empty() {
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
                        send_response(ws_sink, resp).await;
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
                        send_response(ws_sink, resp).await;
                    }
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
            send_response(ws_sink, resp).await;
        }
        _ => {
            warn!(msg_type, "Unknown forwarded session message type");
        }
    }
}

/// Convert an `OutputEntry` to a WS JSON message (same as `ws/mod.rs`).
fn entry_to_ws_message(session_id: &str, entry: &OutputEntry) -> Value {
    json!({
        "type": format!("session.{}", entry.stream.as_str()),
        "session_id": session_id,
        "data": entry.data,
        "seq": entry.seq,
        "timestamp_ms": entry.timestamp_ms,
    })
}

/// Background task that reads from a session's `OutputBuffer` and forwards
/// entries as WS messages through the tunnel. Similar to `ws/mod.rs` `subscriber_task`.
async fn tunnel_subscriber_task(
    session_id: String,
    buffer: Arc<tokio::sync::Mutex<OutputBuffer>>,
    ws_sink: WsSink,
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
            let text = serde_json::to_string(&msg).unwrap();
            let mut sink = ws_sink.lock().await;
            if sink
                .send(tokio_tungstenite::tungstenite::Message::Text(text.into()))
                .await
                .is_err()
            {
                return;
            }
            cursor = entry.seq;
        }
        if let Some(n) = notify {
            n.notified().await;
        }
    }
}
