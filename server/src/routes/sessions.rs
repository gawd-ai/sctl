//! REST endpoints for session management.
//!
//! - `GET    /api/sessions`            — list all sessions
//! - `POST   /api/sessions/{id}/signal` — send POSIX signal
//! - `DELETE  /api/sessions/{id}`       — kill session
//! - `PATCH   /api/sessions/{id}`       — rename, set AI permission/status

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::activity::{self, request_id_from_headers, ActivityType};
use crate::AppState;

/// `GET /api/sessions` — list all active sessions (same shape as WS `session.listed`).
pub async fn list_sessions(State(state): State<AppState>) -> Json<Value> {
    let items = state.session_manager.list_sessions().await;
    let sessions_json: Vec<Value> = items
        .iter()
        .map(|s| {
            let mut obj = json!({
                "session_id": s.session_id,
                "pid": s.pid,
                "persistent": s.persistent,
                "pty": s.pty,
                "attached": s.attached,
                "status": s.status,
                "idle": s.idle,
                "idle_timeout": s.idle_timeout,
                "created_at": s.created_at,
                "user_allows_ai": s.user_allows_ai,
                "ai_is_working": s.ai_is_working,
            });
            if let Some(exit_code) = s.exit_code {
                obj["exit_code"] = json!(exit_code);
            }
            if let Some(ref name) = s.name {
                obj["name"] = json!(name);
            }
            if let Some(ref activity) = s.ai_activity {
                obj["ai_activity"] = json!(activity);
            }
            if let Some(ref msg) = s.ai_status_message {
                obj["ai_status_message"] = json!(msg);
            }
            obj
        })
        .collect();

    Json(json!({
        "sessions": sessions_json,
    }))
}

// ─── Signal ──────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct SignalRequest {
    pub signal: i32,
}

/// `POST /api/sessions/{id}/signal` — send a POSIX signal to a session.
pub async fn signal_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<SignalRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let source = activity::source_from_headers(&headers);
    let req_id = request_id_from_headers(&headers);

    state
        .session_manager
        .signal_session(&id, payload.signal)
        .await
        .map_err(|e| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error": e, "code": "SESSION_NOT_FOUND"})),
            )
        })?;

    state
        .activity_log
        .log(
            ActivityType::SessionSignal,
            source,
            format!("signal {} → {}", payload.signal, &id[..8.min(id.len())]),
            Some(json!({ "session_id": id, "signal": payload.signal })),
            req_id,
        )
        .await;

    Ok(Json(json!({
        "ok": true,
        "session_id": id,
        "signal": payload.signal,
    })))
}

// ─── Kill ────────────────────────────────────────────────────────────────────

/// `DELETE /api/sessions/{id}` — kill a session and remove it.
pub async fn kill_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let source = activity::source_from_headers(&headers);
    let req_id = request_id_from_headers(&headers);

    let found = state.session_manager.kill_session(&id).await;
    if !found {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("Session {id} not found"), "code": "SESSION_NOT_FOUND"})),
        ));
    }

    let _ = state.session_events.send(json!({
        "type": "session.destroyed",
        "session_id": id,
        "reason": "killed",
    }));

    state
        .activity_log
        .log(
            ActivityType::SessionKill,
            source,
            format!("session {}", &id[..8.min(id.len())]),
            Some(json!({ "session_id": id })),
            req_id,
        )
        .await;

    Ok(Json(json!({
        "ok": true,
        "session_id": id,
    })))
}

// ─── Patch (rename, AI permission, AI status) ────────────────────────────────

#[derive(Deserialize)]
pub struct SessionPatch {
    pub name: Option<String>,
    pub allowed: Option<bool>,
    pub working: Option<bool>,
    pub activity: Option<String>,
    pub message: Option<String>,
}

/// `PATCH /api/sessions/{id}` — combined update: rename, AI permission, AI status.
pub async fn patch_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(patch): Json<SessionPatch>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    // Rename
    if let Some(ref name) = patch.name {
        state
            .session_manager
            .rename_session(&id, name)
            .await
            .map_err(|e| {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": e, "code": "SESSION_NOT_FOUND"})),
                )
            })?;
        let _ = state.session_events.send(json!({
            "type": "session.renamed",
            "session_id": id,
            "name": name,
        }));
    }

    // AI permission
    if let Some(allowed) = patch.allowed {
        let ai_cleared = state
            .session_manager
            .set_user_allows_ai(&id, allowed)
            .await
            .map_err(|e| {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": e, "code": "SESSION_NOT_FOUND"})),
                )
            })?;
        let _ = state.session_events.send(json!({
            "type": "session.ai_permission_changed",
            "session_id": id,
            "allowed": allowed,
        }));
        if ai_cleared {
            let _ = state.session_events.send(json!({
                "type": "session.ai_status_changed",
                "session_id": id,
                "working": false,
            }));
        }
    }

    // AI status
    if let Some(working) = patch.working {
        state
            .session_manager
            .set_ai_status(
                &id,
                working,
                patch.activity.as_deref(),
                patch.message.as_deref(),
            )
            .await
            .map_err(|e| {
                (
                    StatusCode::CONFLICT,
                    Json(json!({"error": e, "code": "AI_NOT_ALLOWED"})),
                )
            })?;
        let mut broadcast = json!({
            "type": "session.ai_status_changed",
            "session_id": id,
            "working": working,
        });
        if let Some(ref a) = patch.activity {
            broadcast["activity"] = json!(a);
        }
        if let Some(ref m) = patch.message {
            broadcast["message"] = json!(m);
        }
        let _ = state.session_events.send(broadcast);
    }

    Ok(Json(json!({
        "ok": true,
        "session_id": id,
    })))
}
