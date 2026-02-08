//! Unauthenticated health-check endpoint.

use axum::{extract::State, Json};
use serde_json::{json, Value};

use crate::AppState;

/// `GET /api/health` — liveness probe.
///
/// Returns `{"status":"ok","uptime_secs":N,"version":"..."}`. No authentication
/// required, suitable for load-balancer health checks.
pub async fn health(State(state): State<AppState>) -> Json<Value> {
    let uptime = state.start_time.elapsed().as_secs();
    Json(json!({
        "status": "ok",
        "uptime_secs": uptime,
        "version": env!("CARGO_PKG_VERSION"),
    }))
}
