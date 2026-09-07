//! Monitoring loop — runs as a long-lived tokio task.
//!
//! Spawned when config is first pushed (or loaded from disk at startup).
//! Aborted and re-spawned on config change.
//!
//! Due targets run CONCURRENTLY under a small semaphore, each with its own
//! deadline. The first cut ran them one after another inside the tick, so a
//! target that answers slowly (a router API check is several requests) held
//! every ping target behind it.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinSet;
use tracing::{debug, info, warn};

use super::checks::{self, CheckContext, CheckResult};
use super::{
    now_epoch, now_iso, CheckSpec, DataSample, InfraConfig, InfraState, InfraTarget,
    RecoveryLogEntry, TargetState, TargetStatus,
};

/// How many checks may run at once. Small: these devices have one core and
/// the checks fork processes.
const CHECK_CONCURRENCY: usize = 3;
/// Hard ceiling on one check, whatever its own timeouts say.
const CHECK_DEADLINE: Duration = Duration::from_secs(90);

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

        // Per-target tracking: last_check_epoch for interval gating
        let mut last_check: HashMap<String, u64> = HashMap::new();
        let limiter = Arc::new(Semaphore::new(CHECK_CONCURRENCY));
        let config = Arc::new(config);

        let tick_interval = Duration::from_secs(5); // check every 5s which targets are due
        let mut interval = tokio::time::interval(tick_interval);

        loop {
            interval.tick().await;
            let now = now_epoch();

            let mut set: JoinSet<()> = JoinSet::new();
            for target in &config.targets {
                let target_interval = target.interval_secs.unwrap_or(config.check_interval_secs);
                let last = last_check.get(&target.id).copied().unwrap_or(0);
                if now.saturating_sub(last) < target_interval {
                    continue; // not due yet
                }
                last_check.insert(target.id.clone(), now);

                let ctx = context_for(&infra_state, target).await;
                let target = target.clone();
                let state = infra_state.clone();
                let limiter = limiter.clone();
                let version = config.version;
                set.spawn(async move {
                    let Ok(_permit) = limiter.acquire().await else {
                        return;
                    };
                    debug!("Checking target {}: {}", target.id, target.name);
                    let result = match tokio::time::timeout(
                        CHECK_DEADLINE,
                        checks::run_check_with(&target.check, &ctx),
                    )
                    .await
                    {
                        Ok(r) => r,
                        Err(_) => CheckResult::failed(format!(
                            "CHECK DEADLINE: no answer within {}s",
                            CHECK_DEADLINE.as_secs()
                        )),
                    };
                    let mut guard = state.lock().await;
                    apply_result(&mut guard, &target, result, version, now_epoch()).await;
                });
            }
            // Wait for this tick's checks so a target is never in flight twice.
            while set.join_next().await.is_some() {}
        }
    })
}

/// What a target's check needs from shared state: the pin store location,
/// its credential, and the session cached from the last run.
async fn context_for(infra_state: &Arc<Mutex<InfraState>>, target: &InfraTarget) -> CheckContext {
    let CheckSpec::HttpApi { credential_id, .. } = &target.check else {
        return CheckContext::default();
    };
    let guard = infra_state.lock().await;
    CheckContext {
        data_dir: guard
            .config_path
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        credential: credential_id
            .as_ref()
            .and_then(|id| guard.credentials.get(id).cloned()),
        session: guard.sessions.get(&target.id).cloned(),
    }
}

/// Fold one check result into the target's state, run the state machine,
/// fire recovery, keep the session and the data history.
pub async fn apply_result(
    state: &mut InfraState,
    target: &InfraTarget,
    result: CheckResult,
    config_version: u32,
    now: u64,
) {
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

    // A failed API check keeps the last good snapshot visible, marked by the
    // status and counters, rather than blanking it: the consumer reads
    // `data.ts` for freshness and the status for health.
    let data = match (&result.data, current.and_then(|t| t.data.clone())) {
        (Some(d), _) => Some(d.clone()),
        (None, kept) if matches!(target.check, CheckSpec::HttpApi { .. }) => kept,
        _ => None,
    };

    let ts = now_iso();
    state.results.targets.insert(
        target.id.clone(),
        TargetState {
            status: new_status,
            latency_ms: result.latency_ms,
            since,
            consecutive_ok,
            consecutive_fail,
            last_check: ts.clone(),
            detail: result.detail.clone(),
            name: target.name.clone(),
            http_status: result.http_status,
            data,
        },
    );
    state.results.ts.clone_from(&ts);
    state.results.config_version = config_version;

    if let Some(session) = result.session {
        state.sessions.insert(target.id.clone(), session);
    } else if !result.ok {
        // Whatever the failure, do not trust the cached session past it.
        state.sessions.remove(&target.id);
    }
    if let Some(d) = result.data {
        state.push_data_sample(
            &target.id,
            DataSample {
                ts,
                status: new_status,
                latency_ms: result.latency_ms,
                data: d,
            },
        );
    }

    // Recovery action: fire on any DOWN state (new transition or sustained)
    if new_status == TargetStatus::Down {
        if let Some(ref recovery) = target.recovery {
            if recovery.enabled {
                try_recovery(state, &target.id, recovery, now).await;
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
        if !result.ok {
            warn!("Target {} ({}): {}", target.id, target.name, result.detail);
        }
    }
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

#[cfg(test)]
#[allow(
    clippy::unreadable_literal,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
mod tests {
    use super::*;
    use serde_json::json;

    fn target(check: CheckSpec) -> InfraTarget {
        InfraTarget {
            id: "t1".into(),
            name: "Peplink".into(),
            check,
            degraded_threshold_ms: 200,
            down_after_consecutive: 3,
            up_after_consecutive: 2,
            interval_secs: None,
            recovery: None,
        }
    }

    fn api_target() -> InfraTarget {
        target(CheckSpec::HttpApi {
            base_url: "https://192.168.50.1".into(),
            profile: super::super::ApiProfile::Peplink,
            pin_sha256: None,
            credential_id: Some("c1".into()),
            timeout_ms: None,
        })
    }

    #[tokio::test]
    async fn a_good_api_check_stores_data_session_and_history() {
        let mut state = InfraState::new("/tmp/sctl-infra-test");
        let t = api_target();
        let result = CheckResult {
            ok: true,
            latency_ms: Some(40),
            detail: "API OK".into(),
            data: Some(json!({"ap_wan_up": 1})),
            session: Some("bauth=abc".into()),
            ..CheckResult::default()
        };
        apply_result(&mut state, &t, result, 7, 1_000).await;
        let ts = state.results.targets.get("t1").unwrap();
        assert_eq!(ts.status, TargetStatus::Up);
        assert_eq!(ts.data.as_ref().unwrap()["ap_wan_up"], 1);
        assert_eq!(
            state.sessions.get("t1").map(String::as_str),
            Some("bauth=abc")
        );
        assert_eq!(state.data_history.get("t1").unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_failed_api_check_keeps_the_last_snapshot_and_drops_the_session() {
        let mut state = InfraState::new("/tmp/sctl-infra-test");
        let t = api_target();
        let good = CheckResult {
            ok: true,
            latency_ms: Some(40),
            detail: "API OK".into(),
            data: Some(json!({"ap_wan_up": 1})),
            session: Some("bauth=abc".into()),
            ..CheckResult::default()
        };
        apply_result(&mut state, &t, good, 7, 1_000).await;
        apply_result(&mut state, &t, CheckResult::failed("API TIMEOUT"), 7, 1_060).await;
        let ts = state.results.targets.get("t1").unwrap();
        assert_eq!(ts.status, TargetStatus::Degraded);
        assert_eq!(ts.detail, "API TIMEOUT");
        assert_eq!(
            ts.data.as_ref().unwrap()["ap_wan_up"],
            1,
            "the last reading stays visible; status says it is old"
        );
        assert!(
            !state.sessions.contains_key("t1"),
            "a failure invalidates the cached login"
        );
        assert_eq!(
            state.data_history.get("t1").unwrap().len(),
            1,
            "no sample for a failure"
        );
    }

    #[tokio::test]
    async fn the_history_ring_is_bounded() {
        let mut state = InfraState::new("/tmp/sctl-infra-test");
        let t = api_target();
        for i in 0..(super::super::MAX_DATA_HISTORY as i64 + 5) {
            let r = CheckResult {
                ok: true,
                latency_ms: Some(1),
                detail: "ok".into(),
                data: Some(json!({"i": i})),
                ..CheckResult::default()
            };
            apply_result(&mut state, &t, r, 1, 1_000 + i as u64).await;
        }
        let ring = state.data_history.get("t1").unwrap();
        assert_eq!(ring.len(), super::super::MAX_DATA_HISTORY);
        assert_eq!(
            ring.back().unwrap().data["i"],
            super::super::MAX_DATA_HISTORY as i64 + 4
        );
    }

    #[test]
    fn plain_kinds_never_carry_data() {
        let t = target(CheckSpec::Ping {
            host: "10.0.0.1".into(),
            timeout_ms: None,
        });
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut state = InfraState::new("/tmp/sctl-infra-test");
            let r = CheckResult {
                ok: true,
                latency_ms: Some(3),
                detail: "PING OK".into(),
                ..CheckResult::default()
            };
            apply_result(&mut state, &t, r, 1, 1_000).await;
            assert!(state.results.targets.get("t1").unwrap().data.is_none());
        });
    }
}
