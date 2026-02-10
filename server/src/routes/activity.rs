//! Activity journal endpoint.
//!
//! `GET /api/activity?since_id=N&limit=N` — returns recent activity entries.

use axum::extract::{Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::AppState;

/// Query parameters for `GET /api/activity`.
#[derive(Deserialize)]
pub struct ActivityQuery {
    /// Return entries with `id > since_id`. Defaults to 0 (all entries).
    #[serde(default)]
    pub since_id: u64,
    /// Maximum number of entries to return. Defaults to 50, max 200.
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    50
}

/// `GET /api/activity` — read recent activity entries.
pub async fn get_activity(
    State(state): State<AppState>,
    Query(query): Query<ActivityQuery>,
) -> Json<Value> {
    let limit = query.limit.min(200);
    let entries = state.activity_log.read_since(query.since_id, limit).await;
    Json(json!({ "entries": entries }))
}
