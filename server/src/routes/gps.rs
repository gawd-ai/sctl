//! GPS location endpoint.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};

use crate::AppState;

/// `GET /api/gps` — returns current GPS status, last fix, and history.
///
/// Returns 404 if GPS is not configured on this device.
pub async fn gps(State(state): State<AppState>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(gps_state) = &state.gps_state else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "GPS not configured on this device"})),
        ));
    };

    let gs = gps_state.lock().await;

    let last_fix = gs.last_fix.as_ref().map(|f| {
        json!({
            "latitude": f.latitude,
            "longitude": f.longitude,
            "altitude": f.altitude,
            "speed_kmh": f.speed_kmh,
            "course": f.course,
            "hdop": f.hdop,
            "satellites": f.satellites,
            "utc": f.utc,
            "date": f.date,
            "fix_type": f.fix_type,
            "recorded_at": f.recorded_at,
        })
    });

    let fix_age_secs = gs.last_fix_at.map(|t| t.elapsed().as_secs());

    let history: Vec<Value> = gs
        .history
        .iter()
        .rev()
        .take(50)
        .map(|f| {
            json!({
                "latitude": f.latitude,
                "longitude": f.longitude,
                "altitude": f.altitude,
                "speed_kmh": f.speed_kmh,
                "satellites": f.satellites,
                "recorded_at": f.recorded_at,
            })
        })
        .collect();

    Ok(Json(json!({
        "status": gs.status,
        "last_fix": last_fix,
        "fix_age_secs": fix_age_secs,
        "history": history,
        "fixes_total": gs.fixes_total,
        "errors_total": gs.errors_total,
        "last_error": gs.last_error,
    })))
}
