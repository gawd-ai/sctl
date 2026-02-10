//! Command execution endpoints.
//!
//! - `POST /api/exec` — execute a single command
//! - `POST /api/exec/batch` — execute multiple commands sequentially
//!
//! Both endpoints support per-request overrides for `shell`, `working_dir`, and
//! `env` (environment variables merged into the inherited environment).

use std::collections::HashMap;

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::activity::{self, ActivityType};
use crate::shell::process;
use crate::AppState;

/// Request body for `POST /api/exec`.
///
/// Only `command` is required — all other fields fall back to server config
/// defaults when omitted.
#[derive(Deserialize)]
pub struct ExecRequest {
    /// Shell command string (passed to `<shell> -c`).
    pub command: String,
    /// Per-request timeout in milliseconds. Defaults to `server.exec_timeout_ms`.
    pub timeout_ms: Option<u64>,
    /// Opaque correlation ID echoed back in the response.
    pub request_id: Option<String>,
    /// Override the working directory for this command.
    pub working_dir: Option<String>,
    /// Extra environment variables **merged into** the inherited environment.
    pub env: Option<HashMap<String, String>>,
    /// Override the shell binary (e.g. `/bin/bash`).
    pub shell: Option<String>,
}

/// Response body for `POST /api/exec` (and each item in a batch response).
#[derive(Serialize)]
pub struct ExecResponse {
    /// Process exit code (`-1` if unavailable).
    pub exit_code: i32,
    /// Captured stdout (capped at 1 MB).
    pub stdout: String,
    /// Captured stderr (capped at 1 MB).
    pub stderr: String,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
    /// Echoed from request, omitted if not provided.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

/// `POST /api/exec` — execute a single shell command.
///
/// # Errors
///
/// - `504 Gateway Timeout` with `{"code":"TIMEOUT"}` — command exceeded its timeout
/// - `500 Internal Server Error` with `{"code":"EXEC_FAILED"}` — spawn or wait failure
pub async fn exec(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ExecRequest>,
) -> Result<Json<ExecResponse>, (StatusCode, Json<Value>)> {
    let source = activity::source_from_headers(&headers);
    let timeout = payload
        .timeout_ms
        .unwrap_or(state.config.server.exec_timeout_ms);
    let shell = payload
        .shell
        .as_deref()
        .unwrap_or(&state.config.shell.default_shell);
    let raw_dir = payload
        .working_dir
        .as_deref()
        .unwrap_or(&state.config.shell.default_working_dir);
    let expanded_dir = crate::util::expand_tilde(raw_dir);
    let working_dir = expanded_dir.as_ref();

    match Box::pin(process::exec_command(
        shell,
        working_dir,
        &payload.command,
        timeout,
        payload.env.as_ref(),
    ))
    .await
    {
        Ok(result) => {
            state
                .activity_log
                .log(
                    ActivityType::Exec,
                    source,
                    activity::truncate_str(&payload.command, 80),
                    Some(json!({
                        "exit_code": result.exit_code,
                        "duration_ms": result.duration_ms,
                        "stdout_preview": activity::truncate_str(&result.stdout, 200),
                        "stderr_preview": activity::truncate_str(&result.stderr, 200),
                    })),
                )
                .await;
            Ok(Json(ExecResponse {
                exit_code: result.exit_code,
                stdout: result.stdout,
                stderr: result.stderr,
                duration_ms: result.duration_ms,
                request_id: payload.request_id,
            }))
        }
        Err(process::ExecError::Timeout) => {
            let mut err = json!({"error": "Command timed out", "code": "TIMEOUT"});
            if let Some(ref rid) = payload.request_id {
                err["request_id"] = json!(rid);
            }
            Err((StatusCode::GATEWAY_TIMEOUT, Json(err)))
        }
        Err(e) => {
            let mut err = json!({"error": e.to_string(), "code": "EXEC_FAILED"});
            if let Some(ref rid) = payload.request_id {
                err["request_id"] = json!(rid);
            }
            Err((StatusCode::INTERNAL_SERVER_ERROR, Json(err)))
        }
    }
}

// ---------------------------------------------------------------------------
// Batch exec
// ---------------------------------------------------------------------------

/// Request body for `POST /api/exec/batch`.
///
/// Top-level `shell`, `working_dir`, and `env` serve as defaults for all
/// commands. Per-command fields override them (env is merged, command wins).
#[derive(Deserialize)]
pub struct BatchExecRequest {
    /// One or more commands to execute sequentially.
    pub commands: Vec<BatchCommand>,
    /// Default working directory for all commands.
    pub working_dir: Option<String>,
    /// Default environment variables for all commands.
    pub env: Option<HashMap<String, String>>,
    /// Default shell for all commands.
    pub shell: Option<String>,
    /// Correlation ID echoed in the batch response.
    pub request_id: Option<String>,
}

/// A single command within a [`BatchExecRequest`].
#[derive(Deserialize)]
pub struct BatchCommand {
    /// Shell command string.
    pub command: String,
    /// Per-command timeout override.
    pub timeout_ms: Option<u64>,
    /// Per-command working directory override.
    pub working_dir: Option<String>,
    /// Per-command env override (merged with batch-level env; command wins).
    pub env: Option<HashMap<String, String>>,
    /// Per-command shell override.
    pub shell: Option<String>,
}

/// Response body for `POST /api/exec/batch`.
#[derive(Serialize)]
pub struct BatchExecResponse {
    /// Results in the same order as `commands`. Errors (timeout, spawn failure)
    /// are represented inline with `exit_code: -1` rather than aborting the
    /// batch.
    pub results: Vec<ExecResponse>,
    /// Echoed from request, omitted if not provided.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

/// `POST /api/exec/batch` — execute multiple commands sequentially.
///
/// Commands run one at a time in order. A failing command does **not** abort
/// the remaining commands — its error is captured in the results array so the
/// caller can inspect each outcome.
///
/// # Errors
///
/// - `400 Bad Request` with `{"code":"INVALID_REQUEST"}` — empty commands array
/// - `400 Bad Request` with `{"code":"BATCH_TOO_LARGE"}` — exceeds `max_batch_size`
pub async fn batch_exec(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<BatchExecRequest>,
) -> Result<Json<BatchExecResponse>, (StatusCode, Json<Value>)> {
    let source = activity::source_from_headers(&headers);
    if payload.commands.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "commands array is empty", "code": "INVALID_REQUEST"})),
        ));
    }
    if payload.commands.len() > state.config.server.max_batch_size {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("Too many commands (max {})", state.config.server.max_batch_size),
                "code": "BATCH_TOO_LARGE"
            })),
        ));
    }

    let default_shell = payload
        .shell
        .as_deref()
        .unwrap_or(&state.config.shell.default_shell);
    let default_dir = payload
        .working_dir
        .as_deref()
        .unwrap_or(&state.config.shell.default_working_dir);

    let expanded_default_dir = crate::util::expand_tilde(default_dir);

    let mut results = Vec::with_capacity(payload.commands.len());
    for cmd in &payload.commands {
        let shell = cmd.shell.as_deref().unwrap_or(default_shell);
        let raw_cmd_dir = cmd.working_dir.as_deref().unwrap_or(&expanded_default_dir);
        let expanded_cmd_dir = crate::util::expand_tilde(raw_cmd_dir);
        let working_dir = expanded_cmd_dir.as_ref();
        let timeout = cmd
            .timeout_ms
            .unwrap_or(state.config.server.exec_timeout_ms);

        // Per-command env merges batch-level env with command-level env (command wins)
        let merged_env = match (&payload.env, &cmd.env) {
            (None, None) => None,
            (Some(base), None) => Some(base.clone()),
            (None, Some(over)) => Some(over.clone()),
            (Some(base), Some(over)) => {
                let mut merged = base.clone();
                merged.extend(over.iter().map(|(k, v)| (k.clone(), v.clone())));
                Some(merged)
            }
        };

        match Box::pin(process::exec_command(
            shell,
            working_dir,
            &cmd.command,
            timeout,
            merged_env.as_ref(),
        ))
        .await
        {
            Ok(result) => {
                state
                    .activity_log
                    .log(
                        ActivityType::Exec,
                        source,
                        activity::truncate_str(&cmd.command, 80),
                        Some(json!({
                            "exit_code": result.exit_code,
                            "duration_ms": result.duration_ms,
                            "batch": true,
                        })),
                    )
                    .await;
                results.push(ExecResponse {
                    exit_code: result.exit_code,
                    stdout: result.stdout,
                    stderr: result.stderr,
                    duration_ms: result.duration_ms,
                    request_id: None,
                });
            }
            Err(process::ExecError::Timeout) => {
                results.push(ExecResponse {
                    exit_code: -1,
                    stdout: String::new(),
                    stderr: "Command timed out".to_string(),
                    duration_ms: timeout,
                    request_id: None,
                });
            }
            Err(e) => {
                results.push(ExecResponse {
                    exit_code: -1,
                    stdout: String::new(),
                    stderr: e.to_string(),
                    duration_ms: 0,
                    request_id: None,
                });
            }
        }
    }

    Ok(Json(BatchExecResponse {
        results,
        request_id: payload.request_id,
    }))
}
