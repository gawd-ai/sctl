//! Unauthenticated health-check endpoint.

use std::sync::atomic::Ordering;

use axum::{extract::State, Json};
use serde_json::{json, Value};

use crate::AppState;

/// `GET /api/health` — liveness probe.
///
/// Returns status, uptime, version, session count, and tunnel status. No
/// authentication required, suitable for load-balancer health checks.
pub async fn health(State(state): State<AppState>) -> Json<Value> {
    let uptime = state.start_time.elapsed().as_secs();
    let sessions = state.session_manager.session_count().await;
    let tunnel_connected = state.tunnel_connected.load(Ordering::Relaxed);
    let tunnel_reconnects = state.tunnel_reconnects.load(Ordering::Relaxed);
    Json(json!({
        "status": "ok",
        "uptime_secs": uptime,
        "version": env!("CARGO_PKG_VERSION"),
        "sessions": sessions,
        "tunnel_connected": tunnel_connected,
        "tunnel_reconnects": tunnel_reconnects,
    }))
}
