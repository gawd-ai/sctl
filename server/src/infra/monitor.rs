//! Monitoring loop — runs as a long-lived tokio task.
//!
//! Spawned when config is first pushed (or loaded from disk at startup).
//! Aborted and re-spawned on config change.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tracing::{debug, info};

use super::checks::{self, CheckResult};
use super::{
    now_epoch, now_iso, InfraConfig, InfraState, RecoveryLogEntry, TargetState, TargetStatus,
};

/// Spawn the infra monitoring loop. Returns a `JoinHandle` that the caller
/// should store so it can be aborted on config change or shutdown.
pub fn spawn_monitor(
    infra_state: Arc<Mutex<InfraState>>,
    config: InfraConfig,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!(
            "Infra monitor started: {} targets, config v{}",
            config.targets.len(),
            config.version
        );

        // Build per-target tracking: last_check_epoch for interval gating
        let mut last_check: HashMap<String, u64> = HashMap::new();

        let tick_interval = Duration::from_secs(5); // check every 5s which targets are due
        let mut interval = tokio::time::interval(tick_interval);

        loop {
            interval.tick().await;
            let now = now_epoch();

            for target in &config.targets {
                let target_interval = target.interval_secs.unwrap_or(config.check_interval_secs);
                let last = last_check.get(&target.id).copied().unwrap_or(0);

                if now.saturating_sub(last) < target_interval {
                    continue; // not due yet
                }
                last_check.insert(target.id.clone(), now);

                debug!("Checking target {}: {}", target.id, target.name);
                let result = checks::run_check(&target.check).await;

                // Update state under lock
                let mut state = infra_state.lock().await;
                let prev_status = state
                    .results
                    .targets
                    .get(&target.id)
                    .map_or(TargetStatus::Unknown, |t| t.status);

                let current = state.results.targets.get(&target.id);
                let new_status = compute_status(
                    prev_status,
                    &result,
                    target.degraded_threshold_ms,
                    target.down_after_consecutive,
                    target.up_after_consecutive,
                    current,
                );

                let since = if new_status == prev_status {
                    current.map_or_else(now_iso, |t| t.since.clone())
                } else {
                    now_iso()
                };

                let (consecutive_ok, consecutive_fail) = if result.ok {
                    let prev_ok = current.map_or(0, |t| t.consecutive_ok);
                    (prev_ok.saturating_add(1), 0)
                } else {
                    let prev_fail = current.map_or(0, |t| t.consecutive_fail);
                    (0, prev_fail.saturating_add(1))
                };

                state.results.targets.insert(
                    target.id.clone(),
                    TargetState {
                        status: new_status,
                        latency_ms: result.latency_ms,
                        since,
                        consecutive_ok,
                        consecutive_fail,
                        last_check: now_iso(),
                        detail: result.detail.clone(),
                        name: target.name.clone(),
                    },
                );
                state.results.ts = now_iso();
                state.results.config_version = config.version;

                // Recovery action: fire on any DOWN state (new transition or sustained)
                if new_status == TargetStatus::Down {
                    if let Some(ref recovery) = target.recovery {
                        if recovery.enabled {
                            try_recovery(&mut state, &target.id, recovery, now).await;
                        }
                    }
                }

                // Reset recovery tracker on recovery
                if new_status == TargetStatus::Up && prev_status == TargetStatus::Down {
                    state.recovery_tracker.remove(&target.id);
                    info!("Target {} ({}) recovered to UP", target.id, target.name);
                }

                if new_status != prev_status {
                    info!(
                        "Target {} ({}) status: {prev_status} → {new_status}",
                        target.id, target.name
                    );
                }
            }
        }
    })
}

/// Compute the new status based on the state machine rules.
fn compute_status(
    prev: TargetStatus,
    result: &CheckResult,
    degraded_threshold_ms: u64,
    down_after: u32,
    up_after: u32,
    current_state: Option<&TargetState>,
) -> TargetStatus {
    let consecutive_fail = current_state.map_or(0, |s| s.consecutive_fail);
    let consecutive_ok = current_state.map_or(0, |s| s.consecutive_ok);

    match prev {
        TargetStatus::Unknown => {
            if result.ok {
                TargetStatus::Up
            } else {
                TargetStatus::Degraded
            }
        }
        TargetStatus::Up => {
            if !result.ok {
                return TargetStatus::Degraded;
            }
            if result
                .latency_ms
                .is_some_and(|ms| ms > degraded_threshold_ms)
            {
                TargetStatus::Degraded
            } else {
                TargetStatus::Up
            }
        }
        TargetStatus::Degraded => {
            if result.ok && consecutive_ok + 1 >= up_after {
                TargetStatus::Up
            } else if !result.ok && consecutive_fail + 1 >= down_after {
                TargetStatus::Down
            } else {
                TargetStatus::Degraded
            }
        }
        TargetStatus::Down => {
            if result.ok && consecutive_ok + 1 >= up_after {
                TargetStatus::Up
            } else {
                TargetStatus::Down
            }
        }
    }
}

/// Attempt to execute a recovery action (respecting cooldown and max retries).
async fn try_recovery(
    state: &mut InfraState,
    target_id: &str,
    recovery: &super::RecoveryConfig,
    now: u64,
) {
    let (last_exec, count) = state
        .recovery_tracker
        .get(target_id)
        .copied()
        .unwrap_or((0, 0));

    // Check max retries
    if count >= recovery.max_retries {
        debug!(
            "Recovery for {target_id}: exhausted ({count}/{} retries)",
            recovery.max_retries
        );
        return;
    }

    // Check cooldown
    if now.saturating_sub(last_exec) < recovery.cooldown_secs {
        debug!(
            "Recovery for {target_id}: cooling down ({} of {}s)",
            now - last_exec,
            recovery.cooldown_secs
        );
        return;
    }

    info!("Executing recovery for {target_id}: {}", recovery.command);

    // Run the recovery command (5-minute hard timeout)
    let result = super::checks::exec_simple_pub(&recovery.command, 300_000).await;

    let (exit_code, stdout) = match result {
        Ok((exit, out, _err)) => (exit, out),
        Err(e) => (-1, format!("ERROR: {e}")),
    };

    // Truncate stdout for the log
    let stdout_trunc = if stdout.len() > 512 {
        format!("{}...", &stdout[..512])
    } else {
        stdout
    };

    info!("Recovery for {target_id}: exit={exit_code}, output={stdout_trunc}");

    state.push_recovery_log(RecoveryLogEntry {
        ts: now_iso(),
        target_id: target_id.to_string(),
        command: recovery.command.clone(),
        exit_code,
        stdout: stdout_trunc,
    });

    state
        .recovery_tracker
        .insert(target_id.to_string(), (now, count + 1));
}
