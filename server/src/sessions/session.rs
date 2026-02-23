//! Buffer-backed managed session with process group and PTY support.
//!
//! A [`ManagedSession`] wraps a shell process whose stdout/stderr are written
//! to an [`OutputBuffer`] instead of being coupled directly to a WebSocket.
//! This allows the session to survive subscriber disconnects — the buffer
//! keeps accumulating output and a new subscriber can catch up later.
//!
//! ## Process groups
//!
//! The shell is spawned via [`crate::shell::process::spawn_shell_pgroup`] so it becomes a process
//! group leader. Signals sent to `-pgid` reach the entire process tree,
//! giving us real Ctrl-C behavior.
//!
//! ## PTY sessions
//!
//! When `pty: true` is requested, the session uses a PTY instead of pipes.
//! This enables TUI programs, `isatty()` detection, and terminal resize. The
//! PTY merges stdout+stderr into a single stream.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::io::RawFd;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Child;
use tokio::sync::{mpsc, Mutex};
use tracing::{error, info};

use super::buffer::{OutputBuffer, OutputStream};
use crate::shell::pty;

/// Session lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    Running,
    Exited,
}

/// A running shell session with buffer-backed I/O.
pub struct ManagedSession {
    /// OS process ID of the shell.
    pub pid: u32,
    /// Process group ID (equals pid since the shell is the group leader).
    pub pgid: u32,
    /// Shared output buffer.
    pub buffer: Arc<Mutex<OutputBuffer>>,
    /// Session lifecycle status.
    pub status: Arc<Mutex<SessionStatus>>,
    /// Exit code, set when the process exits.
    pub exit_code: Arc<Mutex<Option<i32>>>,
    /// Channel to write data to the shell's stdin (raw bytes).
    stdin_tx: mpsc::Sender<Vec<u8>>,
    /// Handles to the background I/O tasks — aborted on kill.
    tasks: Vec<tokio::task::JoinHandle<()>>,
    /// PTY master fd (only set for PTY sessions). Kept alive for resize.
    pty_master: Option<OwnedFd>,
}

impl ManagedSession {
    /// Spawn a new pipe-backed managed session from an already-created `Child`.
    ///
    /// Takes ownership of the child's stdio handles and spawns four background
    /// tasks (stdin writer, stdout reader, stderr reader, exit watcher) that
    /// route I/O through the [`OutputBuffer`].
    pub fn spawn(session_id: String, mut child: Child, buffer_size: usize) -> Result<Self, String> {
        let process_id = child.id().unwrap_or(0);
        // pgid = pid because the shell is the process group leader via setpgid(0,0)
        let process_group_id = process_id;

        let stdin = child.stdin.take().ok_or("Failed to take stdin pipe")?;
        let stdout = child.stdout.take().ok_or("Failed to take stdout pipe")?;
        let stderr = child.stderr.take().ok_or("Failed to take stderr pipe")?;

        let buffer = Arc::new(Mutex::new(OutputBuffer::new(buffer_size)));
        let status = Arc::new(Mutex::new(SessionStatus::Running));
        let exit_code: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));

        // stdin writer task
        let (stdin_tx, mut stdin_rx) = mpsc::channel::<Vec<u8>>(64);
        let stdin_task = tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(data) = stdin_rx.recv().await {
                if stdin.write_all(&data).await.is_err() {
                    break;
                }
                if stdin.flush().await.is_err() {
                    break;
                }
            }
        });

        // stdout reader task — chunk-based for immediate delivery
        let sid_out = session_id.clone();
        let buf_out = Arc::clone(&buffer);
        let stdout_task = tokio::spawn(async move {
            let mut stdout = stdout;
            let mut tmp = [0u8; 4096];
            loop {
                match stdout.read(&mut tmp).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let data = String::from_utf8_lossy(&tmp[..n]).into_owned();
                        buf_out.lock().await.push(OutputStream::Stdout, data);
                    }
                }
            }
            info!("Session {sid_out} stdout closed");
        });

        // stderr reader task — chunk-based
        let sid_err = session_id.clone();
        let buf_err = Arc::clone(&buffer);
        let stderr_task = tokio::spawn(async move {
            let mut stderr = stderr;
            let mut tmp = [0u8; 4096];
            loop {
                match stderr.read(&mut tmp).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let data = String::from_utf8_lossy(&tmp[..n]).into_owned();
                        buf_err.lock().await.push(OutputStream::Stderr, data);
                    }
                }
            }
            info!("Session {sid_err} stderr closed");
        });

        // Exit watcher task
        let sid_exit = session_id;
        let buf_exit = Arc::clone(&buffer);
        let status_exit = Arc::clone(&status);
        let exit_code_exit = Arc::clone(&exit_code);
        let exit_task = tokio::spawn(async move {
            match child.wait().await {
                Ok(s) => {
                    let code = s.code().unwrap_or(-1);
                    info!("Session {sid_exit} exited with code {code}");
                    *exit_code_exit.lock().await = Some(code);
                    buf_exit.lock().await.push(
                        OutputStream::System,
                        format!("Process exited with code {code}"),
                    );
                }
                Err(e) => {
                    error!("Session {sid_exit} wait error: {e}");
                    *exit_code_exit.lock().await = Some(-1);
                    buf_exit
                        .lock()
                        .await
                        .push(OutputStream::System, format!("Process wait error: {e}"));
                }
            }
            *status_exit.lock().await = SessionStatus::Exited;
        });

        Ok(ManagedSession {
            pid: process_id,
            pgid: process_group_id,
            buffer,
            status,
            exit_code,
            stdin_tx,
            tasks: vec![stdin_task, stdout_task, stderr_task, exit_task],
            pty_master: None,
        })
    }

    /// Spawn a PTY-backed session. Output is a single merged stream.
    ///
    /// Only 3 background tasks: stdin writer (to PTY master), output reader
    /// (from PTY master), and exit watcher.
    pub fn spawn_pty(
        session_id: String,
        mut child: Child,
        pty_master: OwnedFd,
        buffer_size: usize,
    ) -> Result<Self, String> {
        let process_id = child.id().unwrap_or(0);
        // For PTY sessions the child is a session leader via setsid(), so
        // the pgid equals the pid.
        let process_group_id = process_id;

        let buffer = Arc::new(Mutex::new(OutputBuffer::new(buffer_size)));
        let status = Arc::new(Mutex::new(SessionStatus::Running));
        let exit_code: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));

        let master_raw: RawFd = pty_master.as_raw_fd();

        // Dup the master fd: one for writing, one for reading, one kept for resize
        let writer_fd: RawFd = unsafe { libc::dup(master_raw) };
        if writer_fd < 0 {
            return Err(format!(
                "dup() failed for PTY master writer: {}",
                std::io::Error::last_os_error()
            ));
        }
        let reader_fd: RawFd = unsafe { libc::dup(master_raw) };
        if reader_fd < 0 {
            // Close the first dup'd fd before returning
            unsafe {
                libc::close(writer_fd);
            }
            return Err(format!(
                "dup() failed for PTY master reader: {}",
                std::io::Error::last_os_error()
            ));
        }

        // Convert to tokio async file handles (File wraps std::fs::File → tokio)
        // SAFETY: we own these file descriptors via dup
        let master_write =
            tokio::fs::File::from_std(unsafe { std::fs::File::from_raw_fd(writer_fd) });
        let master_read =
            tokio::fs::File::from_std(unsafe { std::fs::File::from_raw_fd(reader_fd) });

        // stdin writer task: mpsc → PTY master (write side)
        let (stdin_tx, mut stdin_rx) = mpsc::channel::<Vec<u8>>(64);
        let stdin_task = tokio::spawn(async move {
            let mut writer = master_write;
            while let Some(data) = stdin_rx.recv().await {
                if writer.write_all(&data).await.is_err() {
                    break;
                }
                if writer.flush().await.is_err() {
                    break;
                }
            }
        });

        // Output reader task: PTY master (read side) → buffer
        let sid_out = session_id.clone();
        let buf_out = Arc::clone(&buffer);
        let output_task = tokio::spawn(async move {
            let mut reader = master_read;
            let mut tmp = [0u8; 4096];
            loop {
                match reader.read(&mut tmp).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let data = String::from_utf8_lossy(&tmp[..n]).into_owned();
                        buf_out.lock().await.push(OutputStream::Stdout, data);
                    }
                }
            }
            info!("Session {sid_out} PTY output closed");
        });

        // Exit watcher task
        let sid_exit = session_id;
        let buf_exit = Arc::clone(&buffer);
        let status_exit = Arc::clone(&status);
        let exit_code_exit = Arc::clone(&exit_code);
        let exit_task = tokio::spawn(async move {
            match child.wait().await {
                Ok(s) => {
                    let code = s.code().unwrap_or(-1);
                    info!("Session {sid_exit} exited with code {code}");
                    *exit_code_exit.lock().await = Some(code);
                    buf_exit.lock().await.push(
                        OutputStream::System,
                        format!("Process exited with code {code}"),
                    );
                }
                Err(e) => {
                    error!("Session {sid_exit} wait error: {e}");
                    *exit_code_exit.lock().await = Some(-1);
                    buf_exit
                        .lock()
                        .await
                        .push(OutputStream::System, format!("Process wait error: {e}"));
                }
            }
            *status_exit.lock().await = SessionStatus::Exited;
        });

        // pty_master OwnedFd stays alive for resize operations. The dup'd fds
        // for read/write are independent and will be closed when their tasks end.
        Ok(ManagedSession {
            pid: process_id,
            pgid: process_group_id,
            buffer,
            status,
            exit_code,
            stdin_tx,
            tasks: vec![stdin_task, output_task, exit_task],
            pty_master: Some(pty_master),
        })
    }

    /// Create an archived (read-only) session from recovered journal data.
    pub fn archived(buffer: OutputBuffer, exit_code: Option<i32>) -> Self {
        let (stdin_tx, _) = mpsc::channel(1);
        ManagedSession {
            pid: 0,
            pgid: 0,
            buffer: Arc::new(Mutex::new(buffer)),
            status: Arc::new(Mutex::new(SessionStatus::Exited)),
            exit_code: Arc::new(Mutex::new(exit_code)),
            stdin_tx,
            tasks: Vec::new(),
            pty_master: None,
        }
    }

    /// Send data to the session's stdin (as UTF-8 string).
    pub async fn write_stdin(&self, data: &str) -> Result<(), String> {
        self.stdin_tx
            .send(data.as_bytes().to_vec())
            .await
            .map_err(|_| "Session stdin closed".to_string())
    }

    /// Send raw bytes to the session's stdin.
    #[allow(dead_code)]
    pub async fn write_stdin_bytes(&self, data: Vec<u8>) -> Result<(), String> {
        self.stdin_tx
            .send(data)
            .await
            .map_err(|_| "Session stdin closed".to_string())
    }

    /// Send a signal to the entire process group.
    ///
    /// Uses `kill(-pgid, signal)` which delivers to all processes in the group.
    /// In PTY sessions, the kernel's TTY job control layer protects the shell —
    /// SIGINT only reaches the foreground job. In non-PTY (pipe) sessions there
    /// is no TTY layer, so the signal hits the shell itself and will typically
    /// terminate the entire session.
    pub fn send_signal(&self, signal: i32) -> Result<(), String> {
        #[allow(clippy::cast_possible_wrap)]
        let pgid = self.pgid as i32;
        // kill(-pgid, signal) sends to all processes in the group
        let ret = unsafe { libc::kill(-pgid, signal) };
        if ret == 0 {
            Ok(())
        } else {
            Err(format!(
                "kill(-{}, {}) failed: {}",
                self.pgid,
                signal,
                std::io::Error::last_os_error()
            ))
        }
    }

    /// Kill the session immediately by sending SIGKILL to the process group
    /// and aborting all background tasks.
    pub fn kill(&self) {
        #[allow(clippy::cast_possible_wrap)]
        let pgid = self.pgid as i32;
        if pgid > 0 {
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
        for task in &self.tasks {
            task.abort();
        }
    }

    /// Gracefully kill the session: SIGTERM first, wait up to 3 s for the
    /// process to exit, then SIGKILL if it's still running.
    pub async fn graceful_kill(&self) {
        #[allow(clippy::cast_possible_wrap)]
        let pgid = self.pgid as i32;
        if pgid <= 0 {
            // Archived or already-dead session — just abort tasks.
            for task in &self.tasks {
                task.abort();
            }
            return;
        }

        // Phase 1: SIGTERM
        unsafe {
            libc::kill(-pgid, libc::SIGTERM);
        }

        // Phase 2: poll status for up to 3 seconds
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(3);
        loop {
            if *self.status.lock().await == SessionStatus::Exited {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                // Still running — force kill
                unsafe {
                    libc::kill(-pgid, libc::SIGKILL);
                }
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }

        for task in &self.tasks {
            task.abort();
        }
    }

    /// Resize the PTY (no-op error for pipe sessions).
    pub fn resize(&self, rows: u16, cols: u16) -> Result<(), String> {
        if let Some(ref master) = self.pty_master {
            pty::resize_pty(master, rows, cols).map_err(|e| e.to_string())
        } else {
            Err("Not a PTY session".into())
        }
    }

    /// Abort all background I/O tasks (stdin writer, readers, exit watcher).
    pub fn abort_tasks(&self) {
        for task in &self.tasks {
            task.abort();
        }
    }

    /// Whether this is a PTY-backed session.
    pub fn is_pty(&self) -> bool {
        self.pty_master.is_some()
    }
}
