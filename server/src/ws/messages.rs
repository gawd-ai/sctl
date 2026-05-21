//! Typed wire format for server → client WebSocket messages.
//!
//! Replaces the previous `json!()` sites in [`super`] with a single tagged
//! enum so that:
//!   1. The shape of every outgoing message is statically checked at compile
//!      time (no more typos in field names).
//!   2. The web client consumes a ts-rs-generated TypeScript discriminated
//!      union that tracks Rust changes via `cargo test export_bindings`.
//!
//! Wire format invariant: `{"type": "<kebab.dot.code>", ...payload}`. Serde's
//! internally-tagged enum encoding produces exactly this shape, so a side-by-
//! side diff between the old `json!()` output and the new enum output is
//! byte-identical for every existing message variant.
//!
//! `request_id` is conditionally present (sent only when the corresponding
//! client message carried one). We model this as `Option<String>` on every
//! variant that can appear as a response to a request — broadcasts omit it.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::activity::ActivityEntry;
use crate::gawdxfer::types::{Complete, Progress};

/// Server → client message. Wire format is `{"type": "<code>", ...fields}`
/// via serde's internally-tagged enum representation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export))]
#[serde(tag = "type")]
pub enum WsServerMsg {
    // ─── Heartbeat ───────────────────────────────────────────────────────────
    #[serde(rename = "pong")]
    Pong {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },

    // ─── Error envelope ──────────────────────────────────────────────────────
    /// Covers every error code emitted by the WS layer. `code` is a screaming
    /// snake-case identifier (e.g. `INVALID_JSON`, `SESSION_NOT_FOUND`).
    /// `session_id` is present when the error is scoped to a specific session.
    #[serde(rename = "error")]
    Error {
        code: String,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },

    // ─── Session lifecycle ───────────────────────────────────────────────────
    /// Sent to the originating connection in response to `session.start`.
    #[serde(rename = "session.started")]
    SessionStarted {
        session_id: String,
        pid: u32,
        persistent: bool,
        pty: bool,
        user_allows_ai: bool,
        created_at: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },

    /// Broadcast to all clients when any connection creates a session.
    #[serde(rename = "session.created")]
    SessionCreated {
        session_id: String,
        pid: u32,
        pty: bool,
        persistent: bool,
        user_allows_ai: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },

    /// Broadcast when a session is killed (via `session.kill` or hangup).
    #[serde(rename = "session.destroyed")]
    SessionDestroyed { session_id: String, reason: String },

    /// Response to the originating connection's `session.kill`.
    #[serde(rename = "session.closed")]
    SessionClosed {
        session_id: String,
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },

    /// Response to `session.attach` — carries replayed buffered output.
    #[serde(rename = "session.attached")]
    SessionAttached {
        session_id: String,
        entries: Vec<Value>,
        dropped: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },

    /// Response to `session.list`.
    #[serde(rename = "session.listed")]
    SessionListed {
        sessions: Vec<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },

    /// Broadcast when a session is renamed.
    #[serde(rename = "session.renamed")]
    SessionRenamed { session_id: String, name: String },

    /// Response to `session.rename`.
    #[serde(rename = "session.rename.ack")]
    SessionRenameAck {
        session_id: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },

    /// Response to `session.exec` — confirms stdin write.
    #[serde(rename = "session.exec.ack")]
    SessionExecAck {
        session_id: String,
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },

    /// Response to `session.signal`.
    #[serde(rename = "session.signal.ack")]
    SessionSignalAck {
        session_id: String,
        signal: i32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },

    /// Response to `session.resize`.
    #[serde(rename = "session.resize.ack")]
    SessionResizeAck {
        session_id: String,
        rows: u16,
        cols: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },

    /// Response to `session.allow_ai`.
    #[serde(rename = "session.allow_ai.ack")]
    SessionAllowAiAck {
        session_id: String,
        allowed: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },

    /// Broadcast when AI permission flips for a session.
    #[serde(rename = "session.ai_permission_changed")]
    SessionAiPermissionChanged { session_id: String, allowed: bool },

    /// Broadcast when AI working/idle state changes.
    #[serde(rename = "session.ai_status_changed")]
    SessionAiStatusChanged {
        session_id: String,
        working: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        activity: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },

    /// Response to `session.ai_status` — confirms an AI status update from the
    /// originating connection.
    #[serde(rename = "session.ai_status.ack")]
    SessionAiStatusAck {
        session_id: String,
        working: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        activity: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },

    /// Response to `shell.list`.
    #[serde(rename = "shell.listed")]
    ShellListed {
        shells: Vec<Value>,
        default_shell: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },

    // ─── Session output ──────────────────────────────────────────────────────
    /// Stdout chunk from a PTY/process.
    #[serde(rename = "session.stdout")]
    SessionStdout {
        session_id: String,
        data: String,
        seq: u64,
        timestamp_ms: u64,
    },

    /// Stderr chunk.
    #[serde(rename = "session.stderr")]
    SessionStderr {
        session_id: String,
        data: String,
        seq: u64,
        timestamp_ms: u64,
    },

    /// System-emitted message (lifecycle banner, exit code, etc.).
    #[serde(rename = "session.system")]
    SessionSystem {
        session_id: String,
        data: String,
        seq: u64,
        timestamp_ms: u64,
    },

    // ─── Activity log ────────────────────────────────────────────────────────
    /// Broadcast for every new activity log entry.
    #[serde(rename = "activity.new")]
    ActivityNew { entry: ActivityEntry },

    // ─── gawdxfer transfer events ───────────────────────────────────────────
    /// Broadcast when a transfer finishes (upload or download).
    #[serde(rename = "gx.complete")]
    GxComplete { data: Complete },

    /// Broadcast for every chunk progress tick.
    #[serde(rename = "gx.progress")]
    GxProgress { data: Progress },
}

impl WsServerMsg {
    /// Convert to a `serde_json::Value` for transmission through the existing
    /// `mpsc::Sender<Value>` plumbing. Serialization cannot fail for any of
    /// the typed variants — every field is a primitive, string, or another
    /// already-serializable type.
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).expect("WsServerMsg must serialize")
    }
}
