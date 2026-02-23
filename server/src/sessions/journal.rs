//! Append-only disk journal for session output persistence.
//!
//! Each session gets a `.jsonl` file under `$DATA_DIR/sessions/`. The first line
//! is metadata (version, pid, shell, etc.) and subsequent lines are compact
//! output entries. On startup, journals are scanned to recover archived sessions.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use super::buffer::{OutputEntry, OutputStream};

/// Metadata header written as the first line of each journal file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMetadata {
    /// Journal format version.
    pub v: u32,
    /// OS process ID of the shell.
    pub pid: u32,
    /// Shell binary path.
    pub shell: String,
    /// Working directory at session start.
    #[serde(rename = "wd")]
    pub working_dir: String,
    /// Whether the session was persistent.
    pub persistent: bool,
    /// Whether the session uses a PTY.
    #[serde(default)]
    pub pty: bool,
    /// Creation timestamp in milliseconds since epoch.
    pub created: u64,
}

/// Compact journal entry (one per line after the metadata header).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    /// Sequence number.
    pub s: u64,
    /// Stream type: 'o' = stdout, 'e' = stderr, 'x' = system.
    pub t: char,
    /// Output data.
    pub d: String,
    /// Timestamp in milliseconds since epoch.
    pub ts: u64,
}

impl JournalEntry {
    pub fn from_output_entry(entry: &OutputEntry) -> Self {
        Self {
            s: entry.seq,
            t: match entry.stream {
                OutputStream::Stdout => 'o',
                OutputStream::Stderr => 'e',
                OutputStream::System => 'x',
            },
            d: entry.data.clone(),
            ts: entry.timestamp_ms,
        }
    }

    pub fn to_output_entry(&self) -> OutputEntry {
        OutputEntry {
            seq: self.s,
            stream: match self.t {
                'o' => OutputStream::Stdout,
                'e' => OutputStream::Stderr,
                _ => OutputStream::System,
            },
            data: self.d.clone(),
            timestamp_ms: self.ts,
        }
    }
}

/// An archived session loaded from a journal file on disk.
pub struct ArchivedSession {
    pub session_id: String,
    pub metadata: SessionMetadata,
    pub entries: Vec<OutputEntry>,
    pub exit_code: Option<i32>,
}

/// Append-only journal writer for a single session.
///
/// Writes are sent via an mpsc channel to a background task that batches them
/// to disk.
pub struct SessionJournal {
    tx: mpsc::Sender<JournalEntry>,
    /// Set to `false` if the background writer task exits due to an error.
    alive: Arc<AtomicBool>,
}

impl SessionJournal {
    /// Create a new journal file for a session and spawn the background writer.
    pub async fn create(
        dir: &Path,
        session_id: &str,
        metadata: &SessionMetadata,
    ) -> Result<Self, std::io::Error> {
        fs::create_dir_all(dir).await?;

        let path = dir.join(format!("{session_id}.jsonl"));
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;

        // Write metadata header
        let header = serde_json::to_string(metadata).expect("serialize metadata");
        file.write_all(header.as_bytes()).await?;
        file.write_all(b"\n").await?;
        file.flush().await?;

        let (tx, rx) = mpsc::channel(10_000);
        let alive = Arc::new(AtomicBool::new(true));
        tokio::spawn(journal_writer_task(file, rx, Arc::clone(&alive)));

        Ok(Self { tx, alive })
    }

    /// Get a clone of the sender for use by the buffer hook.
    pub fn sender(&self) -> mpsc::Sender<JournalEntry> {
        self.tx.clone()
    }

    /// Whether the background writer task is still alive.
    #[allow(dead_code)]
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }
}

/// Background task that drains journal entries and writes them to disk.
async fn journal_writer_task(
    mut file: fs::File,
    mut rx: mpsc::Receiver<JournalEntry>,
    alive: Arc<AtomicBool>,
) {
    while let Some(entry) = rx.recv().await {
        let line = match serde_json::to_string(&entry) {
            Ok(l) => l,
            Err(e) => {
                error!("Journal serialize error: {e}");
                continue;
            }
        };
        if let Err(e) = file.write_all(line.as_bytes()).await {
            error!("Journal write error: {e}");
            alive.store(false, Ordering::Relaxed);
            return;
        }
        if let Err(e) = file.write_all(b"\n").await {
            error!("Journal write error: {e}");
            alive.store(false, Ordering::Relaxed);
            return;
        }
        // Batch: drain all remaining entries in channel before flushing
        while let Ok(entry) = rx.try_recv() {
            let line = match serde_json::to_string(&entry) {
                Ok(l) => l,
                Err(e) => {
                    error!("Journal serialize error: {e}");
                    continue;
                }
            };
            if let Err(e) = file.write_all(line.as_bytes()).await {
                error!("Journal write error: {e}");
                alive.store(false, Ordering::Relaxed);
                return;
            }
            if let Err(e) = file.write_all(b"\n").await {
                error!("Journal write error: {e}");
                alive.store(false, Ordering::Relaxed);
                return;
            }
        }
        // Flush after draining batch
        if let Err(e) = file.flush().await {
            error!("Journal flush error: {e}");
            alive.store(false, Ordering::Relaxed);
            return;
        }
    }
}

/// Scan the journal directory and recover archived sessions from disk.
pub async fn recover_sessions(dir: &Path) -> Vec<ArchivedSession> {
    let sessions_dir = dir.join("sessions");
    let mut archived = Vec::new();

    let Ok(mut read_dir) = fs::read_dir(&sessions_dir).await else {
        return archived;
    };

    while let Ok(Some(entry)) = read_dir.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }

        let session_id = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };

        match recover_single_journal(&path, &session_id).await {
            Ok(session) => {
                info!(
                    "Recovered archived session {} ({} entries)",
                    session.session_id,
                    session.entries.len()
                );
                archived.push(session);
            }
            Err(e) => {
                warn!("Failed to recover journal {}: {e}", path.display());
            }
        }
    }

    archived
}

/// Parse a single journal file into an `ArchivedSession`.
async fn recover_single_journal(path: &Path, session_id: &str) -> Result<ArchivedSession, String> {
    let file = fs::File::open(path)
        .await
        .map_err(|e| format!("open: {e}"))?;
    let reader = BufReader::new(file);
    let mut lines = reader.lines();

    // First line is metadata
    let meta_line = lines
        .next_line()
        .await
        .map_err(|e| format!("read metadata: {e}"))?
        .ok_or_else(|| "empty journal file".to_string())?;

    let metadata: SessionMetadata =
        serde_json::from_str(&meta_line).map_err(|e| format!("parse metadata: {e}"))?;

    let mut entries = Vec::new();
    let mut exit_code = None;

    while let Ok(Some(line)) = lines.next_line().await {
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<JournalEntry>(&line) {
            Ok(je) => {
                // Check for exit info in system messages
                if je.t == 'x' {
                    if let Some(code) = parse_exit_code(&je.d) {
                        exit_code = Some(code);
                    }
                }
                entries.push(je.to_output_entry());
            }
            Err(e) => {
                warn!("Skipping corrupt journal line: {e}");
            }
        }
    }

    Ok(ArchivedSession {
        session_id: session_id.to_string(),
        metadata,
        entries,
        exit_code,
    })
}

/// Try to parse "Process exited with code N" from a system message.
fn parse_exit_code(msg: &str) -> Option<i32> {
    msg.strip_prefix("Process exited with code ")
        .and_then(|s| s.trim().parse().ok())
}

/// Delete journal files older than `max_age_hours`.
pub async fn cleanup_old_journals(dir: &Path, max_age_hours: u64) {
    let sessions_dir = dir.join("sessions");
    let max_age = std::time::Duration::from_secs(max_age_hours * 3600);

    let Ok(mut read_dir) = fs::read_dir(&sessions_dir).await else {
        return;
    };

    let now = SystemTime::now();

    while let Ok(Some(entry)) = read_dir.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(metadata) = fs::metadata(&path).await else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if let Ok(age) = now.duration_since(modified) {
            if age > max_age {
                info!("Removing old journal: {}", path.display());
                let _ = fs::remove_file(&path).await;
            }
        }
    }
}

/// Scan journals for sessions that were running when the server last died
/// (no exit code in journal). If those PIDs are still alive, gracefully kill
/// them — they're orphans we can't reconnect to (PTY/pipe fds are gone).
pub async fn kill_orphaned_processes(dir: &Path) {
    let sessions_dir = dir.join("sessions");

    let Ok(mut read_dir) = fs::read_dir(&sessions_dir).await else {
        return;
    };

    while let Ok(Some(entry)) = read_dir.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let session_id = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let Ok(archived) = recover_single_journal(&path, &session_id).await else {
            continue;
        };

        // Only care about sessions with no exit code (were still running)
        if archived.exit_code.is_some() {
            continue;
        }

        let pid = archived.metadata.pid;
        if pid == 0 {
            continue;
        }

        // Check if the PID is still alive
        #[allow(clippy::cast_possible_wrap)]
        let alive = unsafe { libc::kill(pid as i32, 0) } == 0;
        if !alive {
            continue;
        }

        // Verify it's plausibly our shell by checking /proc/PID/cmdline
        let cmdline_path = format!("/proc/{pid}/cmdline");
        let is_shell = std::fs::read(&cmdline_path)
            .ok()
            .and_then(|bytes| {
                // cmdline is NUL-separated; first arg is the executable
                let exe = bytes.split(|&b| b == 0).next()?;
                let exe_str = std::str::from_utf8(exe).ok()?;
                Some(exe_str.contains(&archived.metadata.shell))
            })
            .unwrap_or(false);

        if !is_shell {
            info!(
                "PID {pid} from session {session_id} is alive but doesn't match shell '{}', skipping",
                archived.metadata.shell
            );
            continue;
        }

        // Gracefully kill the orphan's process group
        info!(
            "Killing orphaned session {session_id} (PID {pid}, shell '{}')",
            archived.metadata.shell
        );
        #[allow(clippy::cast_possible_wrap)]
        let neg_pgid = -(pid as i32);
        unsafe {
            libc::kill(neg_pgid, libc::SIGTERM);
        }
        // Give it a moment, then force-kill if needed
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        #[allow(clippy::cast_possible_wrap)]
        let still_alive = unsafe { libc::kill(pid as i32, 0) } == 0;
        if still_alive {
            unsafe {
                libc::kill(neg_pgid, libc::SIGKILL);
            }
            info!("Orphaned PID {pid} required SIGKILL");
        }
    }
}

/// Get the sessions subdirectory path.
pub fn sessions_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("sessions")
}

/// Current timestamp in milliseconds.
pub fn now_ms() -> u64 {
    #[allow(clippy::cast_possible_truncation)]
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}
