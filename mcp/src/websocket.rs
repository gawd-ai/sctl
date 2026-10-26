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
//! which its markers (a process exit, an output gap) take too; reads and
//! attaches go by that counter, which follows the daemon's seq where it can.
//! An attach asks the daemon for its entries above the highest the buffer
//! holds.
//!
//! A session is closed to AI clients until a person opens it with the Allow
//! AI control in a console (ADR-013). This client starts every session
//! closed, and types or signals only into a session the agent confirms a
//! person opened: it asks for the AI status `working: true` first, which
//! the agent answers only for such a session, and sends nothing to the
//! session on any other answer or on none. Each AI status request carries
//! its own `request_id`, which the agent echoes and the relay restores, and
//! only the call that sent it takes the answer. A late ack that set the AI
//! working for a call that stopped waiting is undone with `working: false`,
//! so a person's console does not stay locked by a call that refused.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, Mutex, Notify};
use tokio::time::{Duration, Instant};

/// How long an AI status request waits for the agent's answer.
const AI_STATUS_WAIT: Duration = Duration::from_secs(5);

/// How many AI status calls that stopped waiting are remembered, so that a
/// late ack for one of them is undone and never taken as another's answer.
const ABANDONED_KEPT: usize = 64;

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

/// How long an attach waits for its reply, and so how long live output is
/// held for one.
const ATTACH_REPLY_WAIT: Duration = Duration::from_secs(10);

/// Live output held while an attach waits for its reply.
struct Holding {
    /// What came, in the order it came.
    held: Vec<Held>,
    /// When the hold ends if no reply has come.
    until: Instant,
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
    /// replay, or when the hold ends with no reply, in the order it came.
    held: Option<Holding>,
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

    /// Keep an entry at the next place, or at the daemon's seq when that
    /// is further on (`None` for a marker).
    fn keep(&mut self, daemon_seq: Option<u64>, stream: String, data: String, timestamp_ms: u64) {
        self.last_place = (self.last_place + 1).max(daemon_seq.unwrap_or(0));
        if self.entries.len() >= MAX_BUFFER_ENTRIES {
            self.entries.pop_front();
        }
        self.entries.push_back(OutputEntry {
            seq: self.last_place,
            stream,
            data,
            timestamp_ms,
        });
        self.notify.notify_waiters();
    }

    /// Live output: kept now, or held while an attach waits for its reply.
    fn live(&mut self, entry: DaemonEntry) {
        self.end_overdue_hold();
        match &mut self.held {
            Some(holding) => holding.held.push(Held::Entry(entry)),
            None => self.push(entry),
        }
    }

    /// A marker, held behind live output while an attach waits.
    fn live_mark(&mut self, data: String) {
        self.end_overdue_hold();
        match &mut self.held {
            Some(holding) => holding.held.push(Held::Marker(data)),
            None => self.mark(data),
        }
    }

    /// An attach is about to be sent: hold live output until its reply,
    /// `ATTACH_REPLY_WAIT` from now at most.
    fn hold(&mut self) {
        let until = Instant::now() + ATTACH_REPLY_WAIT;
        self.held
            .get_or_insert_with(|| Holding {
                held: Vec::new(),
                until,
            })
            .until = until;
    }

    /// End a hold whose reply has not come in time: what it held is kept
    /// now. A reply that comes later is still kept where its seqs are above
    /// the highest kept.
    fn end_overdue_hold(&mut self) {
        if self
            .held
            .as_ref()
            .is_some_and(|holding| Instant::now() >= holding.until)
        {
            self.settle(Vec::new());
        }
    }

    /// An attach is about to be sent for a reader at the place `since`:
    /// hold live output until its reply, and return the daemon's seq to
    /// attach from. A buffer that has given no place yet (a session
    /// mcp-sctl has held nothing for, as after a restart) takes `since` as
    /// the daemon's seq, which its places would follow, and counts itself
    /// as holding up to it. Any other buffer attaches from the daemon's
    /// last seq it kept: the daemon's entries at or below it are dropped as
    /// repeats, so asking for them would change nothing.
    fn attach_from(&mut self, since: u64) -> u64 {
        if self.last_place == 0 {
            self.last_seq = since;
            self.last_place = since;
        }
        self.hold();
        self.last_seq
    }

    /// The attach answered, or will not: its replay, then what was held
    /// meanwhile, each of the daemon's entries kept only when its seq is
    /// above the highest kept.
    fn settle(&mut self, replay: impl IntoIterator<Item = DaemonEntry>) {
        for entry in replay {
            self.push(entry);
        }
        for held in self
            .held
            .take()
            .map(|holding| holding.held)
            .unwrap_or_default()
        {
            match held {
                Held::Entry(entry) => self.push(entry),
                Held::Marker(data) => self.mark(data),
            }
        }
    }

    /// The entries after the place `since`, markers included.
    fn after(&self, since: u64) -> Vec<OutputEntry> {
        self.entries
            .iter()
            .filter(|e| e.seq > since)
            .cloned()
            .collect()
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
    ai_status: Mutex<AiStatusCalls>,
}

/// The AI status requests of one connection (ADR-013). Each is sent with a
/// `request_id` of its own, which the agent echoes on its ack and on its
/// `AI_NOT_ALLOWED` refusal (every agent version does) and the relay
/// restores, so an answer goes only to the call that asked.
#[derive(Default)]
struct AiStatusCalls {
    /// The number in the last call's `request_id`.
    last: u64,
    /// The calls waiting for their answer, by `request_id`: the session
    /// each asked about, and where its answer goes.
    waiting: HashMap<String, (String, oneshot::Sender<Value>)>,
    /// The `working: true` calls that stopped waiting with no answer, as
    /// (`request_id`, session), oldest first, at most [`ABANDONED_KEPT`].
    abandoned: VecDeque<(String, String)>,
}

impl AiStatusCalls {
    /// A new call about `session_id`: its `request_id` and its answer.
    fn ask(&mut self, session_id: &str) -> (String, oneshot::Receiver<Value>) {
        self.last += 1;
        let request_id = format!("mcp-ai-status-{}", self.last);
        let (answer, answered) = oneshot::channel();
        self.waiting
            .insert(request_id.clone(), (session_id.to_string(), answer));
        (request_id, answered)
    }

    /// The call `request_id` stopped waiting with no answer. A `working:
    /// true` call is remembered, so that its late ack is undone.
    fn give_up(&mut self, request_id: &str, session_id: &str, working: bool) {
        self.waiting.remove(request_id);
        if working {
            if self.abandoned.len() == ABANDONED_KEPT {
                self.abandoned.pop_front();
            }
            self.abandoned
                .push_back((request_id.to_string(), session_id.to_string()));
        }
    }

    /// Whether a call about `session_id` is waiting for its answer.
    fn waits_on(&self, session_id: &str) -> bool {
        self.waiting.values().any(|(asked, _)| asked == session_id)
    }

    /// Route the agent's answer `msg` to the call that sent its
    /// `request_id`, and to no other. Returns a frame to send: `working:
    /// false` for the session of a call that stopped waiting, when its late
    /// ack set the AI working there and no other call on that session waits
    /// for its own answer, which then decides. Any other answer is dropped.
    fn answer(&mut self, msg: &Value) -> Option<Value> {
        let request_id = msg["request_id"].as_str()?;
        if let Some((_, answer)) = self.waiting.remove(request_id) {
            let _ = answer.send(msg.clone());
            return None;
        }
        let at = self.abandoned.iter().position(|(id, _)| id == request_id)?;
        let (_, session_id) = self.abandoned.remove(at)?;
        let set_working = msg["type"] == "session.ai_status.ack"
            && msg["session_id"] == session_id.as_str()
            && msg["working"] == true;
        (set_working && !self.waits_on(&session_id))
            .then(|| ai_status_message(&session_id, false, None, None))
    }
}

/// `session.ai_status` for `session_id`.
fn ai_status_message(
    session_id: &str,
    working: bool,
    activity: Option<&str>,
    message: Option<&str>,
) -> Value {
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
    msg
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
            ai_status: Mutex::new(AiStatusCalls::default()),
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
    /// The session starts closed to AI clients: this client never asks for
    /// one open to them, since a person opens a session (ADR-013).
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
    ) -> Result<Value, String> {
        // Clear any stale start result
        *self.notifiers.start_result.lock().await = None;

        let mut msg = json!({
            "type": "session.start",
            "persistent": persistent,
            "user_allows_ai": false,
        });
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
    ///
    /// The request carries a `request_id` of its own, and only the answer
    /// that echoes it is this call's ([`AiStatusCalls`]). With no answer
    /// within [`AI_STATUS_WAIT`] the call gives up; if its `working: true`
    /// is acked later, the connection undoes it.
    pub async fn set_ai_status(
        &self,
        session_id: &str,
        working: bool,
        activity: Option<&str>,
        message: Option<&str>,
    ) -> Result<Value, String> {
        let (request_id, mut answered) = self.notifiers.ai_status.lock().await.ask(session_id);
        let mut msg = ai_status_message(session_id, working, activity, message);
        msg["request_id"] = json!(request_id);
        if let Err(e) = self.send(msg).await {
            self.notifiers
                .ai_status
                .lock()
                .await
                .waiting
                .remove(&request_id);
            return Err(e);
        }

        let answer = if let Ok(answer) = tokio::time::timeout(AI_STATUS_WAIT, &mut answered).await {
            answer.ok()
        } else {
            let mut calls = self.notifiers.ai_status.lock().await;
            if calls.waiting.contains_key(&request_id) {
                calls.give_up(&request_id, session_id, working);
                return Err("Timeout waiting for session.ai_status response".to_string());
            }
            // Answered as the wait ended: the answer is in.
            answered.try_recv().ok()
        };
        match answer {
            Some(v) if v["type"] == "error" => Err(v["message"]
                .as_str()
                .unwrap_or("AI status rejected")
                .to_string()),
            Some(v) => Ok(v),
            None => Err("No ai_status response received".to_string()),
        }
    }

    /// `working: false` for `session_id`, with no wait for the ack: the
    /// agent always takes it for a session it holds.
    async fn stop_working(&self, session_id: &str) {
        let msg = ai_status_message(session_id, false, None, None);
        if let Err(e) = self.send(msg).await {
            eprintln!("mcp-sctl: could not clear the AI status of {session_id}: {e}");
        }
        self.clear_ai_working(session_id).await;
    }

    /// Read output from a session's local buffer.
    ///
    /// Returns entries with `seq > since`. If no entries are available, waits
    /// up to `timeout_ms` for new data. Output held for an attach whose
    /// reply is overdue is kept first, and the wait ends when such a hold
    /// does.
    pub async fn read_output(
        &self,
        session_id: &str,
        since: u64,
        timeout_ms: u64,
    ) -> Result<ReadResult, String> {
        let (notify, wait) = {
            let mut sessions = self.sessions.lock().await;
            let buf = sessions
                .get_mut(session_id)
                .ok_or_else(|| format!("Session {session_id} not found locally"))?;
            buf.end_overdue_hold();

            // Return at once with new data, or with none from an exited session
            let entries = buf.after(since);
            if !entries.is_empty() || buf.status == SessionStatus::Exited {
                return Ok(ReadResult::read(buf, entries, since));
            }

            let wait = Duration::from_millis(timeout_ms);
            let wait = buf.held.as_ref().map_or(wait, |holding| {
                wait.min(holding.until.saturating_duration_since(Instant::now()))
            });
            (Arc::clone(&buf.notify), wait)
        };

        // Wait for new data with timeout
        let _ = tokio::time::timeout(wait, notify.notified()).await;

        // Re-read after wait
        let mut sessions = self.sessions.lock().await;
        let buf = sessions
            .get_mut(session_id)
            .ok_or_else(|| format!("Session {session_id} not found locally"))?;
        buf.end_overdue_hold();

        let entries = buf.after(since);
        Ok(ReadResult::read(buf, entries, since))
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

    /// Whether a person opened `session_id` to AI clients, asked of the
    /// agent before anything is typed or signalled into it (ADR-013). The
    /// agent answers the AI status `working: true` with an ack only for
    /// such a session, so the ack is the answer; it also marks the AI
    /// working, as a person watching the console sees. A refusal, an ack
    /// for another session or no answer to this call's own request
    /// ([`Self::set_ai_status`]) refuses, and the caller sends nothing to
    /// the session.
    pub async fn require_open_to_ai(&self, session_id: &str, activity: &str) -> Result<(), String> {
        let why = match self
            .set_ai_status(session_id, true, Some(activity), None)
            .await
        {
            Ok(ack)
                if ack["type"] == "session.ai_status.ack"
                    && ack["session_id"] == session_id
                    && ack["working"] == true =>
            {
                self.mark_ai_working(session_id).await;
                return Ok(());
            }
            Ok(other) => format!("its answer was not for this session: {other}"),
            Err(e) => e,
        };
        self.clear_ai_working(session_id).await;
        Err(format!(
            "Nothing was sent to session {session_id}: the agent did not confirm that a person \
             opened it to AI clients ({why}). A person opens a session to AI with the Allow AI \
             control in its console."
        ))
    }

    /// `session.exec` into a session a person opened to AI clients, else
    /// nothing is sent ([`Self::require_open_to_ai`]).
    pub async fn exec(&self, session_id: &str, command: &str) -> Result<(), String> {
        self.require_open_to_ai(session_id, "write").await?;
        self.send(json!({
            "type": "session.exec",
            "session_id": session_id,
            "command": command,
        }))
        .await
    }

    /// `session.stdin` into a session a person opened to AI clients, else
    /// nothing is sent ([`Self::require_open_to_ai`]).
    pub async fn send_input(&self, session_id: &str, data: &str) -> Result<(), String> {
        self.require_open_to_ai(session_id, "write").await?;
        self.send(json!({
            "type": "session.stdin",
            "session_id": session_id,
            "data": data,
        }))
        .await
    }

    /// `session.signal` to a session a person opened to AI clients, else
    /// nothing is sent ([`Self::require_open_to_ai`]): a Ctrl-C reaches the
    /// person's command as a keystroke does. A signal is one keystroke, so
    /// it leaves the AI status as it found it: where the AI was not
    /// working, `working: false` follows the signal, and the person's
    /// console takes keys again at once.
    pub async fn signal(&self, session_id: &str, signal: i64) -> Result<(), String> {
        let was_working = self.is_ai_working(session_id).await;
        self.require_open_to_ai(session_id, "write").await?;
        let sent = self
            .send(json!({
                "type": "session.signal",
                "session_id": session_id,
                "signal": signal,
            }))
            .await;
        if !was_working {
            self.stop_working(session_id).await;
        }
        sent
    }

    /// Auto-set AI working status if not already working.
    /// Sends `session.ai_status` with `working=true` to the server.
    /// On failure (e.g. AI_NOT_ALLOWED), logs but does not propagate the
    /// error: a read needs no person's switch. Typing and signals go
    /// through [`Self::require_open_to_ai`] instead.
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
    /// `session.attached` response with replayed entries. `since` is a
    /// place in the buffer (see [`OutputEntry::seq`]), as
    /// [`Self::read_output`] takes it; the daemon is asked for its entries
    /// above the highest the buffer holds (see `SessionBuffer::attach_from`).
    /// Answers the entries after `since`, and where to read on from.
    pub async fn attach_session(&self, session_id: &str, since: u64) -> Result<ReadResult, String> {
        // Ensure local buffer exists (may be new MCP process that doesn't know this session)
        let (attach_notify, from) = {
            let mut sessions = self.sessions.lock().await;
            let buf = sessions
                .entry(session_id.to_string())
                .or_insert_with(SessionBuffer::new);
            buf.attached = false;
            let from = buf.attach_from(since);
            (Arc::clone(&buf.attach_notify), from)
        };

        // Register waiter before sending to avoid Notify race
        let notified = attach_notify.notified();
        let sent = self.send(attach_message(session_id, from)).await;
        let answered = sent.is_ok()
            && tokio::time::timeout(ATTACH_REPLY_WAIT, notified)
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

        // With nothing after `since`, read on from the buffer's last place,
        // which is `since` unless `since` is a place this buffer never gave.
        Ok(ReadResult::read(buf, buf.after(since), buf.last_place))
    }

    /// Execute a command and wait for it to complete, returning all output.
    ///
    /// Wraps the command with a unique end marker, then polls for output until
    /// the marker is found or the timeout expires. Sends the command only into
    /// a session a person opened to AI clients ([`Self::require_open_to_ai`]).
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

        self.require_open_to_ai(session_id, "write").await?;

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

/// Result of a `read_output` or `attach_session` call.
pub struct ReadResult {
    pub entries: Vec<OutputEntry>,
    /// The place to read on from: the last entry's, else what the call
    /// gave for none.
    pub last_seq: u64,
    pub status: SessionStatus,
    pub exit_code: Option<i32>,
    /// Number of entries dropped due to daemon ring buffer eviction.
    pub dropped_count: u64,
}

impl ReadResult {
    /// `entries` read from `buf`, to read on from `none` when there are none.
    fn read(buf: &SessionBuffer, entries: Vec<OutputEntry>, none: u64) -> Self {
        Self {
            last_seq: entries.last().map_or(none, |e| e.seq),
            entries,
            status: buf.status,
            exit_code: buf.exit_code,
            dropped_count: buf.dropped_count,
        }
    }
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
            buf.hold();
            attach_message(session_id, buf.last_seq)
        })
        .collect()
}

/// Send a new connection's re-attaches, one per running session. A
/// re-attach that cannot be sent holds nothing: no reply will come for it.
async fn reattach<S>(sink: &mut S, sessions: &mut HashMap<String, SessionBuffer>)
where
    S: futures_util::Sink<tokio_tungstenite::tungstenite::Message> + Unpin,
{
    for attach_msg in reattach_messages(sessions) {
        let session_id = attach_msg["session_id"].as_str().unwrap_or("");
        let text = serde_json::to_string(&attach_msg).unwrap_or_default();
        if sink
            .send(tokio_tungstenite::tungstenite::Message::Text(text))
            .await
            .is_err()
        {
            eprintln!("mcp-sctl: failed to re-attach session {session_id}");
            if let Some(buf) = sessions.get_mut(session_id) {
                buf.settle(Vec::new());
            }
        }
    }
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
                            let reply = dispatch_message(
                                &parsed,
                                &sessions,
                                &notifiers,
                                &ai_working_sessions,
                            ).await;
                            // A frame that cannot go is lost with the
                            // connection, which the reader then sees closed.
                            if let Some(reply) = reply {
                                let text = serde_json::to_string(&reply).unwrap_or_default();
                                if ws_sink.send(tokio_tungstenite::tungstenite::Message::Text(text)).await.is_err() {
                                    eprintln!("mcp-sctl: failed to send {}", reply["type"]);
                                }
                            }
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

/// Dispatch an incoming WS message to the appropriate handler. Returns a
/// frame the connection sends in reply, if any ([`AiStatusCalls::answer`]).
async fn dispatch_message(
    msg: &Value,
    sessions: &Arc<Mutex<HashMap<String, SessionBuffer>>>,
    n: &WsNotifiers,
    ai_working_sessions: &Arc<Mutex<HashSet<String>>>,
) -> Option<Value> {
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
                // The agent's refusal of an AI status request.
                return n.ai_status.lock().await.answer(msg);
            }
            if let Some(session_id) = msg.get("session_id").and_then(Value::as_str) {
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

            eprintln!("mcp-sctl: WS error: {error_msg}");
        }
        "session.ai_status.ack" => {
            // The call that asked keeps the local tracking from its answer;
            // an ack no call waits for marks nothing.
            return n.ai_status.lock().await.answer(msg);
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
            // The relay dropped session output (backpressure). Nothing is
            // sent here, no re-attach included: the buffer keeps a marker,
            // so a reader knows output may be missing. The marker takes the
            // next place and no seq of the daemon's, so the entry that
            // follows the gap is still kept. A `session_attach` from the
            // marker's place asks the daemon for its entries above
            // `last_seq` and recovers the dropped lines, but only while no
            // later live output has been kept: the buffer keeps a daemon
            // entry only above the highest seq it holds, and the attach
            // asks from there.
            let session_id = msg["session_id"].as_str().unwrap_or("");
            let reason = msg["reason"].as_str().unwrap_or("unknown");
            eprintln!(
                "mcp-sctl: session.gap for {session_id} (reason: {reason}): output was dropped; a session_attach from the gap marker's place recovers it while no later output has been kept"
            );
            if !session_id.is_empty() {
                let last_seq = {
                    let sessions = sessions.lock().await;
                    sessions.get(session_id).map_or(0, |b| b.last_seq)
                };
                let mut sessions = sessions.lock().await;
                if let Some(buf) = sessions.get_mut(session_id) {
                    buf.live_mark(format!(
                        "[output gap detected: {reason}, last_seq={last_seq}]"
                    ));
                }
            }
        }
        "tunnel.device_disconnected" => {
            // The relay lost the device's tunnel: no reply comes to an
            // attach sent before this, so nothing is held for one.
            let reason = msg["reason"].as_str().unwrap_or("unknown");
            eprintln!("mcp-sctl: the relay lost the device (reason: {reason})");
            let mut sessions = sessions.lock().await;
            for buf in sessions.values_mut() {
                if buf.held.is_some() {
                    buf.settle(Vec::new());
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
        // pong plus the acks that need no action (resize, rename: the tool
        // already returned ok).
        _ => {}
    }
    None
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
                reattach(&mut ws_sink, &mut *sessions.lock().await).await;

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

    fn texts(entries: &[OutputEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.data.as_str()).collect()
    }

    fn places(entries: &[OutputEntry]) -> Vec<u64> {
        entries.iter().map(|e| e.seq).collect()
    }

    fn notifiers() -> WsNotifiers {
        WsNotifiers {
            start_notify: Arc::new(Notify::new()),
            start_result: Arc::new(Mutex::new(None)),
            list_notify: Arc::new(Notify::new()),
            list_result: Arc::new(Mutex::new(None)),
            ai_status: Mutex::new(AiStatusCalls::default()),
        }
    }

    /// A connection with no socket: what it sends comes out of the
    /// receiver, and what the device sends goes in through `deliver`.
    fn connection() -> (DeviceWsConnection, mpsc::Receiver<Value>) {
        let (sender, sent) = mpsc::channel(16);
        let ws = DeviceWsConnection {
            sender,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            connected: Arc::new(AtomicBool::new(true)),
            notifiers: Arc::new(notifiers()),
            ai_working_sessions: Arc::new(Mutex::new(HashSet::new())),
        };
        (ws, sent)
    }

    /// A message from the device, through the connection's reader, which
    /// sends the reply it makes, if any.
    async fn deliver(ws: &DeviceWsConnection, msg: &Value) {
        if let Some(reply) =
            dispatch_message(msg, &ws.sessions, &ws.notifiers, &ws.ai_working_sessions).await
        {
            ws.send(reply).await.unwrap();
        }
    }

    /// `line <seq>`, as the daemon sends it for session `s1`.
    fn output(seq: u64) -> Value {
        json!({
            "type": "session.stdout",
            "session_id": "s1",
            "data": format!("line {seq}\n"),
            "seq": seq,
            "timestamp_ms": 0,
        })
    }

    /// `session.attached` for `s1`, replaying the daemon's `lines`.
    fn attached(lines: std::ops::RangeInclusive<u64>) -> Value {
        let entries: Vec<Value> = lines.map(output).collect();
        json!({
            "type": "session.attached",
            "session_id": "s1",
            "entries": entries,
            "dropped": 0,
        })
    }

    /// The daemon's answer to `attach`: its lines above the attach's
    /// `since`, up to `last`.
    fn replay(attach: &Value, last: u64) -> Value {
        attached(attach["since"].as_u64().unwrap() + 1..=last)
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
        assert_eq!(buf.attach_from(1), 1);
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
        assert_eq!(buf.attach_from(10), 10);
        buf.live(line(9));
        buf.settle([line(11), line(12)]);
        assert_eq!(data(&buf), ["line 11\n", "line 12\n"]);
        assert_eq!(buf.dropped_count, 0);
    }

    #[tokio::test]
    async fn the_reader_keeps_a_replay_and_the_live_output_around_it_once() {
        let (ws, _sent) = connection();
        let mut buf = SessionBuffer::new();
        buf.hold();
        ws.sessions.lock().await.insert("s1".to_string(), buf);

        // A batch the shared stream sent before the reply, which the
        // replay also carries.
        deliver(&ws, &output(3)).await;
        deliver(&ws, &attached(1..=3)).await;
        deliver(&ws, &output(3)).await;
        deliver(&ws, &output(4)).await;

        let sessions = ws.sessions.lock().await;
        let buf = &sessions["s1"];
        assert_eq!(data(buf), ["line 1\n", "line 2\n", "line 3\n", "line 4\n"]);
        assert!(buf.attached);
        assert!(buf.held.is_none());
    }

    #[tokio::test]
    async fn an_attach_from_a_marker_asks_for_the_line_the_relay_dropped() {
        // The daemon's lines 1 to 5 are kept; the relay drops 6 to 8 and
        // says so, and the gap marker takes place 6.
        let (ws, mut sent) = connection();
        let mut buf = SessionBuffer::new();
        for seq in 1..=5 {
            buf.push(line(seq));
        }
        buf.mark("[output gap detected: backpressure, last_seq=5]".into());
        ws.sessions.lock().await.insert("s1".to_string(), buf);

        // A reader that holds up to the marker attaches from its place,
        // and the daemon is asked from its own last seq the buffer kept.
        let (answer, ()) = tokio::join!(ws.attach_session("s1", 6), async {
            let attach = sent.recv().await.unwrap();
            assert_eq!(attach["since"], 5);
            deliver(&ws, &replay(&attach, 8)).await;
        });
        let answer = answer.unwrap();
        assert_eq!(texts(&answer.entries), ["line 6\n", "line 7\n", "line 8\n"]);
        assert_eq!(places(&answer.entries), [7, 8, 9]);
        assert_eq!(answer.last_seq, 9);
        assert_eq!(answer.dropped_count, 0);
    }

    /// The gap handler sends nothing: it keeps a marker. A `session_attach`
    /// from the marker's place recovers the dropped lines only while no
    /// later live output has been kept. Once a later line is kept, the
    /// attach asks the daemon from that line's seq, and the dropped lines
    /// are not asked for again.
    #[tokio::test]
    async fn a_gap_sends_nothing_and_later_live_output_ends_its_recovery() {
        let (ws, mut sent) = connection();
        ws.sessions
            .lock()
            .await
            .insert("s1".to_string(), SessionBuffer::new());
        for seq in 1..=5 {
            deliver(&ws, &output(seq)).await;
        }
        // The relay drops 6 to 8 and says so.
        let gap = json!({ "type": "session.gap", "session_id": "s1", "reason": "backpressure" });
        deliver(&ws, &gap).await;
        assert!(sent.try_recv().is_err(), "no attach was sent");
        // Line 9 comes live and is kept before any attach.
        deliver(&ws, &output(9)).await;

        let (answer, ()) = tokio::join!(ws.attach_session("s1", 6), async {
            let attach = sent.recv().await.unwrap();
            assert_eq!(attach["since"], 9, "asked from the line kept, not the gap");
            deliver(&ws, &replay(&attach, 9)).await;
        });
        let answer = answer.unwrap();
        assert_eq!(texts(&answer.entries), ["line 9\n"]);
        assert_eq!(answer.dropped_count, 3, "6 to 8 stay missing");
    }

    #[tokio::test]
    async fn an_attach_takes_and_answers_the_buffers_own_seq() {
        let (ws, mut sent) = connection();
        let mut buf = SessionBuffer::new();
        buf.push(line(1));
        buf.push(line(2));
        buf.mark("[output gap detected: backpressure, last_seq=2]".into());
        buf.push(line(3));
        ws.sessions.lock().await.insert("s1".to_string(), buf);

        // A reader that holds up to the marker, place 3, has not read
        // line 3, at place 4.
        let (answer, ()) = tokio::join!(ws.attach_session("s1", 3), async {
            let attach = sent.recv().await.unwrap();
            assert_eq!(attach["since"], 3);
            deliver(&ws, &replay(&attach, 3)).await;
        });
        let answer = answer.unwrap();
        assert_eq!(texts(&answer.entries), ["line 3\n"]);
        assert_eq!(answer.last_seq, 4);

        // From the place it answered nothing comes back, and last_seq stays
        // there. That place, past the daemon's seq, moves no seq of the
        // daemon's: its next line is kept.
        let (answer, ()) = tokio::join!(ws.attach_session("s1", 4), async {
            let attach = sent.recv().await.unwrap();
            assert_eq!(attach["since"], 3);
            deliver(&ws, &replay(&attach, 3)).await;
        });
        let answer = answer.unwrap();
        assert!(answer.entries.is_empty());
        assert_eq!(answer.last_seq, 4);
        deliver(&ws, &output(4)).await;
        let read = ws.read_output("s1", answer.last_seq, 0).await.unwrap();
        assert_eq!(texts(&read.entries), ["line 4\n"]);
        assert_eq!(read.last_seq, 5);
    }

    #[tokio::test]
    async fn a_new_buffer_attached_from_a_seq_places_what_follows_after_it() {
        // After a restart mcp-sctl holds nothing for the session, and the
        // reader attaches from the last seq it read.
        let (ws, mut sent) = connection();
        let (answer, ()) = tokio::join!(ws.attach_session("s1", 10), async {
            let attach = sent.recv().await.unwrap();
            assert_eq!(attach["since"], 10);
            deliver(&ws, &replay(&attach, 10)).await;
        });
        let answer = answer.unwrap();
        assert!(answer.entries.is_empty());
        assert_eq!(answer.last_seq, 10);

        // A marker and the daemon's next line both come after it.
        let gap = json!({ "type": "session.gap", "session_id": "s1", "reason": "backpressure" });
        deliver(&ws, &gap).await;
        deliver(&ws, &output(14)).await;
        let read = ws.read_output("s1", answer.last_seq, 0).await.unwrap();
        assert_eq!(
            texts(&read.entries),
            [
                "[output gap detected: backpressure, last_seq=10]",
                "line 14\n"
            ]
        );
        assert_eq!(places(&read.entries), [11, 14]);
        assert_eq!(read.dropped_count, 3);
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

    #[tokio::test(start_paused = true)]
    async fn a_re_attach_with_no_reply_holds_live_output_no_longer_than_an_attach_waits() {
        let (ws, _sent) = connection();
        let mut buf = SessionBuffer::new();
        buf.push(line(1));
        ws.sessions.lock().await.insert("s1".to_string(), buf);

        // A new connection re-attaches, and the reply is lost on the way.
        reattach_messages(&mut *ws.sessions.lock().await);
        deliver(&ws, &output(2)).await;
        let read = ws.read_output("s1", 1, 0).await.unwrap();
        assert!(read.entries.is_empty(), "held while the reply may come");

        // A reader that would wait an hour reads what it held as the hold
        // ends.
        let start = Instant::now();
        let read = ws.read_output("s1", 1, 3_600_000).await.unwrap();
        assert_eq!(texts(&read.entries), ["line 2\n"]);
        assert_eq!(start.elapsed(), ATTACH_REPLY_WAIT);
        assert!(ws.sessions.lock().await["s1"].held.is_none());

        // Live output that comes later is kept as it comes.
        deliver(&ws, &output(3)).await;
        let read = ws.read_output("s1", 2, 0).await.unwrap();
        assert_eq!(texts(&read.entries), ["line 3\n"]);
    }

    #[tokio::test(start_paused = true)]
    async fn live_output_after_a_holds_end_comes_after_what_it_held() {
        let mut buf = SessionBuffer::new();
        buf.push(line(1));
        buf.hold();
        buf.live(line(2));
        buf.live_mark("Process exited with code 0".into());
        tokio::time::advance(ATTACH_REPLY_WAIT).await;
        buf.live(line(3));
        assert_eq!(
            data(&buf),
            [
                "line 1\n",
                "line 2\n",
                "Process exited with code 0",
                "line 3\n"
            ]
        );
        assert!(buf.held.is_none());

        // A reply that comes after is kept only above the highest kept.
        buf.settle([line(2), line(3), line(4)]);
        assert_eq!(buf.entries.back().unwrap().data, "line 4\n");
        assert_eq!(buf.entries.len(), 5);
    }

    #[tokio::test(start_paused = true)]
    async fn a_second_attach_holds_for_its_own_wait() {
        let mut buf = SessionBuffer::new();
        buf.hold();
        tokio::time::advance(ATTACH_REPLY_WAIT / 2).await;
        buf.hold();
        tokio::time::advance(ATTACH_REPLY_WAIT / 2).await;
        buf.live(line(1));
        assert!(
            buf.entries.is_empty(),
            "held while the second reply may come"
        );
        tokio::time::advance(ATTACH_REPLY_WAIT / 2).await;
        buf.live(line(2));
        assert_eq!(data(&buf), ["line 1\n", "line 2\n"]);
    }

    #[tokio::test]
    async fn the_relay_losing_the_device_ends_every_hold() {
        let (ws, _sent) = connection();
        for session_id in ["s1", "s2"] {
            let mut buf = SessionBuffer::new();
            buf.hold();
            buf.live(line(1));
            ws.sessions.lock().await.insert(session_id.to_string(), buf);
        }
        let lost = json!({ "type": "tunnel.device_disconnected", "reason": "tunnel closed" });
        deliver(&ws, &lost).await;
        let sessions = ws.sessions.lock().await;
        for session_id in ["s1", "s2"] {
            assert_eq!(data(&sessions[session_id]), ["line 1\n"]);
            assert!(sessions[session_id].held.is_none());
        }
    }

    #[tokio::test]
    async fn the_reader_keeps_a_gap_marker_and_the_real_next_entry() {
        let (ws, _sent) = connection();
        ws.sessions
            .lock()
            .await
            .insert("s1".to_string(), SessionBuffer::new());
        deliver(&ws, &output(1)).await;
        deliver(&ws, &output(2)).await;
        let gap = json!({ "type": "session.gap", "session_id": "s1", "reason": "backpressure" });
        deliver(&ws, &gap).await;
        {
            // The marker takes the next place and no seq of the daemon's.
            let sessions = ws.sessions.lock().await;
            assert_eq!(sessions["s1"].last_seq, 2);
            assert_eq!(sessions["s1"].last_place, 3);
        }

        // A re-attach starts from the daemon's last seq. Its replay brings
        // the line after the gap, which also comes live before and after
        // the reply.
        let attaches = reattach_messages(&mut *ws.sessions.lock().await);
        assert_eq!(attaches[0]["since"], 2);
        deliver(&ws, &output(3)).await;
        deliver(&ws, &attached(3..=3)).await;
        deliver(&ws, &output(3)).await;

        let read = ws.read_output("s1", 0, 0).await.unwrap();
        assert_eq!(
            texts(&read.entries),
            [
                "line 1\n",
                "line 2\n",
                "[output gap detected: backpressure, last_seq=2]",
                "line 3\n"
            ]
        );
        assert_eq!(places(&read.entries), [1, 2, 3, 4]);
        assert_eq!(ws.sessions.lock().await["s1"].last_seq, 3);
        assert_eq!(read.dropped_count, 0);
    }

    #[tokio::test]
    async fn the_reader_keeps_the_last_output_an_exit_overtakes() {
        // Through a relay a session's exit can come before its last output.
        let (ws, _sent) = connection();
        ws.sessions
            .lock()
            .await
            .insert("s1".to_string(), SessionBuffer::new());
        deliver(&ws, &output(1)).await;
        let exited = json!({ "type": "session.exited", "session_id": "s1", "exit_code": 0 });
        deliver(&ws, &exited).await;
        {
            let sessions = ws.sessions.lock().await;
            assert_eq!(sessions["s1"].last_seq, 1);
            assert_eq!(sessions["s1"].last_place, 2);
            assert_eq!(sessions["s1"].status, SessionStatus::Exited);
        }
        deliver(&ws, &output(2)).await;
        let read = ws.read_output("s1", 0, 0).await.unwrap();
        assert_eq!(
            texts(&read.entries),
            ["line 1\n", "Process exited with code 0", "line 2\n"]
        );
        assert_eq!(places(&read.entries), [1, 2, 3]);
        assert_eq!(read.exit_code, Some(0));
    }

    #[tokio::test]
    async fn an_attach_holds_live_output_from_before_it_is_sent() {
        let (ws, mut sent) = connection();
        // The connection's queue is full, so the attach waits to go out.
        while ws.sender.try_send(json!({ "type": "ping" })).is_ok() {}
        let (answer, ()) = tokio::join!(ws.attach_session("s1", 0), async {
            // Output that comes before the attach goes out waits for the
            // replay.
            assert_eq!(sent.recv().await.unwrap()["type"], "ping");
            deliver(&ws, &output(3)).await;
            let attach = loop {
                let frame = sent.recv().await.unwrap();
                if frame["type"] == "session.attach" {
                    break frame;
                }
            };
            assert_eq!(
                attach,
                json!({
                    "type": "session.attach",
                    "session_id": "s1",
                    "since": 0,
                    "dedupes": true,
                })
            );
            deliver(&ws, &replay(&attach, 3)).await;
        });
        let answer = answer.unwrap();
        assert_eq!(texts(&answer.entries), ["line 1\n", "line 2\n", "line 3\n"]);
        assert_eq!(answer.last_seq, 3);

        // Live output after the reply: a repeat is dropped, a new line kept.
        deliver(&ws, &output(3)).await;
        deliver(&ws, &output(4)).await;
        let read = ws.read_output("s1", answer.last_seq, 0).await.unwrap();
        assert_eq!(texts(&read.entries), ["line 4\n"]);
    }

    #[tokio::test]
    async fn an_attach_that_cannot_be_sent_holds_nothing() {
        let (ws, sent) = connection();
        drop(sent);
        let mut buf = SessionBuffer::new();
        buf.push(line(1));
        ws.sessions.lock().await.insert("s1".to_string(), buf);

        assert!(ws.attach_session("s1", 1).await.is_err());
        assert!(ws.sessions.lock().await["s1"].held.is_none());
        deliver(&ws, &output(2)).await;
        let read = ws.read_output("s1", 1, 0).await.unwrap();
        assert_eq!(texts(&read.entries), ["line 2\n"]);
    }

    #[tokio::test(start_paused = true)]
    async fn an_attach_with_no_reply_holds_nothing_after_its_wait() {
        let (ws, mut sent) = connection();
        let (answer, attach) = tokio::join!(ws.attach_session("s1", 0), sent.recv());
        assert_eq!(attach.unwrap()["type"], "session.attach");
        assert_eq!(
            answer.err().as_deref(),
            Some("Timeout waiting for session.attached response")
        );
        assert!(ws.sessions.lock().await["s1"].held.is_none());
        deliver(&ws, &output(1)).await;
        let read = ws.read_output("s1", 0, 0).await.unwrap();
        assert_eq!(texts(&read.entries), ["line 1\n"]);
    }

    #[tokio::test]
    async fn an_error_for_a_session_ends_its_hold() {
        let (ws, _sent) = connection();
        let mut buf = SessionBuffer::new();
        buf.push(line(1));
        buf.hold();
        buf.live(line(2));
        ws.sessions.lock().await.insert("s1".to_string(), buf);

        let error = json!({
            "type": "error",
            "code": "SESSION_NOT_FOUND",
            "session_id": "s1",
            "message": "Session s1 not found",
        });
        deliver(&ws, &error).await;
        let sessions = ws.sessions.lock().await;
        assert_eq!(data(&sessions["s1"]), ["line 1\n", "line 2\n"]);
        assert!(sessions["s1"].held.is_none());
        assert_eq!(sessions["s1"].status, SessionStatus::Exited);
    }

    /// A socket that takes nothing: every send fails.
    struct ClosedSocket;

    impl futures_util::Sink<tokio_tungstenite::tungstenite::Message> for ClosedSocket {
        type Error = ();

        fn poll_ready(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), ()>> {
            std::task::Poll::Ready(Err(()))
        }

        fn start_send(
            self: std::pin::Pin<&mut Self>,
            _: tokio_tungstenite::tungstenite::Message,
        ) -> Result<(), ()> {
            Err(())
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), ()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), ()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn a_re_attach_that_cannot_be_sent_holds_nothing() {
        let mut sessions = HashMap::new();
        let mut buf = SessionBuffer::new();
        buf.push(line(1));
        sessions.insert("s1".to_string(), buf);

        // Sent: the session holds live output until the reply.
        reattach(&mut futures_util::sink::drain(), &mut sessions).await;
        assert!(sessions["s1"].held.is_some());

        // The next connection cannot send it: no reply will come.
        reattach(&mut ClosedSocket, &mut sessions).await;
        let buf = sessions.get_mut("s1").unwrap();
        assert!(buf.held.is_none());
        buf.live(line(2));
        assert_eq!(data(buf), ["line 1\n", "line 2\n"]);
    }

    /// What an AI client's typing call sends into `s1`, if anything.
    #[derive(Clone, Copy, Debug)]
    enum Typing {
        Exec,
        Input,
        Signal,
        ExecWait,
    }

    async fn type_into(ws: &DeviceWsConnection, how: Typing) -> Result<(), String> {
        match how {
            Typing::Exec => ws.exec("s1", "true").await,
            Typing::Input => ws.send_input("s1", "true\n").await,
            Typing::Signal => ws.signal("s1", 2).await,
            Typing::ExecWait => ws.exec_wait("s1", "true", 1_000).await.map(|_| ()),
        }
    }

    /// A connection that holds `s1`.
    async fn holding_s1() -> (Arc<DeviceWsConnection>, mpsc::Receiver<Value>) {
        let (ws, sent) = connection();
        ws.sessions
            .lock()
            .await
            .insert("s1".to_string(), SessionBuffer::new());
        (Arc::new(ws), sent)
    }

    /// The call in a task of its own.
    fn call(
        ws: &Arc<DeviceWsConnection>,
        how: Typing,
    ) -> tokio::task::JoinHandle<Result<(), String>> {
        let ws = Arc::clone(ws);
        tokio::spawn(async move { type_into(&ws, how).await })
    }

    /// The call in a task of its own, on a connection that holds `s1`.
    async fn typing(
        how: Typing,
    ) -> (
        Arc<DeviceWsConnection>,
        mpsc::Receiver<Value>,
        tokio::task::JoinHandle<Result<(), String>>,
    ) {
        let (ws, sent) = holding_s1().await;
        let call = call(&ws, how);
        (ws, sent, call)
    }

    /// The AI status request a call sent: `working: true` for `s1`, with a
    /// `request_id` of its own.
    async fn the_request(sent: &mut mpsc::Receiver<Value>) -> Value {
        let asked = sent.recv().await.unwrap();
        assert_eq!(asked["type"], "session.ai_status", "{asked}");
        assert_eq!(asked["session_id"], "s1", "{asked}");
        assert_eq!(asked["working"], true, "{asked}");
        assert!(
            asked["request_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty()),
            "{asked}"
        );
        asked
    }

    /// The agent's answer to `working: true` on a session no person
    /// opened, to the request `asked`.
    fn refusal(asked: &Value, session_id: &str) -> Value {
        json!({
            "type": "error",
            "code": "AI_NOT_ALLOWED",
            "session_id": session_id,
            "message": "Cannot set working=true: user has not allowed AI for this session",
            "request_id": asked["request_id"],
        })
    }

    /// The agent's ack of `working: true` on `session_id`, to the request
    /// `asked`.
    fn ack(asked: &Value, session_id: &str) -> Value {
        json!({
            "type": "session.ai_status.ack",
            "session_id": session_id,
            "working": true,
            "activity": "write",
            "request_id": asked["request_id"],
        })
    }

    /// Asserts that `msg` sets the AI status of `s1` to not working.
    fn assert_stops_working(msg: &Value) {
        assert_eq!(msg["type"], "session.ai_status", "{msg}");
        assert_eq!(msg["session_id"], "s1", "{msg}");
        assert_eq!(msg["working"], false, "{msg}");
    }

    const TYPING: [Typing; 4] = [
        Typing::Exec,
        Typing::Input,
        Typing::Signal,
        Typing::ExecWait,
    ];

    #[tokio::test]
    async fn a_session_start_asks_for_a_session_closed_to_ai() {
        let (ws, mut sent) = connection();
        let ws = Arc::new(ws);
        let start = tokio::spawn({
            let ws = Arc::clone(&ws);
            async move {
                ws.start_session(None, None, None, true, true, None, None, None, None)
                    .await
            }
        });
        let msg = sent.recv().await.unwrap();
        assert_eq!(msg["type"], "session.start");
        assert_eq!(msg["user_allows_ai"], false, "{msg}");
        deliver(
            &ws,
            &json!({
                "type": "session.started",
                "session_id": "s1",
                "pid": 1,
                "persistent": true,
                "pty": true,
                "user_allows_ai": false,
            }),
        )
        .await;
        let started = start.await.unwrap().unwrap();
        assert_eq!(started["user_allows_ai"], false);
    }

    #[tokio::test]
    async fn typing_into_a_closed_session_is_refused_with_nothing_sent() {
        for how in TYPING {
            let (ws, mut sent, call) = typing(how).await;
            let asked = the_request(&mut sent).await;
            deliver(&ws, &refusal(&asked, "s1")).await;

            let refused = call.await.unwrap().unwrap_err();
            assert!(
                refused.starts_with("Nothing was sent to session s1")
                    && refused.contains("Allow AI"),
                "{how:?}: {refused}"
            );
            assert!(sent.try_recv().is_err(), "{how:?} sent nothing more");
            assert!(!ws.is_ai_working("s1").await, "{how:?}");
        }
    }

    #[tokio::test]
    async fn typing_into_an_open_session_asks_first_then_sends() {
        for (how, kind) in [
            (Typing::Exec, "session.exec"),
            (Typing::Input, "session.stdin"),
        ] {
            let (ws, mut sent, call) = typing(how).await;
            let asked = the_request(&mut sent).await;
            deliver(&ws, &ack(&asked, "s1")).await;
            call.await.unwrap().unwrap();
            let typed = sent.try_recv().unwrap();
            assert_eq!(typed["type"], kind, "{how:?}");
            assert_eq!(typed["session_id"], "s1", "{how:?}");
            assert!(ws.is_ai_working("s1").await, "{how:?}");
            assert!(sent.try_recv().is_err(), "{how:?} sent nothing more");
        }
    }

    #[tokio::test]
    async fn a_signal_leaves_the_ai_status_as_it_found_it() {
        // The AI was not working: once the signal is sent, the status is
        // cleared, so the person's console takes keys again at once.
        let (ws, mut sent, signal) = typing(Typing::Signal).await;
        let asked = the_request(&mut sent).await;
        deliver(&ws, &ack(&asked, "s1")).await;
        signal.await.unwrap().unwrap();
        let signalled = sent.try_recv().unwrap();
        assert_eq!(signalled["type"], "session.signal", "{signalled}");
        assert_eq!(signalled["signal"], 2, "{signalled}");
        assert_stops_working(&sent.try_recv().unwrap());
        assert!(!ws.is_ai_working("s1").await);
        assert!(sent.try_recv().is_err());

        // The AI was working: it still is, as it was told.
        let (ws, mut sent) = holding_s1().await;
        ws.mark_ai_working("s1").await;
        let signal = call(&ws, Typing::Signal);
        let asked = the_request(&mut sent).await;
        deliver(&ws, &ack(&asked, "s1")).await;
        signal.await.unwrap().unwrap();
        assert_eq!(sent.try_recv().unwrap()["type"], "session.signal");
        assert!(sent.try_recv().is_err(), "the status is left on");
        assert!(ws.is_ai_working("s1").await);
    }

    #[tokio::test(start_paused = true)]
    async fn an_ack_for_another_session_or_no_answer_sends_nothing() {
        for how in TYPING {
            let (ws, mut sent, call) = typing(how).await;
            let asked = the_request(&mut sent).await;
            deliver(&ws, &ack(&asked, "s2")).await;
            assert!(call.await.unwrap().is_err(), "{how:?}: another's ack");
            assert!(sent.try_recv().is_err(), "{how:?}: another's ack");

            // No answer: the wait for it ends, and nothing follows.
            let (_ws, mut sent, call) = typing(how).await;
            the_request(&mut sent).await;
            let refused = call.await.unwrap().unwrap_err();
            assert!(refused.contains("Timeout"), "{how:?}: {refused}");
            assert!(sent.try_recv().is_err(), "{how:?}: no answer");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_ack_to_no_request_of_the_call_is_not_its_answer() {
        for how in TYPING {
            let (ws, mut sent, call) = typing(how).await;
            let asked = the_request(&mut sent).await;
            // An ack for the session that echoes no request, and one that
            // echoes a request this connection never sent.
            let mut stale = ack(&asked, "s1");
            stale.as_object_mut().unwrap().remove("request_id");
            deliver(&ws, &stale).await;
            stale["request_id"] = json!("mcp-ai-status-0");
            deliver(&ws, &stale).await;

            let refused = call.await.unwrap().unwrap_err();
            assert!(refused.contains("Timeout"), "{how:?}: {refused}");
            assert!(sent.try_recv().is_err(), "{how:?}: nothing typed");
            assert!(!ws.is_ai_working("s1").await, "{how:?}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_late_ack_for_a_call_that_gave_up_is_undone() {
        let (ws, mut sent, call) = typing(Typing::Exec).await;
        let asked = the_request(&mut sent).await;
        let refused = call.await.unwrap().unwrap_err();
        assert!(refused.contains("Timeout"), "{refused}");

        // The agent applied it after all, and its ack comes late: the AI
        // status it set is cleared, and nothing is typed.
        deliver(&ws, &ack(&asked, "s1")).await;
        assert_stops_working(&sent.try_recv().unwrap());
        assert!(sent.try_recv().is_err(), "nothing typed");
        assert!(!ws.is_ai_working("s1").await);

        // The same answer again is no one's.
        deliver(&ws, &ack(&asked, "s1")).await;
        assert!(sent.try_recv().is_err());

        // A late refusal leaves nothing to undo.
        let (ws, mut sent, call) = typing(Typing::Exec).await;
        let asked = the_request(&mut sent).await;
        assert!(call.await.unwrap().is_err());
        deliver(&ws, &refusal(&asked, "s1")).await;
        assert!(sent.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn a_late_ack_is_not_the_next_calls_answer() {
        // The next call's own answer: an ack, or the refusal of a session
        // the person closed meanwhile.
        for opened in [true, false] {
            let (ws, mut sent, first) = typing(Typing::Exec).await;
            let first_asked = the_request(&mut sent).await;
            assert!(first.await.unwrap().is_err(), "the first call gave up");

            let next = call(&ws, Typing::Exec);
            let next_asked = the_request(&mut sent).await;
            assert_ne!(first_asked["request_id"], next_asked["request_id"]);

            // The first call's ack comes now. It is not the next call's
            // answer, and that answer, still to come, decides the status,
            // so nothing is sent for it.
            deliver(&ws, &ack(&first_asked, "s1")).await;
            for _ in 0..4 {
                tokio::task::yield_now().await;
            }
            assert!(!next.is_finished(), "the next call waits for its own");
            assert!(sent.try_recv().is_err());

            if opened {
                deliver(&ws, &ack(&next_asked, "s1")).await;
                next.await.unwrap().unwrap();
                assert_eq!(sent.try_recv().unwrap()["type"], "session.exec");
            } else {
                deliver(&ws, &refusal(&next_asked, "s1")).await;
                let refused = next.await.unwrap().unwrap_err();
                assert!(refused.starts_with("Nothing was sent"), "{refused}");
            }
            assert!(sent.try_recv().is_err(), "opened {opened}");
        }
    }
}
