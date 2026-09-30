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
        return Err(
            ApiError::new("UPGRADE_IN_FLIGHT", "an upgrade is being staged")
                .with_detail(json!({"state": state.upgrade.current()}))
                .into_response_with(StatusCode::CONFLICT),
        );
    };
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
