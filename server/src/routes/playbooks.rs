//! Playbook CRUD endpoints — list, get, create/update, delete.
//!
//! Playbooks are Markdown files with YAML frontmatter stored in the configured
//! `playbooks_dir`. The frontmatter defines name, description, and typed
//! parameters; the body must contain a fenced `sh` or `bash` code block.

use std::collections::HashMap;

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::activity::{request_id_from_headers, source_from_headers, ActivityType};
use crate::AppState;

/// Maximum playbook content size (1 MB).
const MAX_PLAYBOOK_SIZE: usize = 1024 * 1024;

// ─── Types ───────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct FrontMatter {
    name: String,
    description: String,
    #[serde(default)]
    params: HashMap<String, RawParam>,
}

#[derive(Deserialize)]
struct RawParam {
    #[serde(rename = "type", default = "default_param_type")]
    param_type: String,
    #[serde(default)]
    description: String,
    default: Option<Value>,
    #[serde(rename = "enum")]
    enum_values: Option<Vec<Value>>,
}

fn default_param_type() -> String {
    "string".to_string()
}

#[derive(Serialize)]
struct PlaybookSummary {
    name: String,
    description: String,
    params: Vec<String>,
}

#[derive(Serialize)]
struct ParamDetail {
    #[serde(rename = "type")]
    param_type: String,
    description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    default: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "enum")]
    enum_values: Option<Vec<Value>>,
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Parse YAML frontmatter and script from markdown content.
fn parse_playbook(markdown: &str) -> Result<(FrontMatter, String), String> {
    let trimmed = markdown.trim_start();
    if !trimmed.starts_with("---") {
        return Err("Missing YAML frontmatter (must start with ---)".into());
    }
    let after_open = &trimmed[3..];
    let close_pos = after_open
        .find("\n---")
        .ok_or("Missing closing --- for frontmatter")?;
    let yaml_str = &after_open[..close_pos];
    let body = &after_open[close_pos + 4..];

    let fm: FrontMatter =
        serde_yaml::from_str(yaml_str).map_err(|e| format!("YAML parse error: {e}"))?;

    if fm.name.trim().is_empty() {
        return Err("Playbook name is empty".into());
    }
    if fm.description.trim().is_empty() {
        return Err("Playbook description is empty".into());
    }
    if !fm
        .name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "Invalid playbook name '{}': only alphanumeric, hyphens, and underscores allowed",
            fm.name
        ));
    }

    let script = extract_script_block(body)?;
    Ok((fm, script))
}

fn extract_script_block(body: &str) -> Result<String, String> {
    let mut in_block = false;
    let mut script_lines = Vec::new();
    for line in body.lines() {
        if !in_block {
            let trimmed = line.trim();
            if trimmed.starts_with("```sh") || trimmed.starts_with("```bash") {
                in_block = true;
            }
        } else if line.trim().starts_with("```") {
            return Ok(script_lines.join("\n"));
        } else {
            script_lines.push(line);
        }
    }
    if in_block {
        return Err("Unclosed code block".into());
    }
    Err("No ```sh or ```bash code block found".into())
}

fn validate_playbook_name(name: &str) -> Result<(), (StatusCode, Json<Value>)> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(
                json!({"error": "Invalid playbook name: only alphanumeric, hyphens, and underscores allowed"}),
            ),
        ));
    }
    Ok(())
}

// ─── Handlers ────────────────────────────────────────────────────────────────

/// `GET /api/playbooks` -- list all playbooks with summary info.
pub async fn list_playbooks(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let dir = &state.config.server.playbooks_dir;
    let source = source_from_headers(&headers);
    let req_id = request_id_from_headers(&headers);

    let entries = match tokio::fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            state
                .activity_log
                .log(
                    ActivityType::PlaybookList,
                    source,
                    "Listed playbooks (empty)".into(),
                    None,
                    req_id.clone(),
                )
                .await;
            return Ok(Json(json!({"playbooks": []})));
        }
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to read playbooks dir: {e}")})),
            ));
        }
    };

    let mut playbooks = Vec::new();
    let mut entries = entries;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "md") {
            if let Ok(content) = tokio::fs::read_to_string(&path).await {
                match parse_playbook(&content) {
                    Ok((fm, _script)) => {
                        playbooks.push(PlaybookSummary {
                            name: fm.name,
                            description: fm.description,
                            params: fm.params.keys().cloned().collect(),
                        });
                    }
                    Err(e) => {
                        tracing::warn!(path = %path.display(), error = %e, "Skipping unparseable playbook");
                    }
                }
            }
        }
    }

    state
        .activity_log
        .log(
            ActivityType::PlaybookList,
            source,
            format!("Listed {} playbooks", playbooks.len()),
            None,
            req_id,
        )
        .await;

    Ok(Json(json!({"playbooks": playbooks})))
}

/// `GET /api/playbooks/:name` -- get full playbook detail.
pub async fn get_playbook(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    validate_playbook_name(&name)?;
    let source = source_from_headers(&headers);
    let req_id = request_id_from_headers(&headers);
    let file_path = format!("{}/{}.md", state.config.server.playbooks_dir, name);

    let content = tokio::fs::read_to_string(&file_path).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error": format!("Playbook '{name}' not found")})),
            )
        } else {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to read playbook: {e}")})),
            )
        }
    })?;

    let (fm, script) = parse_playbook(&content).map_err(|e| {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": format!("Invalid playbook: {e}")})),
        )
    })?;

    let params: HashMap<String, ParamDetail> = fm
        .params
        .into_iter()
        .map(|(k, v)| {
            (
                k,
                ParamDetail {
                    param_type: v.param_type,
                    description: v.description,
                    default: v.default,
                    enum_values: v.enum_values,
                },
            )
        })
        .collect();

    state
        .activity_log
        .log(
            ActivityType::PlaybookRead,
            source,
            format!("Read playbook '{name}'"),
            None,
            req_id,
        )
        .await;

    Ok(Json(json!({
        "name": fm.name,
        "description": fm.description,
        "params": params,
        "script": script,
        "raw_content": content,
    })))
}

/// `PUT /api/playbooks/:name` -- create or update a playbook.
pub async fn put_playbook(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    validate_playbook_name(&name)?;
    let source = source_from_headers(&headers);
    let req_id = request_id_from_headers(&headers);

    if body.len() > MAX_PLAYBOOK_SIZE {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(
                json!({"error": format!("Playbook content exceeds maximum size of {} bytes", MAX_PLAYBOOK_SIZE)}),
            ),
        ));
    }

    // Validate the content parses correctly
    let (_fm, _script) = parse_playbook(&body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("Invalid playbook content: {e}")})),
        )
    })?;

    let dir = &state.config.server.playbooks_dir;
    // Create dir if needed
    if let Err(e) = tokio::fs::create_dir_all(dir).await {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("Failed to create playbooks dir: {e}")})),
        ));
    }

    let file_path = format!("{dir}/{name}.md");
    tokio::fs::write(&file_path, &body).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("Failed to write playbook: {e}")})),
        )
    })?;

    state
        .activity_log
        .log(
            ActivityType::PlaybookWrite,
            source,
            format!("Wrote playbook '{name}'"),
            None,
            req_id,
        )
        .await;

    Ok(Json(json!({"ok": true, "name": name, "path": file_path})))
}

/// `DELETE /api/playbooks/:name` -- delete a playbook.
pub async fn delete_playbook(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    validate_playbook_name(&name)?;
    let source = source_from_headers(&headers);
    let req_id = request_id_from_headers(&headers);
    let file_path = format!("{}/{}.md", state.config.server.playbooks_dir, name);

    tokio::fs::remove_file(&file_path).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error": format!("Playbook '{name}' not found")})),
            )
        } else {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to delete playbook: {e}")})),
            )
        }
    })?;

    state
        .activity_log
        .log(
            ActivityType::PlaybookDelete,
            source,
            format!("Deleted playbook '{name}'"),
            None,
            req_id,
        )
        .await;

    Ok(Json(json!({"ok": true, "name": name})))
}
