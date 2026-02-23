//! Low-level process spawning and output capture.
//!
//! All shell interaction ultimately goes through the two functions here:
//! [`spawn_shell`] for interactive sessions and [`exec_command`] for one-shot
//! commands. Both set `kill_on_drop(true)` so orphaned processes are cleaned up
//! if the owning task is cancelled.

use std::collections::HashMap;
use std::fmt::Write;
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

/// Max output size per stream for [`exec_command`] (1 MB).
///
/// Output beyond this limit is still drained from the pipe (to prevent
/// deadlocks) but discarded. A truncation notice is appended to the returned
/// string.
const MAX_EXEC_OUTPUT: usize = 1024 * 1024;

/// Spawn an interactive shell with piped stdin/stdout/stderr.
///
/// The returned [`Child`] has `kill_on_drop(true)`, so dropping it sends
/// SIGKILL. Callers are expected to take ownership of the stdio handles via
/// `child.stdin.take()` etc.
///
/// **Note:** For sessions that need signal delivery to the process tree, use
/// [`spawn_shell_pgroup`] instead.
#[allow(dead_code)]
pub fn spawn_shell(shell: &str, working_dir: &str) -> std::io::Result<Child> {
    Command::new(shell)
        .current_dir(working_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
}

/// Spawn an interactive shell in its own process group with piped I/O.
///
/// Like [`spawn_shell`] but calls `setpgid(0, 0)` via `pre_exec` so the shell
/// becomes a process group leader. This allows sending signals (e.g. SIGINT) to
/// the entire process tree via `kill(-pgid, signal)`.
///
/// Also accepts optional environment variables to merge into the child's
/// inherited environment.
pub fn spawn_shell_pgroup(
    shell: &str,
    working_dir: &str,
    env: Option<&HashMap<String, String>>,
) -> std::io::Result<Child> {
    let mut cmd = Command::new(shell);
    cmd.current_dir(working_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(vars) = env {
        cmd.envs(vars);
    }
    // SAFETY: setpgid is async-signal-safe per POSIX.
    unsafe {
        cmd.pre_exec(|| {
            libc::setpgid(0, 0);
            Ok(())
        });
    }
    cmd.spawn()
}

/// Execute a one-shot command via `<shell> -c "<command>"` and capture output.
///
/// Stdout and stderr are read concurrently (to avoid pipe deadlock) and each
/// capped at [`MAX_EXEC_OUTPUT`] bytes. The entire operation is wrapped in a
/// `tokio::time::timeout`.
///
/// # Environment variables
///
/// When `env` is `Some`, the provided variables are **merged into** (not
/// replacing) the inherited environment. To override `PATH`, include it in the
/// map.
pub async fn exec_command(
    shell: &str,
    working_dir: &str,
    command: &str,
    timeout_ms: u64,
    env: Option<&HashMap<String, String>>,
) -> Result<ExecResult, ExecError> {
    let start = std::time::Instant::now();

    let mut cmd = Command::new(shell);
    cmd.arg("-c")
        .arg(command)
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(vars) = env {
        cmd.envs(vars);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| ExecError::SpawnFailed(e.to_string()))?;

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| ExecError::ProcessFailed("Failed to take stdout pipe".to_string()))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| ExecError::ProcessFailed("Failed to take stderr pipe".to_string()))?;

    let timeout = tokio::time::Duration::from_millis(timeout_ms);
    match Box::pin(tokio::time::timeout(timeout, async {
        // Read stdout and stderr concurrently to avoid pipe deadlock
        let (stdout_data, stderr_data) = tokio::join!(
            read_capped(&mut stdout, MAX_EXEC_OUTPUT),
            read_capped(&mut stderr, MAX_EXEC_OUTPUT),
        );
        // Drop pipe handles so child sees EOF
        drop(stdout);
        drop(stderr);

        let status = child
            .wait()
            .await
            .map_err(|e| ExecError::ProcessFailed(e.to_string()))?;

        #[allow(clippy::cast_possible_truncation)]
        let duration_ms = start.elapsed().as_millis() as u64;

        Ok::<_, ExecError>(ExecResult {
            exit_code: status.code().unwrap_or(-1),
            stdout: stdout_data,
            stderr: stderr_data,
            duration_ms,
        })
    }))
    .await
    {
        Ok(result) => result,
        Err(_) => Err(ExecError::Timeout),
    }
}

/// Read from an async reader, keeping the first `max_bytes` and discarding the
/// rest.
///
/// Crucially, this function continues reading past the cap instead of closing
/// the pipe early — closing a pipe while the child is still writing causes
/// SIGPIPE / broken pipe errors and potential deadlocks when the child is also
/// writing to the other stream.
async fn read_capped(reader: &mut (impl tokio::io::AsyncRead + Unpin), max_bytes: usize) -> String {
    let mut buf = Vec::with_capacity(max_bytes.min(65536));
    let mut tmp = [0u8; 8192];
    let mut total_read = 0usize;
    loop {
        match reader.read(&mut tmp).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                total_read += n;
                if buf.len() < max_bytes {
                    let take = n.min(max_bytes - buf.len());
                    buf.extend_from_slice(&tmp[..take]);
                }
            }
        }
    }
    let mut s = String::from_utf8_lossy(&buf).into_owned();
    if total_read > max_bytes {
        let _ = write!(
            s,
            "\n[truncated: {total_read} bytes total, showing first {max_bytes}]"
        );
    }
    s
}

/// Successful result of [`exec_command`].
#[derive(Debug, serde::Serialize)]
pub struct ExecResult {
    /// Process exit code, or `-1` if the code was unavailable (e.g. killed by signal).
    pub exit_code: i32,
    /// Captured stdout (capped at [`MAX_EXEC_OUTPUT`], lossy UTF-8 conversion).
    pub stdout: String,
    /// Captured stderr (capped at [`MAX_EXEC_OUTPUT`], lossy UTF-8 conversion).
    pub stderr: String,
    /// Wall-clock duration of the command in milliseconds.
    pub duration_ms: u64,
}

/// Errors that can occur during [`exec_command`].
#[derive(Debug)]
pub enum ExecError {
    /// The shell binary could not be started (e.g. not found, permission denied).
    SpawnFailed(String),
    /// The child process started but `wait()` failed.
    ProcessFailed(String),
    /// The command exceeded its timeout and was killed.
    Timeout,
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecError::SpawnFailed(e) => write!(f, "Failed to spawn process: {e}"),
            ExecError::ProcessFailed(e) => write!(f, "Process error: {e}"),
            ExecError::Timeout => write!(f, "Command timed out"),
        }
    }
}
