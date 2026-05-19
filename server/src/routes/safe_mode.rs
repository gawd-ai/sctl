//! Safe-mode flag endpoints.
//!
//! The supervisor writes `<data_dir>/safe_mode.flag` when it detects a
//! crash-loop. While the flag exists, the next `sctl serve` startup skips
//! every optional subsystem (modem, GPS, LTE, watchdog, infra) and keeps only
//! HTTP+tunnel+sessions live. Once an operator has investigated and cleared
//! the underlying issue, they clear the flag via:
//!
//! ```text
//! DELETE /api/safe_mode/flag    # auth required
//! ```
//!
//! The endpoint is idempotent — deleting a non-existent flag returns OK. The
//! flag content (`since_unix`, `reason`, `consecutive_crashes`) is returned
//! by `GET /api/safe_mode/flag` for inspection.

use std::path::Path;

use axum::{extract::State, http::StatusCode, Json};
use serde_json::{json, Value};

use crate::AppState;

fn flag_path(state: &AppState) -> std::path::PathBuf {
    Path::new(&state.config.server.data_dir).join("safe_mode.flag")
}

/// `GET /api/safe_mode/flag` — return flag contents if present.
pub async fn get_flag(State(state): State<AppState>) -> Result<Json<Value>, StatusCode> {
    let path = flag_path(&state);
    if !path.exists() {
        return Ok(Json(json!({ "active": false })));
    }
    let raw = std::fs::read_to_string(&path).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let body: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    Ok(Json(json!({
        "active": true,
        "flag": body,
    })))
}

/// `DELETE /api/safe_mode/flag` — clear the flag.
///
/// The next supervisor restart will then bring optional subsystems back up.
/// This does NOT restart the daemon — the operator must do that explicitly
/// (e.g. via `systemctl restart sctl`) or rely on the supervisor's normal
/// backoff to recycle the child.
pub async fn clear_flag(State(state): State<AppState>) -> Result<Json<Value>, StatusCode> {
    let path = flag_path(&state);
    if path.exists() {
        std::fs::remove_file(&path).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        Ok(Json(json!({ "cleared": true })))
    } else {
        Ok(Json(
            json!({ "cleared": false, "reason": "flag not present" }),
        ))
    }
}
