//! HTTP route handlers for the infra monitoring API.
//!
//! - `POST /api/infra/config` — push monitoring config, start/restart monitor
//! - `GET  /api/infra/results` — latest monitoring results
//! - `POST /api/infra/check/{target_id}` — on-demand check for one target
//! - `DELETE /api/infra/config` — stop monitoring, remove config
//! - `POST /api/infra/discover` — trigger LAN discovery scan

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde_json::{json, Value};
use tracing::info;

use super::monitor;
use std::collections::HashMap;

use super::{InfraConfig, InfraResults};
use crate::AppState;

/// `POST /api/infra/config` — receive and apply monitoring config.
///
/// Persists config to disk, aborts any running monitor, and spawns a new
/// monitoring loop with the updated config.
pub async fn push_config(
    State(state): State<AppState>,
    Json(config): Json<InfraConfig>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(ref infra) = state.infra_state else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Infra monitoring not available"})),
        ));
    };

    let version = config.version;
    let target_count = config.targets.len();

    let mut guard = infra.lock().await;

    // Abort existing monitor if running
    if let Some(handle) = guard.monitor_handle.take() {
        handle.abort();
    }

    // Store and persist config
    guard.config = Some(config.clone());
    guard.save_config();

    // Spawn new monitor
    let handle = monitor::spawn_monitor(infra.clone(), config);
    guard.monitor_handle = Some(handle);

    info!("Infra config v{version} applied: {target_count} targets");

    Ok(Json(json!({
        "status": "ok",
        "config_version": version,
        "target_count": target_count
    })))
}

/// `GET /api/infra/results` — return latest monitoring results.
pub async fn get_results(
    State(state): State<AppState>,
) -> Result<Json<InfraResults>, (StatusCode, Json<Value>)> {
    let Some(ref infra) = state.infra_state else {
        return Ok(Json(InfraResults {
            ts: super::now_iso(),
            config_version: 0,
            targets: HashMap::default(),
            recovery_log: Vec::new(),
        }));
    };

    let guard = infra.lock().await;
    Ok(Json(guard.results.clone()))
}

/// `POST /api/infra/check/{target_id}` — run an immediate on-demand check.
pub async fn check_target(
    State(state): State<AppState>,
    Path(target_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(ref infra) = state.infra_state else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Infra monitoring not available"})),
        ));
    };

    let guard = infra.lock().await;
    let Some(ref config) = guard.config else {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "No config loaded"})),
        ));
    };

    let target = config.targets.iter().find(|t| t.id == target_id);
    let Some(target) = target else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("Target {target_id} not found")})),
        ));
    };

    let check_spec = target.check.clone();
    drop(guard); // release lock during check

    let result = super::checks::run_check(&check_spec).await;

    Ok(Json(json!({
        "target_id": target_id,
        "ok": result.ok,
        "latency_ms": result.latency_ms,
        "detail": result.detail,
        "http_status": result.http_status,
    })))
}

/// `GET /api/infra/discover/progress` — return current discovery scan progress.
pub async fn discover_progress(State(state): State<AppState>) -> Json<Value> {
    let Some(ref infra) = state.infra_state else {
        return Json(json!({"active": false, "phase": "idle"}));
    };
    let guard = infra.lock().await;
    Json(
        serde_json::to_value(&guard.discovery_progress)
            .unwrap_or(json!({"active": false, "phase": "idle"})),
    )
}

/// `GET /api/infra/discover/subnets` — return auto-detected LAN subnets.
pub async fn discover_subnets() -> Json<Value> {
    let subnets = super::discovery::auto_detect_subnets().await;
    Json(json!({ "subnets": subnets }))
}

/// `DELETE /api/infra/config` — stop monitoring and remove config.
pub async fn delete_config(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(ref infra) = state.infra_state else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Infra monitoring not available"})),
        ));
    };

    let mut guard = infra.lock().await;

    // Abort monitor
    if let Some(handle) = guard.monitor_handle.take() {
        handle.abort();
    }

    // Clear config and results
    guard.config = None;
    guard.results.targets.clear();
    guard.results.config_version = 0;
    guard.recovery_tracker.clear();

    // Remove persisted config
    let _ = std::fs::remove_file(&guard.config_path);

    info!("Infra monitoring stopped and config removed");

    Ok(Json(
        json!({"status": "ok", "message": "Monitoring stopped"}),
    ))
}
