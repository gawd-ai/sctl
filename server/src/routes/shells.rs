//! Shell discovery endpoint.

use axum::{extract::State, Json};
use serde_json::{json, Value};

use crate::AppState;

/// `GET /api/shells` — list available shells on this device.
pub async fn list_shells(State(state): State<AppState>) -> Json<Value> {
    let shells = crate::shell::detect_shells();
    Json(json!({
        "shells": shells,
        "default_shell": &state.config.shell.default_shell,
    }))
}
