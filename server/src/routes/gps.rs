//! GPS location endpoint.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde_json::Value;

use crate::error::{codes, ApiError};
use crate::AppState;

/// `GET /api/gps` — returns current GPS status, last fix, and history.
///
/// Returns 404 if GPS is not configured on this device.
pub async fn gps(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    if state.config.gps.is_none() {
        return Err(
            ApiError::new(codes::NOT_FOUND, "GPS not configured on this device")
                .into_response_with(StatusCode::NOT_FOUND),
        );
    }

    let Some(comms_state) = &state.comms_state else {
        return Err(
            ApiError::new(codes::MODEM_UNAVAILABLE, "comms provider not available")
                .into_response_with(StatusCode::SERVICE_UNAVAILABLE),
        );
    };

    let snapshot = comms_state
        .lock()
        .await
        .gps
        .clone()
        .unwrap_or_else(crate::comms::starting_gps_response);

    Ok(Json(snapshot))
}
