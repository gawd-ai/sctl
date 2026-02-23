//! Activity journal endpoint.
//!
//! `GET /api/activity?since_id=N&limit=N&activity_type=exec&source=mcp&session_id=abc`
//! — returns recent activity entries with optional filtering.

use axum::extract::{Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::activity::{ActivitySource, ActivityType};
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
    /// Filter by activity type (e.g. `exec`, `file_read`, `session_start`).
    pub activity_type: Option<String>,
    /// Filter by source (e.g. `mcp`, `ws`, `rest`).
    pub source: Option<String>,
    /// Filter by session ID (matches `detail.session_id`).
    pub session_id: Option<String>,
}

fn default_limit() -> usize {
    50
}

/// `GET /api/activity` — read recent activity entries with optional filters.
pub async fn get_activity(
    State(state): State<AppState>,
    Query(query): Query<ActivityQuery>,
) -> Json<Value> {
    let limit = query.limit.min(200);
    let activity_type = query
        .activity_type
        .as_deref()
        .and_then(ActivityType::from_str_opt);
    let source = query
        .source
        .as_deref()
        .and_then(ActivitySource::from_str_opt);

    let entries = state
        .activity_log
        .read_since_filtered(
            query.since_id,
            limit,
            activity_type,
            source,
            query.session_id.as_deref(),
        )
        .await;
    Json(json!({ "entries": entries }))
}
