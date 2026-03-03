//! Built-in supervisor that restarts the server on crash.
//!
//! `sctl supervise` forks `sctl serve` and monitors it. On abnormal
//! exit the server is restarted with exponential backoff. A clean exit (code 0)
//! causes the supervisor to stop. SIGINT/SIGTERM trigger graceful shutdown:
//! the signal is forwarded to the child, and once the child exits the supervisor
//! exits too (no restart).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::process::Command;
use tracing::{error, info, warn};

use sctl::config::SupervisorConfig;

/// Run the supervisor loop. Does not return unless the child exits cleanly.
pub async fn run_supervisor(config_path: Option<&str>, sup_config: &SupervisorConfig) -> ! {
    let mut backoff = 1u64;
    let max_backoff = sup_config.max_backoff;
    let stable_threshold = Duration::from_secs(sup_config.stable_threshold);

    let exe = std::env::current_exe().expect("resolve own executable path");

    // Shared shutdown flag — set by SIGINT/SIGTERM handler so the main loop
    // knows not to restart the child.
    let shutting_down = Arc::new(AtomicBool::new(false));

    loop {
        let started = Instant::now();

        let mut cmd = Command::new(&exe);
        cmd.arg("serve");
        if let Some(p) = config_path {
            cmd.args(["--config", p]);
        }

        let mut child = cmd.spawn().expect("failed to spawn server process");
        let server_pid = child.id();
        info!("Supervisor: started server (pid {server_pid:?})");

        // Forward SIGINT and SIGTERM to child, and set shutdown flag
        let fwd_pid = server_pid;
        let sd = Arc::clone(&shutting_down);
        let _signal_task = tokio::spawn(async move {
            let mut sigint =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                    .expect("register SIGINT");
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("register SIGTERM");
            tokio::select! {
                _ = sigint.recv() => {
                    info!("Supervisor: received SIGINT, shutting down");
                    sd.store(true, Ordering::SeqCst);
                    if let Some(pid) = fwd_pid {
                        #[allow(clippy::cast_possible_wrap)]
                        unsafe { libc::kill(pid as i32, libc::SIGINT); }
                    }
                }
                _ = sigterm.recv() => {
                    info!("Supervisor: received SIGTERM, shutting down");
                    sd.store(true, Ordering::SeqCst);
                    if let Some(pid) = fwd_pid {
                        #[allow(clippy::cast_possible_wrap)]
                        unsafe { libc::kill(pid as i32, libc::SIGTERM); }
                    }
                }
            }
        });

        let status = child.wait().await;
        let uptime = started.elapsed();

        // If we received a shutdown signal, always exit regardless of child status
        if shutting_down.load(Ordering::SeqCst) {
            info!("Supervisor: child exited after shutdown signal ({status:?}), stopping");
            std::process::exit(0);
        }

        match status {
            Ok(s) if s.success() => {
                info!("Server exited cleanly, supervisor stopping");
                std::process::exit(0);
            }
            Ok(s) => {
                warn!(
                    "Server exited: {s} (uptime {:.1}s), restarting in {backoff}s",
                    uptime.as_secs_f64()
                );
                tokio::time::sleep(Duration::from_secs(backoff)).await;

                // Check again after sleep — a signal may have arrived during backoff
                if shutting_down.load(Ordering::SeqCst) {
                    info!("Supervisor: shutdown requested during backoff, stopping");
                    std::process::exit(0);
                }

                if uptime >= stable_threshold {
                    backoff = 1;
                } else {
                    backoff = (backoff * 2).min(max_backoff);
                }
            }
            Err(e) => {
                error!(
                    "Server wait error: {e} (uptime {:.1}s), restarting in {backoff}s",
                    uptime.as_secs_f64()
                );
                tokio::time::sleep(Duration::from_secs(backoff)).await;

                if shutting_down.load(Ordering::SeqCst) {
                    info!("Supervisor: shutdown requested during backoff, stopping");
                    std::process::exit(0);
                }

                if uptime >= stable_threshold {
                    backoff = 1;
                } else {
                    backoff = (backoff * 2).min(max_backoff);
                }
            }
        }
    }
}
