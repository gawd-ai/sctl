//! LTE signal and modem info endpoint.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};

use crate::AppState;

/// `GET /api/lte` — returns current LTE signal quality and modem identity.
///
/// Returns 404 if LTE monitoring is not configured on this device.
pub async fn lte(State(state): State<AppState>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(lte_state) = &state.lte_state else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "LTE not configured on this device"})),
        ));
    };

    let ls = lte_state.lock().await;

    let signal = ls.signal.as_ref().map(|sig| {
        json!({
            "rssi_dbm": sig.rssi_dbm,
            "rsrp": sig.rsrp,
            "rsrq": sig.rsrq,
            "sinr": sig.sinr,
            "band": sig.band,
            "operator": sig.operator,
            "technology": sig.technology,
            "cell_id": sig.cell_id,
            "signal_bars": sig.signal_bars,
            "recorded_at": sig.recorded_at,
        })
    });

    let modem = ls.modem.as_ref().map(|m| {
        json!({
            "model": m.model,
            "firmware": m.firmware,
            "imei": m.imei,
            "iccid": m.iccid,
        })
    });

    Ok(Json(json!({
        "signal": signal,
        "modem": modem,
        "errors_total": ls.errors_total,
        "last_error": ls.last_error,
    })))
}
