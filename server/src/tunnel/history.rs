//! Relay-side record of device connection sessions (connect → disconnect).
//!
//! Extracted verbatim from `relay.rs` (0.6.0 Phase 2). The relay is the only
//! observer of a device outage — a device cannot report its own absence — so
//! this module is the fleet's primary outage record.

use std::collections::{HashMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tracing::info;

/// Maximum number of connection sessions to retain in history.
const MAX_CONNECTION_HISTORY: usize = 100;

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
