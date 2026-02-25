//! Hot-reload supervisor for mcp-sctl.
//!
//! When `--supervisor` is passed, `mcp-sctl` runs as a transparent proxy:
//!
//! ```text
//! Claude Code (real stdin/stdout)
//!     │
//!     ▼
//! mcp-sctl --supervisor --config ... (this module, long-lived)
//!     │  ● Owns real stdio pipes
//!     │  ● Proxies JSON-RPC transparently
//!     │  ● Polls binary mtime every 5s, hashes on change
//!     │  ● Handles SIGUSR1 for manual reload
//!     │
//!     ▼ (child process pipes)
//! mcp-sctl --config ... (worker, replaced on reload)
//!     │  ● Existing MCP logic unchanged
//!     │  ● Exits cleanly on stdin EOF
//! ```
//!
//! Sessions live on the device, not in the MCP process. After a reload,
//! the new worker reconnects lazily via `session_attach`.

use indexmap::IndexMap;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::time::SystemTime;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

/// Reason the proxy loop exited.
enum ProxyExit {
    /// Worker stdout hit EOF (crash or clean exit).
    WorkerDied,
    /// Reload signal received (binary change or SIGUSR1).
    Reload,
}

/// Main entry point for supervisor mode.
///
/// Spawns the worker subprocess, proxies JSON-RPC between Claude's stdio and
/// the worker's stdio, and reloads the worker when the binary changes.
pub async fn run(args: Vec<String>) {
    let binary = std::env::current_exe().expect("cannot resolve own binary path");
    eprintln!(
        "mcp-sctl supervisor: watching {} for changes",
        binary.display()
    );

    // Build worker args: remove --supervisor from the arg list
    let worker_args: Vec<String> = args
        .iter()
        .filter(|a| *a != "--supervisor")
        .cloned()
        .collect();

    let mut state = SupervisorState::new(binary.clone(), worker_args);

    // Channel for reload signals (binary watcher + SIGUSR1)
    let (reload_tx, mut reload_rx) = mpsc::channel::<()>(4);

    // Start binary watcher
    let watcher_bin = binary.clone();
    let watcher_tx = reload_tx.clone();
    tokio::spawn(async move {
        binary_watcher(watcher_bin, watcher_tx).await;
    });

    // SIGUSR1 handler
    #[cfg(unix)]
    {
        let sig_tx = reload_tx;
        tokio::spawn(async move {
            let mut sig =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())
                    .expect("failed to register SIGUSR1 handler");
            loop {
                sig.recv().await;
                eprintln!("mcp-sctl supervisor: SIGUSR1 received, triggering reload");
                let _ = sig_tx.send(()).await;
            }
        });
    }

    // Read stdin from Claude in a background task
    let (stdin_tx, mut stdin_rx) = mpsc::channel::<String>(64);
    tokio::spawn(async move {
        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {
                    if stdin_tx.send(line.clone()).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        // Claude closed stdin — exit the whole supervisor
        std::process::exit(0);
    });

    let mut stdout = tokio::io::stdout();

    // Spawn initial worker
    let mut worker = spawn_worker(&state.binary, &state.worker_args);
    let mut w_stdin = worker.stdin.take().expect("worker stdin not captured");
    let mut w_stdout = worker.stdout.take().expect("worker stdout not captured");

    loop {
        // Proxy loop: forward between Claude and worker
        let exit_reason = proxy_loop(
            &mut stdin_rx,
            &mut reload_rx,
            &mut w_stdin,
            &mut w_stdout,
            &mut stdout,
            &mut state,
        )
        .await;

        match exit_reason {
            ProxyExit::Reload => {
                eprintln!("mcp-sctl supervisor: reloading worker...");
            }
            ProxyExit::WorkerDied => {
                eprintln!("mcp-sctl supervisor: worker exited, respawning...");
            }
        }

        // Drain phase: close worker stdin, wait for remaining responses
        drop(w_stdin);
        drain_worker(&mut w_stdout, &mut state, &mut stdout).await;
        let _ = worker.kill().await;
        let _ = worker.wait().await;

        state.generation += 1;
        eprintln!(
            "mcp-sctl supervisor: spawning worker generation {}",
            state.generation
        );

        // Spawn new worker
        worker = spawn_worker(&state.binary, &state.worker_args);
        w_stdin = worker.stdin.take().expect("worker stdin not captured");
        w_stdout = worker.stdout.take().expect("worker stdout not captured");

        // Re-initialize: send stored initialize request, consume the response
        if let Some(ref init_req) = state.init_request {
            let mut init_reader = BufReader::new(&mut w_stdout);

            if let Err(e) = w_stdin.write_all(init_req.as_bytes()).await {
                eprintln!("mcp-sctl supervisor: failed to re-initialize worker: {e}");
                std::process::exit(1);
            }
            let _ = w_stdin.flush().await;

            // Also send the notifications/initialized notification if stored
            if let Some(ref init_notif) = state.init_notification {
                let _ = w_stdin.write_all(init_notif.as_bytes()).await;
                let _ = w_stdin.flush().await;
            }

            // Read and discard the initialize response
            let mut init_response = String::new();
            match tokio::time::timeout(
                std::time::Duration::from_secs(10),
                init_reader.read_line(&mut init_response),
            )
            .await
            {
                Ok(Ok(n)) if n > 0 => {
                    eprintln!(
                        "mcp-sctl supervisor: worker re-initialized (gen {})",
                        state.generation
                    );
                }
                _ => {
                    eprintln!("mcp-sctl supervisor: worker failed to respond to initialize");
                    std::process::exit(1);
                }
            }
        }

        // Send tools/list_changed notification to Claude
        let notification =
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n";
        if let Err(e) = stdout.write_all(notification.as_bytes()).await {
            eprintln!("mcp-sctl supervisor: failed to send list_changed: {e}");
            std::process::exit(1);
        }
        let _ = stdout.flush().await;
        eprintln!("mcp-sctl supervisor: sent tools/list_changed to client");

        // Replay buffered requests
        let pending: Vec<String> = state.pending.values().cloned().collect();
        if !pending.is_empty() {
            eprintln!(
                "mcp-sctl supervisor: replaying {} buffered request(s)",
                pending.len()
            );
            for req in &pending {
                if let Err(e) = w_stdin.write_all(req.as_bytes()).await {
                    eprintln!("mcp-sctl supervisor: replay write error: {e}");
                    break;
                }
            }
            let _ = w_stdin.flush().await;
        }
    }
}

/// Proxy JSON-RPC lines between Claude (stdin_rx/stdout) and worker (w_stdin/w_stdout).
/// Returns the reason the loop exited.
async fn proxy_loop(
    stdin_rx: &mut mpsc::Receiver<String>,
    reload_rx: &mut mpsc::Receiver<()>,
    w_stdin: &mut tokio::process::ChildStdin,
    w_stdout: &mut tokio::process::ChildStdout,
    stdout: &mut tokio::io::Stdout,
    state: &mut SupervisorState,
) -> ProxyExit {
    let mut reader = BufReader::new(&mut *w_stdout);
    let mut worker_line = String::new();

    loop {
        worker_line.clear();
        tokio::select! {
            // Line from Claude → forward to worker
            Some(line) = stdin_rx.recv() => {
                state.track_request(&line);
                if let Err(e) = w_stdin.write_all(line.as_bytes()).await {
                    eprintln!("mcp-sctl supervisor: worker stdin write error: {e}");
                    return ProxyExit::WorkerDied;
                }
                let _ = w_stdin.flush().await;
            }

            // Line from worker → forward to Claude
            result = reader.read_line(&mut worker_line) => {
                match result {
                    Ok(0) => return ProxyExit::WorkerDied,
                    Ok(_) => {
                        state.track_response(&worker_line);
                        if let Err(e) = stdout.write_all(worker_line.as_bytes()).await {
                            eprintln!("mcp-sctl supervisor: stdout write error: {e}");
                            std::process::exit(1);
                        }
                        let _ = stdout.flush().await;
                    }
                    Err(e) => {
                        eprintln!("mcp-sctl supervisor: worker stdout read error: {e}");
                        return ProxyExit::WorkerDied;
                    }
                }
            }

            // Reload signal
            Some(()) = reload_rx.recv() => {
                return ProxyExit::Reload;
            }
        }
    }
}

/// Supervisor state tracking.
struct SupervisorState {
    binary: PathBuf,
    worker_args: Vec<String>,
    /// Stored `initialize` request for replay after reload.
    init_request: Option<String>,
    /// Stored `notifications/initialized` for replay after reload.
    init_notification: Option<String>,
    /// In-flight requests: id → full JSON line. Removed when response arrives.
    pending: IndexMap<String, String>,
    /// Worker generation counter.
    generation: u64,
}

impl SupervisorState {
    fn new(binary: PathBuf, worker_args: Vec<String>) -> Self {
        Self {
            binary,
            worker_args,
            init_request: None,
            init_notification: None,
            pending: IndexMap::new(),
            generation: 0,
        }
    }

    /// Track a request from Claude for potential replay.
    fn track_request(&mut self, line: &str) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return;
        }
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
            let method = val.get("method").and_then(|v| v.as_str()).unwrap_or("");

            if method == "initialize" {
                self.init_request = Some(line.to_string());
                return;
            }
            if method == "notifications/initialized" {
                self.init_notification = Some(line.to_string());
                return;
            }

            // Track requests with IDs (not notifications) as pending
            if let Some(id) = val.get("id") {
                let id_str = id.to_string();
                self.pending.insert(id_str, line.to_string());
            }
        }
    }

    /// Remove a pending request when its response arrives.
    fn track_response(&mut self, line: &str) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return;
        }
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if let Some(id) = val.get("id") {
                let id_str = id.to_string();
                self.pending.shift_remove(&id_str);
            }
        }
    }
}

/// Spawn a worker subprocess with piped stdin/stdout.
fn spawn_worker(binary: &PathBuf, worker_args: &[String]) -> tokio::process::Child {
    Command::new(binary)
        .args(worker_args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .unwrap_or_else(|e| {
            eprintln!("mcp-sctl supervisor: failed to spawn worker: {e}");
            std::process::exit(1);
        })
}

/// Drain remaining output from a dying worker (up to 10s timeout).
async fn drain_worker(
    w_stdout: &mut tokio::process::ChildStdout,
    state: &mut SupervisorState,
    stdout: &mut tokio::io::Stdout,
) {
    let mut reader = BufReader::new(&mut *w_stdout);
    let mut line = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            eprintln!("mcp-sctl supervisor: drain timeout");
            return;
        }

        line.clear();
        match tokio::time::timeout(remaining, reader.read_line(&mut line)).await {
            Ok(Ok(0)) => return, // Clean EOF
            Ok(Ok(_)) => {
                state.track_response(&line);
                let _ = stdout.write_all(line.as_bytes()).await;
                let _ = stdout.flush().await;

                if state.pending.is_empty() {
                    return;
                }
            }
            Ok(Err(_)) | Err(_) => return,
        }
    }
}

/// Background task that polls the binary's mtime and triggers reload on hash change.
async fn binary_watcher(binary: PathBuf, reload_tx: mpsc::Sender<()>) {
    let mut last_mtime: Option<SystemTime> = None;
    let mut last_hash: Option<Vec<u8>> = None;

    // Initialize with current state
    if let Ok(meta) = std::fs::metadata(&binary) {
        last_mtime = meta.modified().ok();
        last_hash = hash_file(&binary);
    }

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;

        let meta = match std::fs::metadata(&binary) {
            Ok(m) => m,
            Err(_) => continue, // Binary missing (mid-compile), skip cycle
        };

        let current_mtime = match meta.modified() {
            Ok(t) => t,
            Err(_) => continue,
        };

        // Quick check: mtime unchanged → skip
        if Some(current_mtime) == last_mtime {
            continue;
        }

        last_mtime = Some(current_mtime);

        // mtime changed — compute hash to confirm actual change
        let current_hash = match hash_file(&binary) {
            Some(h) => h,
            None => continue,
        };

        if Some(&current_hash) == last_hash.as_ref() {
            // mtime changed but content identical (cargo touched but didn't change)
            continue;
        }

        last_hash = Some(current_hash);
        eprintln!(
            "mcp-sctl supervisor: binary change detected ({})",
            binary.display()
        );
        let _ = reload_tx.send(()).await;
    }
}

/// Compute SHA-256 hash of a file.
fn hash_file(path: &PathBuf) -> Option<Vec<u8>> {
    let data = std::fs::read(path).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(&data);
    Some(hasher.finalize().to_vec())
}
