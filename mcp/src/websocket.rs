//! WebSocket client for streaming sessions with auto-reconnect.
//!
//! [`DeviceWsConnection`] maintains a persistent WebSocket connection to a
//! sctl device. Incoming session output is dispatched to per-session local
//! buffers. MCP tools read from these buffers (zero network latency for
//! buffered output) while all writes go over the WebSocket.
//!
//! On disconnect, the client automatically reconnects with exponential backoff
//! and re-attaches to all active sessions using `session.attach` with the last
//! known sequence number, so no output is lost.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{mpsc, Mutex, Notify};

/// Session lifecycle status (mirrors sctl server's `SessionStatus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    Running,
    Exited,
}

/// A single output entry from a session.
#[derive(Debug, Clone)]
pub struct OutputEntry {
    pub seq: u64,
    pub stream: String,
    pub data: String,
    pub timestamp_ms: u64,
}

/// Local buffer for a session's output, held in mcp-sctl's memory.
struct SessionBuffer {
    entries: Vec<OutputEntry>,
    status: SessionStatus,
    exit_code: Option<i32>,
    notify: Arc<Notify>,
    last_seq: u64,
    is_pty: bool,
    /// Count of entries dropped due to ring buffer eviction (detected via sequence gaps).
    dropped_count: u64,
    /// Notify for session.attached responses (used by attach_session).
    attach_notify: Arc<Notify>,
    /// Set to true when a session.attached response has been received.
    attached: bool,
}

impl SessionBuffer {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            status: SessionStatus::Running,
            exit_code: None,
            notify: Arc::new(Notify::new()),
            last_seq: 0,
            is_pty: false,
            dropped_count: 0,
            attach_notify: Arc::new(Notify::new()),
            attached: false,
        }
    }

    fn push(&mut self, entry: OutputEntry) {
        // Detect sequence gaps (ring buffer eviction on the daemon side)
        if entry.seq > self.last_seq + 1 && self.last_seq > 0 {
            self.dropped_count += entry.seq - self.last_seq - 1;
        }
        if entry.seq > self.last_seq {
            self.last_seq = entry.seq;
        }
        self.entries.push(entry);
        self.notify.notify_waiters();
    }
}

/// Synchronization primitives for request-response patterns over WebSocket.
///
/// Each pair (notify + result) allows a tool handler to send a WS message and
/// then block until the server responds with an ack or error.
struct WsNotifiers {
    start_notify: Arc<Notify>,
    start_result: Arc<Mutex<Option<Value>>>,
    list_notify: Arc<Notify>,
    list_result: Arc<Mutex<Option<Value>>>,
    ai_status_notify: Arc<Notify>,
    ai_status_result: Arc<Mutex<Option<Value>>>,
}

/// Persistent WebSocket connection to a sctl device.
#[allow(dead_code)]
pub struct DeviceWsConnection {
    sender: mpsc::Sender<Value>,
    sessions: Arc<Mutex<HashMap<String, SessionBuffer>>>,
    connected: Arc<AtomicBool>,
    notifiers: Arc<WsNotifiers>,
    /// Tracks which sessions the AI is currently marked as working in.
    ai_working_sessions: Arc<Mutex<HashSet<String>>>,
}

impl DeviceWsConnection {
    /// Connect to a sctl device's WebSocket endpoint.
    ///
    /// Spawns background tasks for reading and reconnecting.
    pub async fn connect(url: &str, api_key: &str) -> Result<Self, String> {
        let ws_url = build_ws_url(url, api_key)?;

        let sessions: Arc<Mutex<HashMap<String, SessionBuffer>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let connected = Arc::new(AtomicBool::new(false));
        let notifiers = Arc::new(WsNotifiers {
            start_notify: Arc::new(Notify::new()),
            start_result: Arc::new(Mutex::new(None)),
            list_notify: Arc::new(Notify::new()),
            list_result: Arc::new(Mutex::new(None)),
            ai_status_notify: Arc::new(Notify::new()),
            ai_status_result: Arc::new(Mutex::new(None)),
        });

        let ai_working_sessions: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

        let (out_tx, out_rx) = mpsc::channel::<Value>(256);

        // Initial connect
        let (ws_stream, _) = tokio_tungstenite::connect_async(&ws_url)
            .await
            .map_err(|e| format!("WebSocket connect failed: {e}"))?;

        connected.store(true, Ordering::SeqCst);

        // Spawn the main WS I/O loop + reconnect logic
        tokio::spawn(ws_io_loop(
            ws_stream,
            out_rx,
            Arc::clone(&sessions),
            Arc::clone(&connected),
            Arc::clone(&notifiers),
            Arc::clone(&ai_working_sessions),
            ws_url.clone(),
        ));

        Ok(Self {
            sender: out_tx,
            sessions,
            connected,
            notifiers,
            ai_working_sessions,
        })
    }

    /// Send a raw JSON message over the WebSocket.
    pub async fn send(&self, message: Value) -> Result<(), String> {
        self.sender
            .send(message)
            .await
            .map_err(|_| "WebSocket sender closed".to_string())
    }

    /// Start a new session and wait for the `session.started` response.
    #[allow(clippy::too_many_arguments)]
    pub async fn start_session(
        &self,
        working_dir: Option<&str>,
        shell: Option<&str>,
        env: Option<&HashMap<String, String>>,
        persistent: bool,
        use_pty: bool,
        rows: Option<u64>,
        cols: Option<u64>,
        idle_timeout: Option<u64>,
        name: Option<&str>,
        user_allows_ai: bool,
    ) -> Result<Value, String> {
        // Clear any stale start result
        *self.notifiers.start_result.lock().await = None;

        let mut msg = json!({
            "type": "session.start",
            "persistent": persistent,
        });
        if !user_allows_ai {
            msg["user_allows_ai"] = json!(false);
        }
        if let Some(d) = working_dir {
            msg["working_dir"] = json!(d);
        }
        if let Some(s) = shell {
            msg["shell"] = json!(s);
        }
        if let Some(e) = env {
            msg["env"] = json!(e);
        }
        if use_pty {
            msg["pty"] = json!(true);
        }
        if let Some(r) = rows {
            msg["rows"] = json!(r);
        }
        if let Some(c) = cols {
            msg["cols"] = json!(c);
        }
        if let Some(t) = idle_timeout {
            msg["idle_timeout"] = json!(t);
        }
        if let Some(n) = name {
            msg["name"] = json!(n);
        }

        self.send(msg).await?;

        // Wait for session.started (with timeout)
        let result = tokio::time::timeout(
            tokio::time::Duration::from_secs(10),
            self.notifiers.start_notify.notified(),
        )
        .await;

        if result.is_err() {
            return Err("Timeout waiting for session.started".to_string());
        }

        let result = self.notifiers.start_result.lock().await.take();
        match result {
            Some(v) => {
                // Create local buffer for this session
                if let Some(session_id) = v["session_id"].as_str() {
                    let mut buf = SessionBuffer::new();
                    buf.is_pty = v["pty"].as_bool().unwrap_or(false);
                    let mut sessions = self.sessions.lock().await;
                    sessions.insert(session_id.to_string(), buf);
                }
                Ok(v)
            }
            None => Err("No session.started response received".to_string()),
        }
    }

    /// Set AI working status and wait for the server's ack or rejection.
    pub async fn set_ai_status(
        &self,
        session_id: &str,
        working: bool,
        activity: Option<&str>,
        message: Option<&str>,
    ) -> Result<Value, String> {
        // Clear any stale result
        *self.notifiers.ai_status_result.lock().await = None;

        let mut msg = json!({
            "type": "session.ai_status",
            "session_id": session_id,
            "working": working,
        });
        if let Some(a) = activity {
            msg["activity"] = json!(a);
        }
        if let Some(m) = message {
            msg["message"] = json!(m);
        }

        self.send(msg).await?;

        // Wait for ack or error
        let result = tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            self.notifiers.ai_status_notify.notified(),
        )
        .await;

        if result.is_err() {
            return Err("Timeout waiting for session.ai_status response".to_string());
        }

        let result = self.notifiers.ai_status_result.lock().await.take();
        match result {
            Some(v) => {
                if v["type"].as_str() == Some("error") {
                    Err(v["message"]
                        .as_str()
                        .unwrap_or("AI status rejected")
                        .to_string())
                } else {
                    Ok(v)
                }
            }
            None => Err("No ai_status response received".to_string()),
        }
    }

    /// Read output from a session's local buffer.
    ///
    /// Returns entries with `seq > since`. If no entries are available, waits
    /// up to `timeout_ms` for new data.
    pub async fn read_output(
        &self,
        session_id: &str,
        since: u64,
        timeout_ms: u64,
    ) -> Result<ReadResult, String> {
        let notify = {
            let sessions = self.sessions.lock().await;
            let buf = sessions
                .get(session_id)
                .ok_or_else(|| format!("Session {session_id} not found locally"))?;

            // Check if we already have data
            let entries: Vec<OutputEntry> = buf
                .entries
                .iter()
                .filter(|e| e.seq > since)
                .cloned()
                .collect();
            if !entries.is_empty() {
                return Ok(ReadResult {
                    entries,
                    status: buf.status,
                    exit_code: buf.exit_code,
                    dropped_count: buf.dropped_count,
                });
            }

            // If exited and no new data, return immediately
            if buf.status == SessionStatus::Exited {
                return Ok(ReadResult {
                    entries: vec![],
                    status: buf.status,
                    exit_code: buf.exit_code,
                    dropped_count: buf.dropped_count,
                });
            }

            Arc::clone(&buf.notify)
        };

        // Wait for new data with timeout
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_millis(timeout_ms),
            notify.notified(),
        )
        .await;

        // Re-read after wait
        let sessions = self.sessions.lock().await;
        let buf = sessions
            .get(session_id)
            .ok_or_else(|| format!("Session {session_id} not found locally"))?;

        let entries: Vec<OutputEntry> = buf
            .entries
            .iter()
            .filter(|e| e.seq > since)
            .cloned()
            .collect();

        Ok(ReadResult {
            entries,
            status: buf.status,
            exit_code: buf.exit_code,
            dropped_count: buf.dropped_count,
        })
    }

    /// Check whether the WebSocket is currently connected.
    #[allow(dead_code)]
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    /// Check whether a session is running in PTY mode.
    pub async fn is_pty_session(&self, session_id: &str) -> bool {
        let sessions = self.sessions.lock().await;
        sessions.get(session_id).is_some_and(|buf| buf.is_pty)
    }

    /// Check whether AI is currently marked as working in a session.
    pub async fn is_ai_working(&self, session_id: &str) -> bool {
        self.ai_working_sessions.lock().await.contains(session_id)
    }

    /// Mark AI as working in a session (local tracking only, no WS message).
    pub async fn mark_ai_working(&self, session_id: &str) {
        self.ai_working_sessions
            .lock()
            .await
            .insert(session_id.to_string());
    }

    /// Clear AI working state for a session (local tracking only).
    pub async fn clear_ai_working(&self, session_id: &str) {
        self.ai_working_sessions.lock().await.remove(session_id);
    }

    /// Auto-set AI working status if not already working.
    /// Sends `session.ai_status` with `working=true` to the server.
    /// On failure (e.g. AI_NOT_ALLOWED), logs but does not propagate the error.
    pub async fn auto_set_ai_working(&self, session_id: &str, activity: &str) {
        if self.is_ai_working(session_id).await {
            return;
        }
        match self
            .set_ai_status(session_id, true, Some(activity), None)
            .await
        {
            Ok(_) => {
                self.mark_ai_working(session_id).await;
            }
            Err(e) => {
                eprintln!("mcp-sctl: auto-set AI status failed for {session_id}: {e}");
            }
        }
    }

    /// List all sessions on the remote device by sending `session.list`.
    pub async fn list_sessions_remote(&self) -> Result<Value, String> {
        // Clear any stale result
        *self.notifiers.list_result.lock().await = None;

        self.send(json!({ "type": "session.list" })).await?;

        let result = tokio::time::timeout(
            tokio::time::Duration::from_secs(10),
            self.notifiers.list_notify.notified(),
        )
        .await;

        if result.is_err() {
            return Err("Timeout waiting for session.listed".to_string());
        }

        self.notifiers
            .list_result
            .lock()
            .await
            .take()
            .ok_or_else(|| "No session.listed response received".to_string())
    }

    /// Attach to an existing persistent session and replay buffered output.
    ///
    /// Creates a local `SessionBuffer` if one doesn't exist yet, sends
    /// `session.attach` over the WebSocket, and waits for the daemon's
    /// `session.attached` response with replayed entries.
    pub async fn attach_session(&self, session_id: &str, since: u64) -> Result<ReadResult, String> {
        // Ensure local buffer exists (may be new MCP process that doesn't know this session)
        let attach_notify = {
            let mut sessions = self.sessions.lock().await;
            let buf = sessions
                .entry(session_id.to_string())
                .or_insert_with(SessionBuffer::new);
            buf.attached = false;
            Arc::clone(&buf.attach_notify)
        };

        // Send attach request
        self.send(json!({
            "type": "session.attach",
            "session_id": session_id,
            "since": since,
        }))
        .await?;

        // Wait for session.attached response (with timeout)
        let result = tokio::time::timeout(
            tokio::time::Duration::from_secs(10),
            attach_notify.notified(),
        )
        .await;

        if result.is_err() {
            return Err("Timeout waiting for session.attached response".to_string());
        }

        // Read the replayed entries
        let sessions = self.sessions.lock().await;
        let buf = sessions
            .get(session_id)
            .ok_or_else(|| format!("Session {session_id} not found after attach"))?;

        let entries: Vec<OutputEntry> = buf
            .entries
            .iter()
            .filter(|e| e.seq > since)
            .cloned()
            .collect();

        Ok(ReadResult {
            entries,
            status: buf.status,
            exit_code: buf.exit_code,
            dropped_count: buf.dropped_count,
        })
    }

    /// Execute a command and wait for it to complete, returning all output.
    ///
    /// Wraps the command with a unique end marker, then polls for output until
    /// the marker is found or the timeout expires.
    pub async fn exec_wait(
        &self,
        session_id: &str,
        command: &str,
        timeout_ms: u64,
    ) -> Result<ExecWaitResult, String> {
        let is_pty = self.is_pty_session(session_id).await;
        let nonce = format!(
            "{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let marker = format!("__SCTL_{}_DONE_", nonce);

        // Wrap command: run it, then print marker with exit code
        let wrapped = format!("{} ; printf '\\n{}%s__\\n' \"$?\"", command, marker);

        // Record current position before sending
        let start_seq = {
            let sessions = self.sessions.lock().await;
            let buf = sessions
                .get(session_id)
                .ok_or_else(|| format!("Session {session_id} not found locally"))?;
            buf.last_seq
        };

        // Send the wrapped command
        self.send(json!({
            "type": "session.exec",
            "session_id": session_id,
            "command": wrapped,
        }))
        .await?;

        // Poll for output until marker is found or timeout
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(timeout_ms);
        let mut accumulated = String::new();
        let mut last_read_seq = start_seq;
        let marker_suffix = format!("{}__", marker); // e.g. __SCTL_<nonce>_DONE_0__
        let mut disconnected_since: Option<tokio::time::Instant> = None;
        const DISCONNECT_GRACE_SECS: u64 = 10;

        loop {
            let remaining = deadline.duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Ok(ExecWaitResult {
                    output: accumulated,
                    exit_code: None,
                    timed_out: true,
                });
            }

            // Fast-fail if disconnected for too long
            if self.is_connected() {
                disconnected_since = None;
            } else {
                let since = disconnected_since.get_or_insert_with(tokio::time::Instant::now);
                if since.elapsed().as_secs() >= DISCONNECT_GRACE_SECS {
                    return Err(format!(
                        "WebSocket disconnected for {}s during exec_wait",
                        DISCONNECT_GRACE_SECS
                    ));
                }
            }

            let poll_timeout = remaining.as_millis().min(1000) as u64;
            let result = self
                .read_output(session_id, last_read_seq, poll_timeout)
                .await?;

            for entry in &result.entries {
                accumulated.push_str(&entry.data);
                if entry.seq > last_read_seq {
                    last_read_seq = entry.seq;
                }
            }

            // Check if session died
            if result.status == SessionStatus::Exited {
                return Ok(ExecWaitResult {
                    output: accumulated,
                    exit_code: result.exit_code,
                    timed_out: false,
                });
            }

            // Scan for marker
            if let Some(marker_pos) = accumulated.find(&marker_suffix) {
                // Extract exit code from marker: __SCTL_<nonce>_DONE_<code>__
                let after_marker = &accumulated[marker_pos + marker.len()..];
                let exit_code = after_marker
                    .split("__")
                    .next()
                    .and_then(|s| s.parse::<i32>().ok());

                // Find the start of the marker line (the \n before it)
                let marker_line_start = accumulated[..marker_pos].rfind('\n').unwrap_or(marker_pos);

                let mut output = accumulated[..marker_line_start].to_string();

                // PTY echo stripping: first line is the echoed command
                if is_pty {
                    if let Some(first_newline) = output.find('\n') {
                        output = output[first_newline + 1..].to_string();
                    } else {
                        output.clear();
                    }
                }

                return Ok(ExecWaitResult {
                    output,
                    exit_code,
                    timed_out: false,
                });
            }
        }
    }
}

/// Result of a `read_output` call.
pub struct ReadResult {
    pub entries: Vec<OutputEntry>,
    pub status: SessionStatus,
    pub exit_code: Option<i32>,
    /// Number of entries dropped due to daemon ring buffer eviction.
    pub dropped_count: u64,
}

/// Result of an `exec_wait` call.
pub struct ExecWaitResult {
    pub output: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
}

/// Build the WebSocket URL from the HTTP base URL.
fn build_ws_url(base_url: &str, api_key: &str) -> Result<String, String> {
    let base = base_url.trim_end_matches('/');
    let ws_base = if base.starts_with("https://") {
        base.replacen("https://", "wss://", 1)
    } else if base.starts_with("http://") {
        base.replacen("http://", "ws://", 1)
    } else {
        return Err(format!("Invalid URL scheme: {base}"));
    };
    Ok(format!("{ws_base}/api/ws?token={api_key}"))
}

/// Parse an incoming WS message into an `OutputEntry` if it's a session output message.
fn parse_output_entry(msg: &Value) -> Option<(String, OutputEntry)> {
    let msg_type = msg["type"].as_str()?;
    let session_id = msg["session_id"].as_str()?.to_string();

    let stream = match msg_type {
        "session.stdout" => "stdout",
        "session.stderr" => "stderr",
        "session.system" => "system",
        _ => return None,
    };

    Some((
        session_id,
        OutputEntry {
            seq: msg["seq"].as_u64().unwrap_or(0),
            stream: stream.to_string(),
            data: msg["data"].as_str().unwrap_or("").to_string(),
            timestamp_ms: msg["timestamp_ms"].as_u64().unwrap_or(0),
        },
    ))
}

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Main I/O loop: reads from WS, dispatches to session buffers, handles
/// outgoing messages, and reconnects on failure.
async fn ws_io_loop(
    ws_stream: WsStream,
    mut out_rx: mpsc::Receiver<Value>,
    sessions: Arc<Mutex<HashMap<String, SessionBuffer>>>,
    connected: Arc<AtomicBool>,
    notifiers: Arc<WsNotifiers>,
    ai_working_sessions: Arc<Mutex<HashSet<String>>>,
    ws_url: String,
) {
    let (mut ws_sink, mut ws_reader) = ws_stream.split();

    loop {
        tokio::select! {
            // Incoming WS message
            msg = ws_reader.next() => {
                match msg {
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                        if let Ok(parsed) = serde_json::from_str::<Value>(&text) {
                            dispatch_message(
                                &parsed,
                                &sessions,
                                &notifiers,
                                &ai_working_sessions,
                            ).await;
                        }
                    }
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) | None => {
                        // Connection lost — reconnect
                        connected.store(false, Ordering::SeqCst);
                        eprintln!("mcp-sctl: WebSocket disconnected, reconnecting...");

                        if let Some((new_sink, new_reader)) = reconnect_loop(
                            &ws_url,
                            &sessions,
                        ).await {
                            ws_sink = new_sink;
                            ws_reader = new_reader;
                            connected.store(true, Ordering::SeqCst);
                            eprintln!("mcp-sctl: WebSocket reconnected");
                        } else {
                            // Reconnect loop gave up (shouldn't happen — it loops forever)
                            return;
                        }
                    }
                    Some(Err(e)) => {
                        eprintln!("mcp-sctl: WebSocket error: {e}");
                        connected.store(false, Ordering::SeqCst);

                        if let Some((new_sink, new_reader)) = reconnect_loop(
                            &ws_url,
                            &sessions,
                        ).await {
                            ws_sink = new_sink;
                            ws_reader = new_reader;
                            connected.store(true, Ordering::SeqCst);
                            eprintln!("mcp-sctl: WebSocket reconnected");
                        } else {
                            return;
                        }
                    }
                    _ => {} // Binary/Ping/Pong — ignore
                }
            }
            // Outgoing message from tools
            msg = out_rx.recv() => {
                match msg {
                    Some(value) => {
                        let text = serde_json::to_string(&value).unwrap_or_default();
                        if ws_sink.send(tokio_tungstenite::tungstenite::Message::Text(text)).await.is_err() {
                            eprintln!("mcp-sctl: WS send failed");
                        }
                    }
                    None => {
                        // Sender dropped — shutting down
                        return;
                    }
                }
            }
        }
    }
}

/// Dispatch an incoming WS message to the appropriate handler.
async fn dispatch_message(
    msg: &Value,
    sessions: &Arc<Mutex<HashMap<String, SessionBuffer>>>,
    n: &WsNotifiers,
    ai_working_sessions: &Arc<Mutex<HashSet<String>>>,
) {
    let msg_type = msg["type"].as_str().unwrap_or("");

    match msg_type {
        "session.stdout" | "session.stderr" | "session.system" => {
            if let Some((session_id, entry)) = parse_output_entry(msg) {
                let mut sessions = sessions.lock().await;
                if let Some(buf) = sessions.get_mut(&session_id) {
                    buf.push(entry);
                }
            }
        }
        "session.exited" => {
            let session_id = msg["session_id"].as_str().unwrap_or("");
            let exit_code = msg["exit_code"].as_i64().map(|c| c as i32);
            if !session_id.is_empty() {
                let mut sessions = sessions.lock().await;
                if let Some(buf) = sessions.get_mut(session_id) {
                    buf.status = SessionStatus::Exited;
                    buf.exit_code = exit_code;
                    // Also push as a system entry
                    buf.push(OutputEntry {
                        seq: buf.last_seq + 1,
                        stream: "system".to_string(),
                        data: format!("Process exited with code {}", exit_code.unwrap_or(-1)),
                        timestamp_ms: 0,
                    });
                }
            }
        }
        "session.started" => {
            *n.start_result.lock().await = Some(msg.clone());
            n.start_notify.notify_waiters();
        }
        "session.listed" => {
            *n.list_result.lock().await = Some(msg.clone());
            n.list_notify.notify_waiters();
        }
        "session.attached" => {
            // Replay entries from attach response into local buffer
            let session_id = msg["session_id"].as_str().unwrap_or("");
            if let Some(entries) = msg["entries"].as_array() {
                let mut sessions = sessions.lock().await;
                if let Some(buf) = sessions.get_mut(session_id) {
                    // Update status from attached response if present
                    if let Some(status) = msg["status"].as_str() {
                        match status {
                            "exited" => {
                                buf.status = SessionStatus::Exited;
                                buf.exit_code = msg["exit_code"].as_i64().map(|c| c as i32);
                            }
                            _ => buf.status = SessionStatus::Running,
                        }
                    }
                    // Update PTY flag if present
                    if let Some(pty) = msg["pty"].as_bool() {
                        buf.is_pty = pty;
                    }
                    for entry_val in entries {
                        if let Some((_, entry)) = parse_output_entry(entry_val) {
                            buf.push(entry);
                        }
                    }
                    buf.attached = true;
                    buf.attach_notify.notify_waiters();
                }
            }
        }
        "session.closed" => {
            let session_id = msg["session_id"].as_str().unwrap_or("");
            if !session_id.is_empty() {
                let mut sessions = sessions.lock().await;
                if let Some(buf) = sessions.get_mut(session_id) {
                    buf.status = SessionStatus::Exited;
                    buf.notify.notify_waiters();
                }
            }
        }
        "error" => {
            let code = msg["code"].as_str().unwrap_or("");
            if code == "AI_NOT_ALLOWED" {
                *n.ai_status_result.lock().await = Some(msg.clone());
                n.ai_status_notify.notify_waiters();
            } else if msg.get("session_id").is_none() {
                // Likely a start error
                *n.start_result.lock().await = Some(msg.clone());
                n.start_notify.notify_waiters();
            }
            eprintln!(
                "mcp-sctl: WS error: {}",
                msg["message"].as_str().unwrap_or("unknown")
            );
        }
        "session.resize.ack" | "session.rename.ack" | "session.allow_ai.ack" => {
            // Acknowledged — no action needed, the tool already returned ok
        }
        "session.ai_status.ack" => {
            // Sync local tracking from the ack
            let session_id = msg["session_id"].as_str().unwrap_or("");
            let working = msg["working"].as_bool().unwrap_or(false);
            if !session_id.is_empty() {
                let mut ai_set = ai_working_sessions.lock().await;
                if working {
                    ai_set.insert(session_id.to_string());
                } else {
                    ai_set.remove(session_id);
                }
            }
            *n.ai_status_result.lock().await = Some(msg.clone());
            n.ai_status_notify.notify_waiters();
        }
        "session.ai_status_changed" => {
            // Sync local AI working tracking from broadcast
            let session_id = msg["session_id"].as_str().unwrap_or("unknown");
            let working = msg["working"].as_bool().unwrap_or(false);
            let mut ai_set = ai_working_sessions.lock().await;
            if working {
                ai_set.insert(session_id.to_string());
            } else {
                ai_set.remove(session_id);
            }
            eprintln!("mcp-sctl: broadcast {msg_type} for session {session_id}");
        }
        "session.created"
        | "session.destroyed"
        | "session.renamed"
        | "session.ai_permission_changed" => {
            // Broadcast events from other clients — log for observability
            let session_id = msg["session_id"].as_str().unwrap_or("unknown");
            eprintln!("mcp-sctl: broadcast {msg_type} for session {session_id}");
        }
        _ => {} // pong, ack, etc.
    }
}

/// Reconnect with exponential backoff. On success, re-attaches all active sessions.
async fn reconnect_loop(
    ws_url: &str,
    sessions: &Arc<Mutex<HashMap<String, SessionBuffer>>>,
) -> Option<(
    futures_util::stream::SplitSink<WsStream, tokio_tungstenite::tungstenite::Message>,
    futures_util::stream::SplitStream<WsStream>,
)> {
    let mut delay = 1u64;
    let max_delay = 30u64;

    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(delay)).await;

        match tokio_tungstenite::connect_async(ws_url).await {
            Ok((ws_stream, _)) => {
                let (mut ws_sink, ws_reader) = ws_stream.split();

                // Re-attach all active sessions
                let sessions_lock = sessions.lock().await;
                for (session_id, buf) in sessions_lock.iter() {
                    if buf.status == SessionStatus::Running {
                        let attach_msg = json!({
                            "type": "session.attach",
                            "session_id": session_id,
                            "since": buf.last_seq,
                        });
                        let text = serde_json::to_string(&attach_msg).unwrap_or_default();
                        if ws_sink
                            .send(tokio_tungstenite::tungstenite::Message::Text(text))
                            .await
                            .is_err()
                        {
                            eprintln!("mcp-sctl: failed to re-attach session {session_id}");
                        }
                    }
                }
                drop(sessions_lock);

                return Some((ws_sink, ws_reader));
            }
            Err(e) => {
                eprintln!("mcp-sctl: reconnect failed: {e}, retrying in {delay}s");
                delay = (delay * 2).min(max_delay);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_buffer_detects_no_gap() {
        let mut buf = SessionBuffer::new();
        buf.push(OutputEntry {
            seq: 1,
            stream: "stdout".into(),
            data: "a".into(),
            timestamp_ms: 0,
        });
        buf.push(OutputEntry {
            seq: 2,
            stream: "stdout".into(),
            data: "b".into(),
            timestamp_ms: 0,
        });
        assert_eq!(buf.dropped_count, 0);
        assert_eq!(buf.last_seq, 2);
    }

    #[test]
    fn session_buffer_detects_gap() {
        let mut buf = SessionBuffer::new();
        buf.push(OutputEntry {
            seq: 1,
            stream: "stdout".into(),
            data: "a".into(),
            timestamp_ms: 0,
        });
        // Skip seq 2, 3, 4 — gap of 3
        buf.push(OutputEntry {
            seq: 5,
            stream: "stdout".into(),
            data: "b".into(),
            timestamp_ms: 0,
        });
        assert_eq!(buf.dropped_count, 3);
        assert_eq!(buf.last_seq, 5);
    }

    #[test]
    fn session_buffer_no_gap_on_first_entry() {
        let mut buf = SessionBuffer::new();
        // First entry at seq 100 — no gap (last_seq is 0)
        buf.push(OutputEntry {
            seq: 100,
            stream: "stdout".into(),
            data: "a".into(),
            timestamp_ms: 0,
        });
        assert_eq!(buf.dropped_count, 0);
    }

    #[test]
    fn session_buffer_cumulative_gaps() {
        let mut buf = SessionBuffer::new();
        buf.push(OutputEntry {
            seq: 1,
            stream: "stdout".into(),
            data: "a".into(),
            timestamp_ms: 0,
        });
        buf.push(OutputEntry {
            seq: 5,
            stream: "stdout".into(),
            data: "b".into(),
            timestamp_ms: 0,
        });
        buf.push(OutputEntry {
            seq: 10,
            stream: "stdout".into(),
            data: "c".into(),
            timestamp_ms: 0,
        });
        // Gap 1→5 = 3, gap 5→10 = 4, total = 7
        assert_eq!(buf.dropped_count, 7);
    }
}
