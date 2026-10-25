//! WebSocket client for streaming sessions with auto-reconnect.
//!
//! [`DeviceWsConnection`] maintains a persistent WebSocket connection to a
//! sctl device. Incoming session output is dispatched to per-session local
//! buffers. MCP tools read from these buffers (zero network latency for
//! buffered output) while all writes go over the WebSocket.
//!
//! On disconnect, the client automatically reconnects with exponential backoff
//! and re-attaches to all active sessions using `session.attach` with the last
//! sequence number the daemon sent, so no output is lost.
//!
//! Each attach sends `dedupes: true`: a session's buffer keeps an entry the
//! daemon sent only when its seq is above the highest it holds, and live
//! output that arrives while an attach waits for its reply is held until the
//! replay is in. The buffer numbers its entries with a counter of its own,
//! which its markers (a process exit, an output gap) take too; reads go by
//! that counter, which follows the daemon's seq where it can.

use std::collections::{HashMap, HashSet, VecDeque};
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

/// A single output entry from a session, as the local buffer keeps it.
#[derive(Debug, Clone)]
pub struct OutputEntry {
    /// The entry's place in the session's local buffer: a counter of the
    /// buffer's own, which the daemon's entries and mcp-sctl's markers both
    /// take, so a read from a `seq` misses neither. A daemon entry takes its
    /// own seq when that is above the last place given, so the two are
    /// equal until a marker comes between.
    pub seq: u64,
    /// The daemon's seq for an entry it sent; `None` for a marker.
    pub daemon_seq: Option<u64>,
    pub stream: String,
    pub data: String,
    pub timestamp_ms: u64,
}

/// An output entry as the daemon sent it, numbered by the daemon.
#[derive(Debug, Clone)]
struct DaemonEntry {
    seq: u64,
    stream: String,
    data: String,
    timestamp_ms: u64,
}

/// What came for a session while its attach waited for the reply.
enum Held {
    Entry(DaemonEntry),
    Marker(String),
}

/// Max entries per session buffer to prevent unbounded memory growth.
const MAX_BUFFER_ENTRIES: usize = 5000;

/// Local buffer for a session's output, held in mcp-sctl's memory.
struct SessionBuffer {
    entries: VecDeque<OutputEntry>,
    status: SessionStatus,
    exit_code: Option<i32>,
    notify: Arc<Notify>,
    /// The last place given to an entry (`OutputEntry::seq`).
    last_place: u64,
    /// The highest seq of the daemon's entries this buffer kept: where a
    /// re-attach starts. Markers do not move it.
    last_seq: u64,
    is_pty: bool,
    /// Count of entries dropped due to ring buffer eviction (detected via sequence gaps).
    dropped_count: u64,
    /// Notify for session.attached responses (used by attach_session).
    attach_notify: Arc<Notify>,
    /// Set to true when a session.attached response has been received.
    attached: bool,
    /// Live output held while an attach waits for its reply: kept after the
    /// replay, in the order it came.
    held: Option<Vec<Held>>,
}

impl SessionBuffer {
    fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            status: SessionStatus::Running,
            exit_code: None,
            notify: Arc::new(Notify::new()),
            last_place: 0,
            last_seq: 0,
            is_pty: false,
            dropped_count: 0,
            attach_notify: Arc::new(Notify::new()),
            attached: false,
            held: None,
        }
    }

    /// Keep an entry the daemon sent, unless its seq is not above the
    /// highest kept: an attach's replay and the live stream may both carry
    /// it.
    fn push(&mut self, entry: DaemonEntry) {
        if entry.seq <= self.last_seq {
            return;
        }
        // Detect sequence gaps (ring buffer eviction on the daemon side)
        if self.last_seq > 0 && entry.seq > self.last_seq + 1 {
            self.dropped_count += entry.seq - self.last_seq - 1;
        }
        self.last_seq = entry.seq;
        self.keep(
            Some(entry.seq),
            entry.stream,
            entry.data,
            entry.timestamp_ms,
        );
    }

    /// A line mcp-sctl writes itself (a process exit, an output gap). It
    /// takes a place and no seq of the daemon's, so the daemon's next entry
    /// is still kept and a re-attach still starts after the daemon's last.
    fn mark(&mut self, data: String) {
        self.keep(None, "system".to_string(), data, 0);
    }

    fn keep(&mut self, daemon_seq: Option<u64>, stream: String, data: String, timestamp_ms: u64) {
        self.last_place = (self.last_place + 1).max(daemon_seq.unwrap_or(0));
        if self.entries.len() >= MAX_BUFFER_ENTRIES {
            self.entries.pop_front();
        }
        self.entries.push_back(OutputEntry {
            seq: self.last_place,
            daemon_seq,
            stream,
            data,
            timestamp_ms,
        });
        self.notify.notify_waiters();
    }

    /// Live output: kept now, or held while an attach waits for its reply.
    fn live(&mut self, entry: DaemonEntry) {
        match &mut self.held {
            Some(held) => held.push(Held::Entry(entry)),
            None => self.push(entry),
        }
    }

    /// A marker, held behind live output while an attach waits.
    fn live_mark(&mut self, data: String) {
        match &mut self.held {
            Some(held) => held.push(Held::Marker(data)),
            None => self.mark(data),
        }
    }

    /// An attach is about to be sent from `since`: hold live output until
    /// its reply. The buffer holds what the attach skips.
    fn hold(&mut self, since: u64) {
        self.last_seq = self.last_seq.max(since);
        self.held.get_or_insert_with(Vec::new);
    }

    /// The attach answered, or will not: its replay, then what was held
    /// meanwhile, each of the daemon's entries kept only when its seq is
    /// above the highest kept.
    fn settle(&mut self, replay: impl IntoIterator<Item = DaemonEntry>) {
        for entry in replay {
            self.push(entry);
        }
        for held in self.held.take().unwrap_or_default() {
            match held {
                Held::Entry(entry) => self.push(entry),
                Held::Marker(data) => self.mark(data),
            }
        }
    }

    /// The entries an attach from `since` answers: those after the last of
    /// the daemon's entries at or below `since`, markers included.
    fn after_daemon_seq(&self, since: u64) -> Vec<OutputEntry> {
        let start = self
            .entries
            .iter()
            .rposition(|e| e.daemon_seq.is_some_and(|seq| seq <= since))
            .map_or(0, |i| i + 1);
        self.entries.iter().skip(start).cloned().collect()
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

        // Register waiter BEFORE sending to avoid Notify race: if the I/O
        // loop receives the response between send() and notified(), the
        // notify_waiters() call would find no waiters and the notification
        // would be lost, causing a spurious 10s timeout.
        let notified = self.notifiers.start_notify.notified();
        self.send(msg).await?;

        let result = tokio::time::timeout(tokio::time::Duration::from_secs(10), notified).await;

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

        let notified = self.notifiers.ai_status_notify.notified();
        self.send(msg).await?;

        let result = tokio::time::timeout(tokio::time::Duration::from_secs(5), notified).await;

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

        let notified = self.notifiers.list_notify.notified();
        self.send(json!({ "type": "session.list" })).await?;

        let result = tokio::time::timeout(tokio::time::Duration::from_secs(10), notified).await;

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
    /// `session.attached` response with replayed entries. `since` is the
    /// daemon's seq; the entries returned carry the buffer's own (see
    /// [`OutputEntry::seq`]), which [`Self::read_output`] takes.
    pub async fn attach_session(&self, session_id: &str, since: u64) -> Result<ReadResult, String> {
        // Ensure local buffer exists (may be new MCP process that doesn't know this session)
        let attach_notify = {
            let mut sessions = self.sessions.lock().await;
            let buf = sessions
                .entry(session_id.to_string())
                .or_insert_with(SessionBuffer::new);
            buf.attached = false;
            buf.hold(since);
            Arc::clone(&buf.attach_notify)
        };

        // Register waiter before sending to avoid Notify race
        let notified = attach_notify.notified();
        let sent = self.send(attach_message(session_id, since)).await;
        let answered = sent.is_ok()
            && tokio::time::timeout(tokio::time::Duration::from_secs(10), notified)
                .await
                .is_ok();
        if !answered {
            // Nothing more is held for a reply that will not come.
            if let Some(buf) = self.sessions.lock().await.get_mut(session_id) {
                buf.settle(Vec::new());
            }
            sent?;
            return Err("Timeout waiting for session.attached response".to_string());
        }

        // Read the replayed entries
        let sessions = self.sessions.lock().await;
        let buf = sessions
            .get(session_id)
            .ok_or_else(|| format!("Session {session_id} not found after attach"))?;

        Ok(ReadResult {
            entries: buf.after_daemon_seq(since),
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
        const DISCONNECT_GRACE_SECS: u64 = 10;

        let nonce = format!(
            "{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let start_marker = format!("__SCTL_{nonce}_START__");
        let done_marker = format!("__SCTL_{nonce}_DONE_");

        // Wrap command with start and done markers.
        // The start marker lets us reliably skip the PTY echo of the command.
        let wrapped = format!(
            "printf '{start_marker}\\n'; {command} ; printf '\\n{done_marker}%s__\\n' \"$?\""
        );

        // Record current position before sending
        let start_seq = {
            let sessions = self.sessions.lock().await;
            let buf = sessions
                .get(session_id)
                .ok_or_else(|| format!("Session {session_id} not found locally"))?;
            buf.last_place
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
        let mut disconnected_since: Option<tokio::time::Instant> = None;

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
                        "WebSocket disconnected for {DISCONNECT_GRACE_SECS}s during exec_wait"
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

            // Scan for done marker (e.g. __SCTL_<nonce>_DONE_0__)
            // The PTY echo contains _DONE_%s__ (printf format specifier).
            // We distinguish the real marker by checking that the char after
            // the prefix is a digit (exit code), not '%' or other chars.
            let done_pos = {
                let mut end = accumulated.len();
                loop {
                    match accumulated[..end].rfind(&done_marker) {
                        Some(pos) => {
                            let after = pos + done_marker.len();
                            if after < accumulated.len()
                                && accumulated.as_bytes()[after].is_ascii_digit()
                            {
                                break Some(pos);
                            }
                            // Not the real marker (echo) — keep searching earlier
                            end = pos;
                        }
                        None => break None,
                    }
                }
            };

            if let Some(done_pos) = done_pos {
                // Extract exit code: __SCTL_<nonce>_DONE_<code>__
                let after_done = &accumulated[done_pos + done_marker.len()..];
                let exit_code = after_done
                    .split("__")
                    .next()
                    .and_then(|s| s.trim().parse::<i32>().ok());

                // Find output boundaries using start marker.
                // The start marker is printed BEFORE the command runs, so
                // real output is between start_marker\n and \n before done_marker.
                // Use rfind within the region before done_pos to skip the
                // PTY echo (which also contains the start marker text).
                let output_start = accumulated[..done_pos].rfind(&start_marker).map_or(0, |p| {
                    // Skip past the start marker and its trailing \n
                    let after = p + start_marker.len();
                    accumulated[after..done_pos]
                        .find('\n')
                        .map_or(after, |nl| after + nl + 1)
                });

                let output_end = accumulated[..done_pos].rfind('\n').unwrap_or(done_pos);

                let output = if output_start <= output_end {
                    accumulated[output_start..output_end].to_string()
                } else {
                    String::new()
                };

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

/// Parse an incoming WS message into a `DaemonEntry` if it's a session output message.
fn parse_output_entry(msg: &Value) -> Option<(String, DaemonEntry)> {
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
        DaemonEntry {
            seq: msg["seq"].as_u64().unwrap_or(0),
            stream: stream.to_string(),
            data: msg["data"].as_str().unwrap_or("").to_string(),
            timestamp_ms: msg["timestamp_ms"].as_u64().unwrap_or(0),
        },
    ))
}

/// `session.attach` from `since`, the daemon's seq. `dedupes`: the buffer
/// keeps an entry only when its seq is above the highest it holds, and
/// holds live output until the reply, so the daemon may keep the output
/// stream other clients share.
fn attach_message(session_id: &str, since: u64) -> Value {
    json!({
        "type": "session.attach",
        "session_id": session_id,
        "since": since,
        "dedupes": true,
    })
}

/// The attaches a new connection sends, one per running session, from the
/// daemon's last seq the session's buffer holds; each buffer then holds
/// live output until its reply.
fn reattach_messages(sessions: &mut HashMap<String, SessionBuffer>) -> Vec<Value> {
    sessions
        .iter_mut()
        .filter(|(_, buf)| buf.status == SessionStatus::Running)
        .map(|(session_id, buf)| {
            buf.hold(buf.last_seq);
            attach_message(session_id, buf.last_seq)
        })
        .collect()
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
                            eprintln!("mcp-sctl: WS send failed, reconnecting...");
                            connected.store(false, Ordering::SeqCst);

                            if let Some((new_sink, new_reader)) = reconnect_loop(
                                &ws_url,
                                &sessions,
                            ).await {
                                ws_sink = new_sink;
                                ws_reader = new_reader;
                                connected.store(true, Ordering::SeqCst);
                                eprintln!("mcp-sctl: WebSocket reconnected after send failure");
                            } else {
                                return;
                            }
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
                    buf.live(entry);
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
                    // Also a system line, after the output it ends
                    buf.live_mark(format!(
                        "Process exited with code {}",
                        exit_code.unwrap_or(-1)
                    ));
                    buf.notify.notify_waiters();
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
                    // The replay, then the live output held meanwhile.
                    buf.settle(
                        entries
                            .iter()
                            .filter_map(|e| parse_output_entry(e).map(|(_, entry)| entry)),
                    );
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
            let error_msg = msg["message"].as_str().unwrap_or("unknown");

            if code == "AI_NOT_ALLOWED" {
                *n.ai_status_result.lock().await = Some(msg.clone());
                n.ai_status_notify.notify_waiters();
            } else if let Some(session_id) = msg.get("session_id").and_then(Value::as_str) {
                // Error targeting a specific session (e.g. attach to a session
                // that no longer exists after device reboot). Mark it dead.
                let mut sessions = sessions.lock().await;
                if let Some(buf) = sessions.get_mut(session_id) {
                    buf.status = SessionStatus::Exited;
                    buf.exit_code = None;
                    // No replay follows: keep what an attach held.
                    buf.settle(Vec::new());
                    buf.notify.notify_waiters();
                    buf.attach_notify.notify_waiters();
                }
                eprintln!("mcp-sctl: session {session_id} error: {error_msg}");
            } else {
                // Likely a start error
                *n.start_result.lock().await = Some(msg.clone());
                n.start_notify.notify_waiters();
            }

            if code != "AI_NOT_ALLOWED" {
                eprintln!("mcp-sctl: WS error: {error_msg}");
            }
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
        "session.gap" => {
            // Relay dropped session output due to backpressure.
            // Re-attach with last known seq to recover missed output.
            let session_id = msg["session_id"].as_str().unwrap_or("");
            let reason = msg["reason"].as_str().unwrap_or("unknown");
            eprintln!("mcp-sctl: session.gap for {session_id} (reason: {reason}), will re-attach");
            if !session_id.is_empty() {
                let last_seq = {
                    let sessions = sessions.lock().await;
                    sessions.get(session_id).map_or(0, |b| b.last_seq)
                };
                // Queue a re-attach. We can't call self.attach_session here since
                // we don't have the sender. Instead, synthesize a system entry so
                // the consumer knows output may have been lost. The marker takes
                // no seq of the daemon's, so the entry that follows the gap is
                // still kept.
                let mut sessions = sessions.lock().await;
                if let Some(buf) = sessions.get_mut(session_id) {
                    buf.live_mark(format!(
                        "[output gap detected: {reason}, last_seq={last_seq}]"
                    ));
                }
            }
        }
        "session.created"
        | "session.destroyed"
        | "session.renamed"
        | "session.ai_permission_changed" => {
            // Broadcast events from other clients — log for observability
            let session_id = msg["session_id"].as_str().unwrap_or("unknown");
            eprintln!("mcp-sctl: broadcast {msg_type} for session {session_id}");
        }
        // pong plus the acks that need no action (resize/rename/allow_ai —
        // the tool already returned ok).
        _ => {}
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
                let mut sessions_lock = sessions.lock().await;
                for attach_msg in reattach_messages(&mut sessions_lock) {
                    let session_id = attach_msg["session_id"].as_str().unwrap_or("");
                    let text = serde_json::to_string(&attach_msg).unwrap_or_default();
                    if ws_sink
                        .send(tokio_tungstenite::tungstenite::Message::Text(text))
                        .await
                        .is_err()
                    {
                        eprintln!("mcp-sctl: failed to re-attach session {session_id}");
                        // No reply will come: hold nothing for it.
                        if let Some(buf) = sessions_lock.get_mut(session_id) {
                            buf.settle(Vec::new());
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

    /// An entry the daemon sent: `line <seq>`.
    fn line(seq: u64) -> DaemonEntry {
        DaemonEntry {
            seq,
            stream: "stdout".into(),
            data: format!("line {seq}\n"),
            timestamp_ms: 0,
        }
    }

    fn data(buf: &SessionBuffer) -> Vec<String> {
        buf.entries.iter().map(|e| e.data.clone()).collect()
    }

    fn places(entries: &[OutputEntry]) -> Vec<u64> {
        entries.iter().map(|e| e.seq).collect()
    }

    #[test]
    fn session_buffer_detects_no_gap() {
        let mut buf = SessionBuffer::new();
        buf.push(line(1));
        buf.push(line(2));
        assert_eq!(buf.dropped_count, 0);
        assert_eq!(buf.last_seq, 2);
    }

    #[test]
    fn session_buffer_detects_gap() {
        let mut buf = SessionBuffer::new();
        buf.push(line(1));
        // Skip seq 2, 3, 4 — gap of 3
        buf.push(line(5));
        assert_eq!(buf.dropped_count, 3);
        assert_eq!(buf.last_seq, 5);
    }

    #[test]
    fn session_buffer_no_gap_on_first_entry() {
        let mut buf = SessionBuffer::new();
        // First entry at seq 100 — no gap (last_seq is 0)
        buf.push(line(100));
        assert_eq!(buf.dropped_count, 0);
    }

    #[test]
    fn session_buffer_cumulative_gaps() {
        let mut buf = SessionBuffer::new();
        buf.push(line(1));
        buf.push(line(5));
        buf.push(line(10));
        // Gap 1→5 = 3, gap 5→10 = 4, total = 7
        assert_eq!(buf.dropped_count, 7);
    }

    #[test]
    fn a_gap_marker_and_the_real_next_entry_are_both_kept() {
        let mut buf = SessionBuffer::new();
        buf.push(line(1));
        buf.push(line(2));
        buf.mark("[output gap detected: backpressure, last_seq=2]".into());
        buf.push(line(3));
        assert_eq!(
            data(&buf),
            [
                "line 1\n",
                "line 2\n",
                "[output gap detected: backpressure, last_seq=2]",
                "line 3\n"
            ]
        );
        // The marker takes a place of the buffer's own and no seq of the
        // daemon's.
        assert_eq!(places(buf.entries.make_contiguous()), [1, 2, 3, 4]);
        assert_eq!(buf.entries[2].daemon_seq, None);
        assert_eq!(buf.entries[3].daemon_seq, Some(3));
        assert_eq!(buf.last_seq, 3);
        assert_eq!(buf.dropped_count, 0);
    }

    #[test]
    fn an_exit_marker_moves_no_seq_of_the_daemons() {
        let mut buf = SessionBuffer::new();
        buf.push(line(1));
        buf.mark("Process exited with code 0".into());
        assert_eq!(buf.last_seq, 1);
        assert_eq!(buf.last_place, 2);
    }

    #[test]
    fn a_repeated_entry_is_dropped() {
        let mut buf = SessionBuffer::new();
        buf.push(line(1));
        buf.push(line(2));
        buf.push(line(2));
        buf.push(line(1));
        buf.push(line(3));
        assert_eq!(data(&buf), ["line 1\n", "line 2\n", "line 3\n"]);
        assert_eq!(buf.dropped_count, 0);
    }

    #[test]
    fn a_re_attach_starts_from_the_daemons_last_seq() {
        let mut sessions = HashMap::new();
        let mut buf = SessionBuffer::new();
        buf.push(line(1));
        buf.push(line(2));
        buf.mark("[output gap detected: backpressure, last_seq=2]".into());
        sessions.insert("s1".to_string(), buf);
        let mut exited = SessionBuffer::new();
        exited.push(line(1));
        exited.status = SessionStatus::Exited;
        sessions.insert("s2".to_string(), exited);

        let messages = reattach_messages(&mut sessions);
        assert_eq!(
            messages,
            [json!({
                "type": "session.attach",
                "session_id": "s1",
                "since": 2,
                "dedupes": true,
            })]
        );
        // The re-attached session holds live output until its reply.
        assert!(sessions["s1"].held.is_some());
        assert!(sessions["s2"].held.is_none());
    }

    #[test]
    fn output_held_during_an_attach_is_kept_once_after_the_replay() {
        let mut buf = SessionBuffer::new();
        buf.push(line(1));
        buf.hold(1);
        // Live output before the reply: one entry the replay also carries,
        // then the exit, which follows the output it ends.
        buf.live(line(4));
        buf.live_mark("Process exited with code 0".into());
        assert_eq!(data(&buf), ["line 1\n"]);
        buf.settle([line(2), line(3), line(4)]);
        buf.live(line(4));
        assert_eq!(
            data(&buf),
            [
                "line 1\n",
                "line 2\n",
                "line 3\n",
                "line 4\n",
                "Process exited with code 0"
            ]
        );
        assert!(buf.held.is_none());
        // Live output after the reply is kept as it comes.
        buf.live(line(5));
        assert_eq!(buf.entries.back().unwrap().data, "line 5\n");
    }

    #[test]
    fn an_attach_from_a_seq_skips_what_the_buffer_does_not_ask_for() {
        // A new buffer attached from the daemon's seq 10: an old batch at
        // or below it, held before the reply, is not kept.
        let mut buf = SessionBuffer::new();
        buf.hold(10);
        buf.live(line(9));
        buf.settle([line(11), line(12)]);
        assert_eq!(data(&buf), ["line 11\n", "line 12\n"]);
        assert_eq!(buf.dropped_count, 0);
    }

    #[tokio::test]
    async fn the_reader_keeps_a_replay_and_the_live_output_around_it_once() {
        let notifiers = WsNotifiers {
            start_notify: Arc::new(Notify::new()),
            start_result: Arc::new(Mutex::new(None)),
            list_notify: Arc::new(Notify::new()),
            list_result: Arc::new(Mutex::new(None)),
            ai_status_notify: Arc::new(Notify::new()),
            ai_status_result: Arc::new(Mutex::new(None)),
        };
        let ai = Arc::new(Mutex::new(HashSet::new()));
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        let mut buf = SessionBuffer::new();
        buf.hold(0);
        sessions.lock().await.insert("s1".to_string(), buf);
        let output = |seq: u64| {
            json!({
                "type": "session.stdout",
                "session_id": "s1",
                "data": format!("line {seq}\n"),
                "seq": seq,
                "timestamp_ms": 0,
            })
        };

        // A batch the shared stream sent before the reply, which the
        // replay also carries.
        dispatch_message(&output(3), &sessions, &notifiers, &ai).await;
        let entries: Vec<Value> = (1..=3).map(output).collect();
        let attached = json!({
            "type": "session.attached",
            "session_id": "s1",
            "entries": entries,
            "dropped": 0,
        });
        dispatch_message(&attached, &sessions, &notifiers, &ai).await;
        dispatch_message(&output(3), &sessions, &notifiers, &ai).await;
        dispatch_message(&output(4), &sessions, &notifiers, &ai).await;

        let sessions = sessions.lock().await;
        let buf = &sessions["s1"];
        assert_eq!(data(buf), ["line 1\n", "line 2\n", "line 3\n", "line 4\n"]);
        assert!(buf.attached);
        assert!(buf.held.is_none());
    }

    #[test]
    fn an_attach_answers_from_the_last_daemon_entry_at_or_below_since() {
        let mut buf = SessionBuffer::new();
        buf.push(line(1));
        buf.push(line(2));
        buf.mark("[output gap detected: backpressure, last_seq=2]".into());
        buf.push(line(3));
        assert_eq!(places(&buf.after_daemon_seq(0)), [1, 2, 3, 4]);
        assert_eq!(places(&buf.after_daemon_seq(2)), [3, 4]);
        assert_eq!(places(&buf.after_daemon_seq(3)), Vec::<u64>::new());
        // From a seq the buffer never held, all of it.
        let mut fresh = SessionBuffer::new();
        fresh.hold(10);
        fresh.settle([line(11), line(12)]);
        assert_eq!(places(&fresh.after_daemon_seq(10)), [11, 12]);
    }

    #[test]
    fn places_follow_the_daemons_seq_where_they_can() {
        let mut buf = SessionBuffer::new();
        buf.push(line(5));
        buf.push(line(6));
        // A marker takes the next place; the daemon's next entry the one
        // after it.
        buf.mark("[output gap detected: backpressure, last_seq=6]".into());
        buf.push(line(7));
        // Entries the relay dropped: the daemon's next seq is ahead again.
        buf.push(line(12));
        assert_eq!(places(buf.entries.make_contiguous()), [5, 6, 7, 8, 12]);
    }
}
