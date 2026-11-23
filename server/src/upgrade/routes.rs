//! `POST /api/upgrade`, `GET /api/upgrade`, `DELETE /api/upgrade`.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};

use super::stage::{self, Context, Request};
use super::state::{Phase, UpgradeState};
use super::Reason;
use crate::error::ApiError;
use crate::AppState;

/// What `GET /api/upgrade` and `/api/health`'s `upgrade` carry.
pub fn report(state: &AppState) -> Value {
    let upgrade = state.upgrade.current();
    let install = state
        .install
        .as_ref()
        .map(|i| i.report(&state.config.server.listen));
    json!({
        "state": upgrade,
        // The agent's own restart (ADR-014). Its presence is how a caller
        // knows the agent has `POST /api/restart`.
        "restart": state.restart.current(),
        "running_version": crate::VERSION,
        "target": super::TARGET,
        "layout": state.install.as_ref().map(|i| i.layout.as_str()),
        "install": install,
        "install_error": state.install_error,
        "hold": state.config.upgrade.hold
            || super::install::hold_file_reason(std::path::Path::new(super::install::HOLD_PATH)).is_some(),
    })
}

/// `GET /api/upgrade`.
pub async fn get_upgrade(State(state): State<AppState>) -> Json<Value> {
    Json(report(&state))
}

/// `DELETE /api/upgrade`: clear a finished state.
pub async fn delete_upgrade(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let current = state.upgrade.current();
    if current.in_flight() {
        return Err(
            ApiError::new("UPGRADE_IN_FLIGHT", "an upgrade is in progress")
                .with_detail(json!({"state": current}))
                .into_response_with(StatusCode::CONFLICT),
        );
    }
    state.upgrade.set(UpgradeState::idle());
    Ok(Json(report(&state)))
}

/// The staging context from the app's state.
pub fn context(state: &AppState) -> Result<Context, String> {
    let trust_keys = super::keys::trusted(&state.config.upgrade.trust_keys)?;
    Ok(Context {
        state_dir: state.config.server.state_dir().to_string(),
        listen: state.config.server.listen.clone(),
        install: state.install.as_ref().map(|i| (**i).clone()),
        tunnel: state.config.tunnel.clone(),
        trust_keys,
        config_path: state.config_path.clone(),
        hold: state.config.upgrade.hold,
        hold_file: std::path::PathBuf::from(super::install::HOLD_PATH),
        running_version: crate::VERSION.to_string(),
        recovery_runs: Some(state.infra_digest.runs()),
    })
}

/// `POST /api/upgrade`.
pub async fn post_upgrade(
    State(state): State<AppState>,
    Json(req): Json<Request>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<ApiError>)> {
    let refused = |status: StatusCode, code: &str, refusal: &super::Refusal| {
        ApiError::new(code, refusal.to_string())
            .with_detail(json!({"reason": refusal.reason, "detail": refusal.detail}))
            .into_response_with(status)
    };
    let ctx = context(&state).map_err(|e| {
        ApiError::new("UPGRADE_CONFIG", e).into_response_with(StatusCode::INTERNAL_SERVER_ERROR)
    })?;
    if let Ok(target) = super::version::Version::parse(&req.version).ok_or(()) {
        if super::version::Version::parse(crate::VERSION).as_ref() == Some(&target) {
            return Ok((
                StatusCode::OK,
                Json(
                    json!({"accepted": false, "state": state.upgrade.current(), "running_version": crate::VERSION}),
                ),
            ));
        }
    }
    let Ok(_guard) = state.upgrade.in_flight.try_lock() else {
        // A restart on request holds the same lock (ADR-014): one in flight
        // at a time, across both.
        if state.restart.current().in_flight() {
            return Err(super::restart_route::in_flight(&state));
        }
        return Err(
            ApiError::new("UPGRADE_IN_FLIGHT", "an upgrade is being staged")
                .with_detail(json!({"state": state.upgrade.current()}))
                .into_response_with(StatusCode::CONFLICT),
        );
    };
    if super::restart_route::restart_in_flight(&state) {
        return Err(super::restart_route::in_flight(&state));
    }
    let current = state.upgrade.current();
    if current.in_flight() {
        // The helper may still be at it; when its lock is free it is gone
        // and the state is stale: clear it as helper_lost and go on.
        let stage_dir = stage::stage_dir(&ctx, current.to_version.as_deref().unwrap_or(""));
        if current.phase == Phase::Applying && super::apply::lock_is_free(&stage_dir) {
            state.upgrade.set(
                current
                    .clone()
                    .not_applied(Reason::HelperLost, "the helper never wrote an outcome"),
            );
        } else {
            return Err(
                ApiError::new("UPGRADE_IN_FLIGHT", "an upgrade is in progress")
                    .with_detail(json!({"state": current}))
                    .into_response_with(StatusCode::CONFLICT),
            );
        }
    }
    if let Err(refusal) = stage::refuse_early(&ctx, &req) {
        let (status, code) = match refusal.reason {
            Reason::Held => (StatusCode::LOCKED, "UPGRADE_HELD"),
            _ => (StatusCode::UNPROCESSABLE_ENTITY, "UPGRADE_REFUSED"),
        };
        return Err(refused(status, code, &refusal));
    }
    let staging = UpgradeState::staging(req.request_id.clone(), crate::VERSION, &req.version);
    state.upgrade.set(staging.clone());
    let handle: Arc<super::state::Handle> = state.upgrade.clone();
    let in_flight = state.upgrade.clone();
    tokio::spawn(async move {
        // Hold the staging lock for the task's life: the route's guard
        // dropped when it answered.
        let _guard = in_flight.in_flight.lock().await;
        stage::run(ctx, req, handle).await;
    });
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"accepted": true, "state": staging})),
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::time::Instant;

    use super::*;
    use crate::config::Config;
    use crate::file_access::Gate;
    use crate::infra::checks::CheckResult;
    use crate::infra::monitor::{apply_result, Reading};
    use crate::infra::{CheckSpec, Digest, InfraConfig, InfraState, InfraTarget, RecoveryConfig};

    fn state_in(tag: &str) -> (std::path::PathBuf, AppState) {
        let dir =
            std::env::temp_dir().join(format!("sctl-upgrade-routes-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut config: Config = toml::from_str("").unwrap();
        config.server.data_dir = dir.join("data").display().to_string();
        let gate = Arc::new(Gate::for_agent(&config, None, None));
        (dir, AppState::for_tests(config, gate))
    }

    /// `GET /api/upgrade` carries the restart's record on an agent never
    /// asked: its presence is how a caller knows the route is there
    /// (ADR-014 decision 7).
    #[test]
    fn the_report_carries_the_restart_idle_on_an_agent_never_asked() {
        let (dir, state) = state_in("report");
        let report = report(&state);
        assert_eq!(report["restart"], json!({"v": 1, "phase": "idle"}));
        assert_eq!(report["state"]["phase"], "idle");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A managed upgrade waits for the recovery runs of the agent's own
    /// Infra state (ADR-010 decision 9): the context `POST /api/upgrade`
    /// builds carries the runs of the digest that state starts its runs
    /// on, the one `main` gives both. With no runs, or another set, the
    /// hand-off would not wait, and its restart could cut a power cycle
    /// between off and on.
    #[tokio::test]
    async fn the_context_carries_the_recovery_runs_of_the_agents_infra_state() {
        let dir = std::env::temp_dir().join(format!("sctl-upgrade-context-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut config: Config = toml::from_str("").unwrap();
        config.server.data_dir = dir.join("data").display().to_string();
        let gate = Arc::new(Gate::for_agent(&config, None, None));
        let mut state = AppState::for_tests(config, gate);
        let digest = Arc::new(Digest::new(1_790_000_000_000, false));
        let mut infra =
            InfraState::with_digest(&dir.join("state").display().to_string(), digest.clone());
        state.infra_digest = digest;

        let runs = context(&state)
            .expect("a context")
            .recovery_runs
            .expect("the managed path waits for the runs");
        assert_eq!(runs.in_progress(), 0);

        // A failed check of the monitor takes the outlet Down and starts
        // its recovery run. Nothing runs it here.
        let outlet = InfraTarget {
            id: "pdu".into(),
            name: "Outlet".into(),
            check: CheckSpec::Ping {
                host: "192.0.2.1".into(),
                timeout_ms: None,
            },
            degraded_threshold_ms: 200,
            down_after_consecutive: 1,
            up_after_consecutive: 2,
            interval_secs: None,
            recovery: Some(RecoveryConfig {
                enabled: true,
                command: "true".into(),
                cooldown_secs: 300,
                max_retries: 2,
                credential_ids: Vec::new(),
            }),
        };
        assert!(infra.hold_config(InfraConfig {
            version: 7,
            check_interval_secs: 60,
            targets: vec![outlet.clone()],
        }));
        let run = (0..3)
            .find_map(|_| {
                apply_result(
                    &mut infra,
                    &outlet,
                    CheckResult::failed("NO ANSWER"),
                    7,
                    Instant::now(),
                    Reading::Monitor,
                )
            })
            .expect("Down: a run");
        assert_eq!(runs.in_progress(), 1, "the run the Infra state started");
        drop(run);
        assert_eq!(runs.in_progress(), 0, "and only while it is in progress");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
