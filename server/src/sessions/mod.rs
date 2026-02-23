//! Session lifecycle management with persistent session support.
//!
//! [`SessionManager`] is the single authority for creating, accessing, and
//! destroying shell sessions. It enforces `max_sessions` limits and supports:
//!
//! - **Persistent sessions** that survive WebSocket disconnects (output keeps
//!   buffering, can be re-attached later).
//! - **Attach/detach** for reconnection — a client can detach, reconnect, and
//!   catch up on missed output via `session.attach`.
//! - **Sweep** that cleans up exited sessions and gracefully kills sessions
//!   that exceed their client-requested `idle_timeout`.
//! - **Journal** — session output is persisted to disk for crash recovery.
//! - **PTY** — sessions can be backed by a PTY for full terminal emulation.
//!
//! ## Concurrency
//!
//! The session map is behind an `RwLock`. Read operations (send to stdin,
//! get status) take a read lock; mutations (create, kill, sweep) take a write
//! lock. `create_session` holds the write lock across the limit-check and
//! insert to prevent TOCTOU races.

pub mod buffer;
pub mod journal;
pub mod session;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::RwLock;
use tracing::{info, warn};
use uuid::Uuid;

use crate::shell::process::spawn_shell_pgroup;
use crate::shell::pty::{allocate_pty, spawn_shell_pty};
use buffer::OutputBuffer;
use journal::{SessionJournal, SessionMetadata};
use session::{ManagedSession, SessionStatus};

/// Manages the pool of active interactive shell sessions.
///
/// Cloneable — all clones share the same inner `Arc<RwLock<...>>`.
#[derive(Clone)]
pub struct SessionManager {
    sessions: Arc<RwLock<HashMap<String, SessionEntry>>>,
    max_sessions: usize,
    buffer_size: usize,
    /// Data directory for journals. `None` if journaling is disabled.
    data_dir: Option<String>,
}

/// Summary of a session returned by [`SessionManager::list_sessions`].
#[allow(clippy::struct_excessive_bools)]
pub struct SessionListItem {
    pub session_id: String,
    pub pid: u32,
    pub persistent: bool,
    pub pty: bool,
    pub attached: bool,
    /// `"running"` or `"exited"`.
    pub status: String,
    /// Exit code of the session's process (only set when status is `"exited"`).
    pub exit_code: Option<i32>,
    /// Whether the session is considered idle (detached, no recent activity).
    pub idle: bool,
    /// Client-requested idle timeout in seconds (0 = never auto-kill).
    pub idle_timeout: u64,
    /// Optional human-readable name for the session.
    pub name: Option<String>,
    /// Epoch milliseconds when the session was created.
    pub created_at: u64,
    /// Whether the user permits AI to control this session.
    pub user_allows_ai: bool,
    /// Whether the AI is currently working in this session.
    pub ai_is_working: bool,
    /// Activity type: `"read"` or `"write"`.
    pub ai_activity: Option<String>,
    /// Short status message from the AI (e.g. "Running tests").
    pub ai_status_message: Option<String>,
}

/// Events produced by [`SessionManager::sweep`] for callers to broadcast.
pub enum SweepEvent {
    /// Session was destroyed (removed from pool). Contains `(session_id, reason)`.
    Destroyed(String, String),
    /// AI working status was auto-cleared due to inactivity. Contains `session_id`.
    AiAutoCleared(String),
}

/// Internal bookkeeping for a session.
#[allow(clippy::struct_excessive_bools)]
pub struct SessionEntry {
    pub session: ManagedSession,
    /// Whether this session survives WebSocket disconnect.
    pub persistent: bool,
    /// Last time the session received input or was attached.
    pub last_activity: Instant,
    /// Whether a WebSocket subscriber is currently attached.
    pub attached: bool,
    /// Seconds of idle (detached + no activity) before the session is
    /// gracefully killed by sweep. 0 = never auto-kill.
    pub idle_timeout: u64,
    /// Optional human-readable name for the session.
    pub name: Option<String>,
    /// Epoch milliseconds when the session was created.
    pub created_at: u64,
    /// Whether the user permits AI to control this session.
    pub user_allows_ai: bool,
    /// Whether the AI is currently working in this session.
    pub ai_is_working: bool,
    /// Activity type: `"read"` or `"write"`.
    pub ai_activity: Option<String>,
    /// Short status message from the AI (e.g. "Running tests").
    pub ai_status_message: Option<String>,
    /// Last time the AI sent a command or status update. Used for idle auto-clear.
    pub ai_last_activity: Option<Instant>,
}

impl SessionManager {
    pub fn new(max_sessions: usize, buffer_size: usize) -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            max_sessions,
            buffer_size,
            data_dir: None,
        }
    }

    /// Create a `SessionManager` with journaling enabled.
    pub fn with_journal(max_sessions: usize, buffer_size: usize, data_dir: &str) -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            max_sessions,
            buffer_size,
            data_dir: Some(data_dir.to_string()),
        }
    }

    /// Create a new shell session. Returns `(session_id, pid)`.
    ///
    /// Holds the write lock through the entire check-and-insert to prevent
    /// TOCTOU races.
    pub async fn create_session(
        &self,
        shell: &str,
        working_dir: &str,
        env: Option<&HashMap<String, String>>,
        persistent: bool,
    ) -> Result<(String, u32), String> {
        self.create_session_inner(shell, working_dir, env, persistent, false, 24, 80, 0, None)
            .await
    }

    /// Create a new session with optional PTY support.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_session_with_pty(
        &self,
        shell: &str,
        working_dir: &str,
        env: Option<&HashMap<String, String>>,
        persistent: bool,
        use_pty: bool,
        rows: u16,
        cols: u16,
        idle_timeout: u64,
        name: Option<&str>,
    ) -> Result<(String, u32), String> {
        self.create_session_inner(
            shell,
            working_dir,
            env,
            persistent,
            use_pty,
            rows,
            cols,
            idle_timeout,
            name,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_session_inner(
        &self,
        shell: &str,
        working_dir: &str,
        env: Option<&HashMap<String, String>>,
        persistent: bool,
        use_pty: bool,
        rows: u16,
        cols: u16,
        idle_timeout: u64,
        name: Option<&str>,
    ) -> Result<(String, u32), String> {
        let mut sessions = self.sessions.write().await;

        if sessions.len() >= self.max_sessions {
            return Err(format!("Session limit reached (max {})", self.max_sessions));
        }

        let session_id = Uuid::new_v4().to_string();

        let session = if use_pty {
            // PTY-backed session
            let pty_pair =
                allocate_pty(rows, cols).map_err(|e| format!("Failed to allocate PTY: {e}"))?;

            // Merge TERM into env if not already set
            let mut pty_env = env.cloned().unwrap_or_default();
            pty_env
                .entry("TERM".to_string())
                .or_insert_with(|| "xterm-256color".to_string());

            let child = spawn_shell_pty(&pty_pair, shell, working_dir, Some(&pty_env))
                .map_err(|e| format!("Failed to spawn PTY shell: {e}"))?;

            ManagedSession::spawn_pty(session_id.clone(), child, pty_pair.master, self.buffer_size)?
        } else {
            // Pipe-backed session
            let child = spawn_shell_pgroup(shell, working_dir, env)
                .map_err(|e| format!("Failed to spawn shell: {e}"))?;
            ManagedSession::spawn(session_id.clone(), child, self.buffer_size)?
        };

        let pid = session.pid;

        // Set up journal if data_dir is configured
        if let Some(ref data_dir) = self.data_dir {
            let journal_dir = journal::sessions_dir(Path::new(data_dir));
            let metadata = SessionMetadata {
                v: 1,
                pid,
                shell: shell.to_string(),
                working_dir: working_dir.to_string(),
                persistent,
                pty: use_pty,
                created: journal::now_ms(),
            };
            match SessionJournal::create(&journal_dir, &session_id, &metadata).await {
                Ok(j) => {
                    session.buffer.lock().await.set_journal(j.sender());
                }
                Err(e) => {
                    warn!("Failed to create journal for session {session_id}: {e}");
                }
            }
        }

        let now = Instant::now();
        let created_at = journal::now_ms();

        let session_name = name.map(ToString::to_string);

        sessions.insert(
            session_id.clone(),
            SessionEntry {
                session,
                persistent,
                last_activity: now,
                attached: true,
                idle_timeout,
                name: session_name,
                created_at,
                user_allows_ai: true,
                ai_is_working: false,
                ai_activity: None,
                ai_status_message: None,
                ai_last_activity: None,
            },
        );

        let mode = if use_pty { "pty" } else { "pipe" };
        let ttl = if idle_timeout == 0 {
            "no timeout".to_string()
        } else {
            format!("idle_timeout={idle_timeout}s")
        };
        info!(
            "Session {session_id} created ({mode}, pid {pid}, persistent={persistent}, {ttl}), total: {}",
            sessions.len()
        );
        Ok((session_id, pid))
    }

    /// Send data to a session's stdin.
    pub async fn send_to_session(&self, session_id: &str, data: &str) -> Result<(), String> {
        let sessions = self.sessions.read().await;
        match sessions.get(session_id) {
            Some(entry) => entry.session.write_stdin(data).await,
            None => Err(format!("Session {session_id} not found")),
        }
    }

    /// Send a command to a session, appending the appropriate line ending
    /// (`\r` for PTY sessions, `\n` for pipe sessions).
    pub async fn exec_command(&self, session_id: &str, command: &str) -> Result<(), String> {
        let sessions = self.sessions.read().await;
        match sessions.get(session_id) {
            Some(entry) => {
                let line_ending = if entry.session.is_pty() { "\r" } else { "\n" };
                entry
                    .session
                    .write_stdin(&format!("{command}{line_ending}"))
                    .await
            }
            None => Err(format!("Session {session_id} not found")),
        }
    }

    /// Touch AI last activity timestamp for a session (called on exec/stdin
    /// when AI is working, to prevent idle auto-clear).
    pub async fn touch_ai_activity(&self, session_id: &str) {
        let mut sessions = self.sessions.write().await;
        if let Some(entry) = sessions.get_mut(session_id) {
            if entry.ai_is_working {
                entry.ai_last_activity = Some(Instant::now());
            }
        }
    }

    /// Send a signal to a session's process group.
    pub async fn signal_session(&self, session_id: &str, signal: i32) -> Result<(), String> {
        let sessions = self.sessions.read().await;
        match sessions.get(session_id) {
            Some(entry) => entry.session.send_signal(signal),
            None => Err(format!("Session {session_id} not found")),
        }
    }

    /// Gracefully kill and remove a session (SIGTERM → wait → SIGKILL).
    /// Returns true if the session existed.
    pub async fn kill_session(&self, session_id: &str) -> bool {
        let mut sessions = self.sessions.write().await;
        if let Some(entry) = sessions.remove(session_id) {
            // Drop lock before the potentially slow graceful kill
            drop(sessions);
            entry.session.graceful_kill().await;
            info!("Session {session_id} killed (graceful)");
            true
        } else {
            false
        }
    }

    /// Gracefully kill all sessions (used during shutdown).
    ///
    /// Sends SIGTERM to all at once, waits up to 3 s, then SIGKILL remaining.
    pub async fn kill_all(&self) {
        let mut sessions = self.sessions.write().await;
        let count = sessions.len();
        if count == 0 {
            return;
        }

        // Phase 1: SIGTERM all
        for (id, entry) in sessions.iter() {
            #[allow(clippy::cast_possible_wrap)]
            let pgid = entry.session.pgid as i32;
            if pgid > 0 {
                unsafe {
                    libc::kill(-pgid, libc::SIGTERM);
                }
            }
            info!("Session {id}: SIGTERM sent (shutdown)");
        }

        // Phase 2: wait up to 3 s for processes to exit
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(3);
        loop {
            let mut all_exited = true;
            for entry in sessions.values() {
                if *entry.session.status.lock().await != session::SessionStatus::Exited {
                    all_exited = false;
                    break;
                }
            }
            if all_exited || tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }

        // Phase 3: SIGKILL any remaining + abort tasks
        for (id, entry) in sessions.drain() {
            entry.session.kill();
            info!("Session {id} killed (shutdown)");
        }
        info!("Shut down {count} session(s)");
    }

    /// Attach to a session — marks it as attached and returns its buffer for
    /// subscriber use.
    pub async fn attach(&self, session_id: &str) -> Option<Arc<tokio::sync::Mutex<OutputBuffer>>> {
        let mut sessions = self.sessions.write().await;
        if let Some(entry) = sessions.get_mut(session_id) {
            entry.attached = true;
            entry.last_activity = Instant::now();
            Some(Arc::clone(&entry.session.buffer))
        } else {
            None
        }
    }

    /// Detach from a session — marks it as not attached.
    pub async fn detach(&self, session_id: &str) {
        let mut sessions = self.sessions.write().await;
        if let Some(entry) = sessions.get_mut(session_id) {
            entry.attached = false;
            entry.last_activity = Instant::now();
        }
    }

    /// Detach all sessions in the list (called on WS disconnect for persistent sessions).
    pub async fn detach_all(&self, session_ids: &[String]) {
        let mut sessions = self.sessions.write().await;
        for id in session_ids {
            if let Some(entry) = sessions.get_mut(id) {
                entry.attached = false;
                entry.last_activity = Instant::now();
            }
        }
    }

    /// Get the status and exit code of a session.
    pub async fn get_status(&self, session_id: &str) -> Option<(SessionStatus, Option<i32>)> {
        let sessions = self.sessions.read().await;
        if let Some(entry) = sessions.get(session_id) {
            let status = *entry.session.status.lock().await;
            let code = *entry.session.exit_code.lock().await;
            Some((status, code))
        } else {
            None
        }
    }

    /// Check whether a session is persistent.
    pub async fn is_persistent(&self, session_id: &str) -> bool {
        let sessions = self.sessions.read().await;
        sessions
            .get(session_id)
            .is_some_and(|entry| entry.persistent)
    }

    /// Get the buffer for a session (used by subscriber tasks).
    pub async fn get_buffer(
        &self,
        session_id: &str,
    ) -> Option<Arc<tokio::sync::Mutex<OutputBuffer>>> {
        let sessions = self.sessions.read().await;
        sessions
            .get(session_id)
            .map(|entry| Arc::clone(&entry.session.buffer))
    }

    /// Rename a session. Returns `Err` if the session doesn't exist.
    pub async fn rename_session(&self, session_id: &str, name: &str) -> Result<(), String> {
        let mut sessions = self.sessions.write().await;
        match sessions.get_mut(session_id) {
            Some(entry) => {
                entry.name = Some(name.to_string());
                Ok(())
            }
            None => Err(format!("Session {session_id} not found")),
        }
    }

    /// Set whether the user allows AI to control a session.
    ///
    /// If `allowed` is `false` and the AI is currently working, the AI state
    /// is cleared. Returns `true` if AI state was cleared (caller should
    /// broadcast `session.ai_status_changed`).
    pub async fn set_user_allows_ai(
        &self,
        session_id: &str,
        allowed: bool,
    ) -> Result<bool, String> {
        let mut sessions = self.sessions.write().await;
        match sessions.get_mut(session_id) {
            Some(entry) => {
                entry.user_allows_ai = allowed;
                let cleared = if !allowed && entry.ai_is_working {
                    entry.ai_is_working = false;
                    entry.ai_activity = None;
                    entry.ai_status_message = None;
                    true
                } else {
                    false
                };
                Ok(cleared)
            }
            None => Err(format!("Session {session_id} not found")),
        }
    }

    /// Set AI working status for a session.
    ///
    /// `working=true` fails if `user_allows_ai` is `false`.
    /// `working=false` always succeeds and clears activity + message.
    pub async fn set_ai_status(
        &self,
        session_id: &str,
        working: bool,
        activity: Option<&str>,
        message: Option<&str>,
    ) -> Result<(), String> {
        let mut sessions = self.sessions.write().await;
        match sessions.get_mut(session_id) {
            Some(entry) => {
                if working && !entry.user_allows_ai {
                    return Err(
                        "Cannot set working=true: user has not allowed AI for this session"
                            .to_string(),
                    );
                }
                entry.ai_is_working = working;
                if working {
                    entry.ai_activity = activity.map(ToString::to_string);
                    entry.ai_status_message = message.map(ToString::to_string);
                    entry.ai_last_activity = Some(Instant::now());
                } else {
                    entry.ai_activity = None;
                    entry.ai_status_message = None;
                    entry.ai_last_activity = None;
                }
                Ok(())
            }
            None => Err(format!("Session {session_id} not found")),
        }
    }

    /// Count of active sessions.
    pub async fn session_count(&self) -> usize {
        self.sessions.read().await.len()
    }

    /// List all active sessions (used by the `session.list` WS message).
    pub async fn list_sessions(&self) -> Vec<SessionListItem> {
        let sessions = self.sessions.read().await;
        let idle_threshold = std::time::Duration::from_secs(60);
        let mut items = Vec::with_capacity(sessions.len());
        for (id, entry) in sessions.iter() {
            let status = *entry.session.status.lock().await;
            let exit_code = *entry.session.exit_code.lock().await;
            let idle = !entry.attached && entry.last_activity.elapsed() > idle_threshold;
            items.push(SessionListItem {
                session_id: id.clone(),
                pid: entry.session.pid,
                persistent: entry.persistent,
                pty: entry.session.is_pty(),
                attached: entry.attached,
                status: match status {
                    session::SessionStatus::Running => "running".to_string(),
                    session::SessionStatus::Exited => "exited".to_string(),
                },
                exit_code,
                idle,
                idle_timeout: entry.idle_timeout,
                name: entry.name.clone(),
                created_at: entry.created_at,
                user_allows_ai: entry.user_allows_ai,
                ai_is_working: entry.ai_is_working,
                ai_activity: entry.ai_activity.clone(),
                ai_status_message: entry.ai_status_message.clone(),
            });
        }
        items
    }

    /// Resize a session's PTY.
    pub async fn resize_session(
        &self,
        session_id: &str,
        rows: u16,
        cols: u16,
    ) -> Result<(), String> {
        let sessions = self.sessions.read().await;
        match sessions.get(session_id) {
            Some(entry) => entry.session.resize(rows, cols),
            None => Err(format!("Session {session_id} not found")),
        }
    }

    /// Load archived sessions from disk journals. Called once at startup.
    pub async fn recover_from_journal(&self, data_dir: &Path) {
        let archived = journal::recover_sessions(data_dir).await;
        if archived.is_empty() {
            return;
        }

        let mut sessions = self.sessions.write().await;
        for arch in archived {
            let mut buf = OutputBuffer::new(self.buffer_size);
            for entry in arch.entries {
                buf.push(entry.stream, entry.data);
            }

            let session = ManagedSession::archived(buf, arch.exit_code);
            let now = Instant::now();

            sessions.insert(
                arch.session_id.clone(),
                SessionEntry {
                    session,
                    persistent: arch.metadata.persistent,
                    last_activity: now,
                    attached: false,
                    idle_timeout: 0,
                    name: None,
                    created_at: arch.metadata.created,
                    user_allows_ai: true,
                    ai_is_working: false,
                    ai_activity: None,
                    ai_status_message: None,
                    ai_last_activity: None,
                },
            );

            info!(
                "Recovered archived session {} (exit_code={:?})",
                arch.session_id, arch.exit_code
            );
        }
        info!(
            "Recovered {} archived session(s), total: {}",
            sessions.len(),
            sessions.len()
        );
    }

    /// Periodic sweep that handles three cases:
    ///
    /// 1. **Exited sessions** — process already dead. Cleaned up immediately.
    /// 2. **Idle-timeout sessions** — running sessions whose client requested a
    ///    non-zero `idle_timeout`. If the session is detached and has been idle
    ///    longer than the timeout, it is gracefully killed (SIGTERM → wait →
    ///    SIGKILL). Sessions with `idle_timeout == 0` are **never** auto-killed.
    /// 3. **AI idle timeout** — if AI is marked as working but no activity has
    ///    arrived within 60s, auto-clear the AI status.
    ///
    /// Returns a list of sweep events for callers to broadcast.
    pub async fn sweep(&self) -> Vec<SweepEvent> {
        // Quick check with read lock
        {
            let sessions = self.sessions.read().await;
            if sessions.is_empty() {
                return Vec::new();
            }
        }

        let ai_idle_timeout = std::time::Duration::from_secs(60);
        let mut events: Vec<SweepEvent> = Vec::new();
        let mut sessions = self.sessions.write().await;

        // --- AI idle auto-clear (must happen before removing dead sessions) ---
        for (id, entry) in sessions.iter_mut() {
            if entry.ai_is_working {
                if let Some(last) = entry.ai_last_activity {
                    if last.elapsed() > ai_idle_timeout {
                        info!("Session {id}: AI idle timeout (>60s), auto-clearing");
                        entry.ai_is_working = false;
                        entry.ai_activity = None;
                        entry.ai_status_message = None;
                        entry.ai_last_activity = None;
                        events.push(SweepEvent::AiAutoCleared(id.clone()));
                    }
                }
            }
        }

        // --- Collect exited sessions (process dead) — remove immediately ---
        let mut dead: Vec<String> = Vec::new();
        for (id, entry) in sessions.iter() {
            if *entry.session.status.lock().await == session::SessionStatus::Exited {
                dead.push(id.clone());
            }
        }

        for id in &dead {
            if let Some(entry) = sessions.remove(id) {
                entry.session.abort_tasks();
                info!(
                    "Cleaned up exited session {id}, remaining: {}",
                    sessions.len()
                );
                events.push(SweepEvent::Destroyed(id.clone(), "exited".to_string()));
            }
        }

        // --- Collect idle-timed-out sessions to gracefully kill ---
        let idle_expired: Vec<String> = sessions
            .iter()
            .filter(|(_, entry)| {
                entry.idle_timeout > 0
                    && !entry.attached
                    && entry.last_activity.elapsed()
                        > std::time::Duration::from_secs(entry.idle_timeout)
            })
            .map(|(id, _)| id.clone())
            .collect();

        // Remove from map, then drop lock before the slow graceful kills
        let mut to_kill = Vec::with_capacity(idle_expired.len());
        for id in &idle_expired {
            if let Some(entry) = sessions.remove(id) {
                info!(
                    "Session {id} idle-timed-out ({}s), gracefully killing",
                    entry.idle_timeout
                );
                to_kill.push((id.clone(), entry));
            }
        }
        drop(sessions);

        // Graceful kill outside the lock
        for (id, entry) in to_kill {
            entry.session.graceful_kill().await;
            events.push(SweepEvent::Destroyed(id, "idle_timeout".to_string()));
        }

        events
    }
}
