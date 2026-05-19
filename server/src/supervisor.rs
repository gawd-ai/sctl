//! Built-in supervisor that restarts the server on crash.
//!
//! `sctl supervise` forks `sctl serve` and monitors it. On abnormal
//! exit the server is restarted with exponential backoff. A clean exit (code 0)
//! causes the supervisor to stop. SIGINT/SIGTERM trigger graceful shutdown:
//! the signal is forwarded to the child, and once the child exits the supervisor
//! exits too (no restart).
//!
//! ## Crash-loop detection
//!
//! If the child exits ≥3 times within 180 s (i.e. consistently failing in <60 s
//! per attempt), the supervisor writes `<data_dir>/safe_mode.flag`. The next
//! child instance reads this flag and skips every optional subsystem (modem,
//! GPS, LTE, watchdog, infra) to break the loop while keeping HTTP+tunnel+
//! sessions live so an operator can reach the box.
//!
//! `RUST_BACKTRACE=full` is set unconditionally in the child env so the panic
//! hook in `run_server` captures useful traces.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::process::Command;
use tracing::{error, info, warn};

use sctl::config::SupervisorConfig;

/// Crash threshold: this many recent failures within `CRASH_LOOP_WINDOW`
/// triggers safe-mode.
const CRASH_LOOP_THRESHOLD: usize = 3;
const CRASH_LOOP_WINDOW: Duration = Duration::from_secs(180);

/// Once safe-mode is engaged the supervisor backs off restarts to this
/// interval (vs. the regular exponential ramp) so we don't spin on a flag the
/// operator hasn't cleared.
const SAFE_MODE_BACKOFF_SECS: u64 = 300;

/// Write the safe-mode flag. Best-effort — failure to write is logged, not
/// fatal (worst case: the next restart attempts normal startup again).
fn write_safe_mode_flag(data_dir: &str, reason: &str, consecutive: usize) {
    let path = Path::new(data_dir).join("safe_mode.flag");
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let payload = serde_json::json!({
        "since_unix": ts,
        "reason": reason,
        "consecutive_crashes": consecutive,
    });
    let tmp = path.with_extension("flag.tmp");
    if let Err(e) = std::fs::write(&tmp, payload.to_string()) {
        warn!("supervisor: write safe_mode.flag tmp failed: {e}");
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        warn!("supervisor: rename safe_mode.flag failed: {e}");
    }
}

/// Record this crash, decide whether to engage safe-mode, and return the
/// supervisor's chosen backoff for the upcoming restart.
///
/// Trim the rolling window to the last `CRASH_LOOP_WINDOW`. If the window now
/// contains at least `CRASH_LOOP_THRESHOLD` crashes, write `safe_mode.flag`
/// and stretch the backoff to `SAFE_MODE_BACKOFF_SECS`. The flag itself is
/// idempotent — repeated writes just refresh the timestamp.
fn record_crash_and_pick_backoff(
    history: &mut VecDeque<Instant>,
    engaged: &mut bool,
    data_dir: &str,
    normal_backoff: u64,
    reason: &str,
) -> u64 {
    let now = Instant::now();
    history.push_back(now);
    while let Some(front) = history.front() {
        if now.duration_since(*front) > CRASH_LOOP_WINDOW {
            history.pop_front();
        } else {
            break;
        }
    }
    if history.len() >= CRASH_LOOP_THRESHOLD {
        if !*engaged {
            warn!(
                "Supervisor: crash-loop detected ({} crashes in {}s) — engaging safe mode",
                history.len(),
                CRASH_LOOP_WINDOW.as_secs()
            );
        }
        *engaged = true;
        write_safe_mode_flag(data_dir, reason, history.len());
        SAFE_MODE_BACKOFF_SECS
    } else {
        normal_backoff
    }
}

/// Run the supervisor loop. Does not return unless the child exits cleanly.
#[allow(clippy::too_many_lines)]
pub async fn run_supervisor(config_path: Option<&str>, sup_config: &SupervisorConfig) -> ! {
    let mut backoff = 1u64;
    let max_backoff = sup_config.max_backoff;
    let stable_threshold = Duration::from_secs(sup_config.stable_threshold);

    let exe = std::env::current_exe().expect("resolve own executable path");

    // Resolve data_dir from the same config the child will load — needed for
    // safe_mode.flag persistence.
    let data_dir = sctl::Config::load(config_path).server.data_dir;

    // Shared shutdown flag — set by SIGINT/SIGTERM handler so the main loop
    // knows not to restart the child.
    let shutting_down = Arc::new(AtomicBool::new(false));

    // Rolling window of recent crash timestamps for crash-loop detection.
    let mut crash_history: VecDeque<Instant> = VecDeque::with_capacity(CRASH_LOOP_THRESHOLD);
    let mut safe_mode_engaged = false;

    loop {
        let started = Instant::now();

        let mut cmd = Command::new(&exe);
        cmd.args(["serve", "--skip-lock"]);
        if let Some(p) = config_path {
            cmd.args(["--config", p]);
        }
        // Surface full backtraces so the panic hook in run_server can persist
        // them. Equivalent to running with RUST_BACKTRACE=full set.
        cmd.env("RUST_BACKTRACE", "full");

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
            Ok(s) if s.code() == Some(99) => {
                warn!("Supervisor: child reports another instance is running, stopping");
                std::process::exit(0);
            }
            Ok(s) if s.success() => {
                info!("Server exited cleanly, supervisor stopping");
                std::process::exit(0);
            }
            Ok(s) => {
                let effective_backoff = record_crash_and_pick_backoff(
                    &mut crash_history,
                    &mut safe_mode_engaged,
                    &data_dir,
                    backoff,
                    "child_exit_nonzero",
                );
                warn!(
                    "Server exited: {s} (uptime {:.1}s), restarting in {effective_backoff}s",
                    uptime.as_secs_f64()
                );
                tokio::time::sleep(Duration::from_secs(effective_backoff)).await;

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
                let effective_backoff = record_crash_and_pick_backoff(
                    &mut crash_history,
                    &mut safe_mode_engaged,
                    &data_dir,
                    backoff,
                    "child_wait_error",
                );
                error!(
                    "Server wait error: {e} (uptime {:.1}s), restarting in {effective_backoff}s",
                    uptime.as_secs_f64()
                );
                tokio::time::sleep(Duration::from_secs(effective_backoff)).await;

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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_data_dir(name: &str) -> std::path::PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!(
            "sctl-supervisor-test-{name}-{}",
            std::process::id()
        ));
        // Best-effort cleanup of prior runs.
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("mkdir temp data_dir");
        d
    }

    #[test]
    fn crash_loop_detection_writes_safe_mode_flag() {
        let dir = temp_data_dir("crash-loop");
        let dd = dir.to_string_lossy().into_owned();
        let mut history: VecDeque<Instant> = VecDeque::new();
        let mut engaged = false;

        // Two failures: under threshold, no flag yet.
        record_crash_and_pick_backoff(&mut history, &mut engaged, &dd, 1, "test");
        record_crash_and_pick_backoff(&mut history, &mut engaged, &dd, 1, "test");
        assert!(!engaged, "should not engage on 2 crashes");
        assert!(!dir.join("safe_mode.flag").exists(), "no flag at 2 crashes");

        // Third failure within the window → flag written + safe-mode engaged.
        let backoff = record_crash_and_pick_backoff(&mut history, &mut engaged, &dd, 1, "test");
        assert!(engaged, "should engage on 3rd crash in window");
        assert_eq!(
            backoff, SAFE_MODE_BACKOFF_SECS,
            "backoff stretches under safe-mode"
        );
        let flag = dir.join("safe_mode.flag");
        assert!(flag.exists(), "safe_mode.flag must exist on disk");

        // Cleanup.
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn safe_mode_flag_is_a_real_file_for_child_to_read() {
        // The child reads this on startup via path.exists(). This test
        // confirms the flag is a regular file (not a directory or symlink)
        // and is parsable JSON — both expected by the child.
        let dir = temp_data_dir("flag-shape");
        let dd = dir.to_string_lossy().into_owned();
        write_safe_mode_flag(&dd, "test_reason", 5);
        let flag = dir.join("safe_mode.flag");
        assert!(flag.exists(), "flag file must exist");
        let raw = std::fs::read_to_string(&flag).expect("read flag");
        let parsed: serde_json::Value = serde_json::from_str(&raw).expect("flag is JSON");
        assert_eq!(parsed["reason"], "test_reason");
        assert_eq!(parsed["consecutive_crashes"], 5);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
