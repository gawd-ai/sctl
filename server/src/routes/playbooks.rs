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
use crate::error::{codes, ApiError};
use crate::file_access::{self, Refusal};
use crate::AppState;

type ApiResult<T> = Result<Json<T>, (StatusCode, Json<ApiError>)>;

/// Maximum playbook content size (1 MB).
const MAX_PLAYBOOK_SIZE: usize = 1024 * 1024;

/// A playbook's file, read through the file gate's handle and its type
/// judgment (ADR-011): a regular file is read, at most
/// [`MAX_PLAYBOOK_SIZE`] bytes, and a FIFO, a socket or a device named
/// `*.md` in the playbooks directory is `NOT_A_REGULAR_FILE` and is never
/// opened. A plain read waited on a FIFO for good, and 32 such reads took
/// every handler the tunnel runs. `Ok(None)` when no file is there.
async fn read_playbook_file(path: std::path::PathBuf) -> Result<Option<String>, Refusal> {
    let read = tokio::task::spawn_blocking(move || file_access::read_own(&path, MAX_PLAYBOOK_SIZE))
        .await
        .map_err(|e| Refusal::Io(std::io::Error::other(e)))??;
    read.map(|bytes| {
        String::from_utf8(bytes)
            .map_err(|e| Refusal::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))
    })
    .transpose()
}

/// A refusal of a playbook's file, as the file routes answer the gate's.
fn playbook_refused(refusal: &Refusal, doing: &str) -> (StatusCode, Json<ApiError>) {
    let status = match refusal {
        Refusal::NotRegular(_) | Refusal::IsDirectory => StatusCode::BAD_REQUEST,
        Refusal::Protected { .. } | Refusal::PermissionDenied => StatusCode::FORBIDDEN,
        Refusal::NotFound => StatusCode::NOT_FOUND,
        Refusal::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    ApiError::new(
        refusal.code(),
        format!("Failed to {doing} playbook: {}", refusal.message()),
    )
    .into_response_with(status)
}

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

fn validate_playbook_name(name: &str) -> Result<(), (StatusCode, Json<ApiError>)> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(ApiError::new(
            codes::INVALID_REQUEST,
            "Invalid playbook name: only alphanumeric, hyphens, and underscores allowed",
        )
        .into_response_with(StatusCode::BAD_REQUEST));
    }
    Ok(())
}

// ─── Handlers ────────────────────────────────────────────────────────────────

/// `GET /api/playbooks` -- list all playbooks with summary info.
pub async fn list_playbooks(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Value> {
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
            return Err(ApiError::new(
                codes::IO_ERROR,
                format!("Failed to read playbooks dir: {e}"),
            )
            .into_response_with(StatusCode::INTERNAL_SERVER_ERROR));
        }
    };

    let mut playbooks = Vec::new();
    let mut entries = entries;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "md") {
            let content = match read_playbook_file(path.clone()).await {
                Ok(content) => content,
                Err(refusal) => {
                    tracing::warn!(path = %path.display(), "Skipping a playbook that cannot be read: {}", refusal.message());
                    None
                }
            };
            if let Some(content) = content {
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
) -> ApiResult<Value> {
    validate_playbook_name(&name)?;
    let source = source_from_headers(&headers);
    let req_id = request_id_from_headers(&headers);
    let file_path = format!("{}/{}.md", state.config.server.playbooks_dir, name);

    let content = read_playbook_file(std::path::PathBuf::from(&file_path))
        .await
        .map_err(|refusal| playbook_refused(&refusal, "read"))?
        .ok_or_else(|| {
            ApiError::new(codes::NOT_FOUND, format!("Playbook '{name}' not found"))
                .into_response_with(StatusCode::NOT_FOUND)
        })?;

    let (fm, script) = parse_playbook(&content).map_err(|e| {
        ApiError::new(codes::INVALID_CONTENT, format!("Invalid playbook: {e}"))
            .into_response_with(StatusCode::UNPROCESSABLE_ENTITY)
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
) -> ApiResult<Value> {
    validate_playbook_name(&name)?;
    let source = source_from_headers(&headers);
    let req_id = request_id_from_headers(&headers);

    if body.len() > MAX_PLAYBOOK_SIZE {
        return Err(ApiError::new(
            codes::FILE_TOO_LARGE,
            format!("Playbook content exceeds maximum size of {MAX_PLAYBOOK_SIZE} bytes"),
        )
        .into_response_with(StatusCode::PAYLOAD_TOO_LARGE));
    }

    // Validate the content parses correctly
    let (_fm, _script) = parse_playbook(&body).map_err(|e| {
        ApiError::new(
            codes::INVALID_CONTENT,
            format!("Invalid playbook content: {e}"),
        )
        .into_response_with(StatusCode::BAD_REQUEST)
    })?;

    let dir = &state.config.server.playbooks_dir;
    // Create dir if needed
    if let Err(e) = tokio::fs::create_dir_all(dir).await {
        return Err(ApiError::new(
            codes::IO_ERROR,
            format!("Failed to create playbooks dir: {e}"),
        )
        .into_response_with(StatusCode::INTERNAL_SERVER_ERROR));
    }

    let file_path = format!("{dir}/{name}.md");
    // An open for writing waits on a FIFO with no reader. What is at the
    // name is judged first, through the gate's handle (ADR-011): only
    // nothing, or a regular file, is written.
    let target = std::path::PathBuf::from(&file_path);
    tokio::task::spawn_blocking(move || file_access::writable_own(&target))
        .await
        .map_err(|e| Refusal::Io(std::io::Error::other(e)))
        .and_then(|judged| judged)
        .map_err(|refusal| playbook_refused(&refusal, "write"))?;
    // Written beside the name and renamed over it: the name itself is never
    // opened, so whatever takes its place after the judgment holds nothing.
    let beside = format!("{dir}/.{name}.md.tmp");
    let written = match tokio::fs::write(&beside, &body).await {
        Ok(()) => tokio::fs::rename(&beside, &file_path).await,
        Err(e) => Err(e),
    };
    if let Err(e) = written {
        let _ = tokio::fs::remove_file(&beside).await;
        return Err(
            ApiError::new(codes::IO_ERROR, format!("Failed to write playbook: {e}"))
                .into_response_with(StatusCode::INTERNAL_SERVER_ERROR),
        );
    }

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
) -> ApiResult<Value> {
    validate_playbook_name(&name)?;
    let source = source_from_headers(&headers);
    let req_id = request_id_from_headers(&headers);
    let file_path = format!("{}/{}.md", state.config.server.playbooks_dir, name);

    tokio::fs::remove_file(&file_path).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ApiError::new(codes::NOT_FOUND, format!("Playbook '{name}' not found"))
                .into_response_with(StatusCode::NOT_FOUND)
        } else {
            ApiError::new(codes::IO_ERROR, format!("Failed to delete playbook: {e}"))
                .into_response_with(StatusCode::INTERNAL_SERVER_ERROR)
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

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
    use std::path::{Path as FsPath, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::config::Config;
    use crate::file_access::Gate;

    const PLAYBOOK: &str =
        "---\nname: outlet-cycle\ndescription: Cycle an outlet\n---\n```sh\necho cycle\n```\n";

    /// A state whose playbooks directory is `<scratch>/playbooks`.
    fn bench(tag: &str) -> (PathBuf, AppState) {
        let dir = std::env::temp_dir().join(format!("sctl-playbooks-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("playbooks")).unwrap();
        let mut config: Config = toml::from_str("").unwrap();
        config.server.data_dir = dir.join("data").display().to_string();
        config.server.playbooks_dir = dir.join("playbooks").display().to_string();
        let gate = Arc::new(Gate::for_agent(&config, None, None));
        (dir, AppState::for_tests(config, gate))
    }

    fn mkfifo(path: &FsPath) {
        nix::unistd::mkfifo(path, nix::sys::stat::Mode::S_IRWXU).unwrap();
    }

    /// Wait for `answer` at most 5 s. A read or a write that waited on the
    /// FIFO would hang the run for good: on a timeout the FIFO gets its
    /// other end, opened without waiting, before the test fails.
    async fn at_once<T>(fifo: &FsPath, answer: impl std::future::Future<Output = T>) -> T {
        let Ok(answer) = tokio::time::timeout(Duration::from_secs(5), answer).await else {
            for write in [true, false] {
                let _ = std::fs::OpenOptions::new()
                    .read(!write)
                    .write(write)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(fifo);
            }
            panic!("the route waited on a FIFO");
        };
        answer
    }

    /// A FIFO named `*.md` in the playbooks directory held `GET
    /// /api/playbooks` for good, and 32 such reads took every handler the
    /// tunnel runs. It is judged by its type and never opened: the list
    /// answers at once without it.
    #[tokio::test]
    async fn the_list_skips_a_fifo_named_as_a_playbook_and_answers_at_once() {
        let (dir, state) = bench("list");
        std::fs::write(dir.join("playbooks/outlet-cycle.md"), PLAYBOOK).unwrap();
        let fifo = dir.join("playbooks/stuck.md");
        mkfifo(&fifo);
        for _ in 0..40 {
            let Json(body) = at_once(
                &fifo,
                list_playbooks(State(state.clone()), HeaderMap::new()),
            )
            .await
            .expect("the list answers");
            assert_eq!(body["playbooks"].as_array().unwrap().len(), 1, "{body}");
            assert_eq!(body["playbooks"][0]["name"], "outlet-cycle");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn reading_a_fifo_named_as_a_playbook_is_refused_at_once() {
        let (dir, state) = bench("get");
        std::fs::write(dir.join("playbooks/outlet-cycle.md"), PLAYBOOK).unwrap();
        let fifo = dir.join("playbooks/stuck.md");
        mkfifo(&fifo);
        for _ in 0..40 {
            let (status, Json(error)) = at_once(
                &fifo,
                get_playbook(State(state.clone()), Path("stuck".into()), HeaderMap::new()),
            )
            .await
            .expect_err("a FIFO is not a playbook");
            assert_eq!(
                (status, error.code.as_str()),
                (StatusCode::BAD_REQUEST, "NOT_A_REGULAR_FILE")
            );
            assert!(error.message.contains("a FIFO"), "{}", error.message);
        }
        // A playbook beside it is read as before, and a missing one is 404.
        let Json(body) = get_playbook(
            State(state.clone()),
            Path("outlet-cycle".into()),
            HeaderMap::new(),
        )
        .await
        .expect("a regular playbook");
        assert_eq!(body["script"], "echo cycle");
        assert_eq!(body["raw_content"], PLAYBOOK);
        let (status, _) = get_playbook(State(state), Path("none".into()), HeaderMap::new())
            .await
            .expect_err("no such playbook");
        assert_eq!(status, StatusCode::NOT_FOUND);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A write onto a FIFO with no reader waited in its open. What is at
    /// the name is judged first, and a write never opens the name itself.
    #[tokio::test]
    async fn a_write_onto_a_fifo_is_refused_at_once_and_a_playbook_is_replaced_whole() {
        let (dir, state) = bench("put");
        let fifo = dir.join("playbooks/stuck.md");
        mkfifo(&fifo);
        let put = |name: &str, body: &str| {
            put_playbook(
                State(state.clone()),
                Path(name.to_string()),
                HeaderMap::new(),
                body.to_string(),
            )
        };
        let stuck = PLAYBOOK.replace("outlet-cycle", "stuck");
        let (status, Json(error)) = at_once(&fifo, put("stuck", &stuck))
            .await
            .expect_err("a FIFO is not written");
        assert_eq!(
            (status, error.code.as_str()),
            (StatusCode::BAD_REQUEST, "NOT_A_REGULAR_FILE")
        );
        assert!(
            std::fs::symlink_metadata(&fifo)
                .unwrap()
                .file_type()
                .is_fifo(),
            "the FIFO is left as it was"
        );

        // A new playbook, then the same name again: written, then replaced.
        let file = dir.join("playbooks/outlet-cycle.md");
        let Json(written) = put("outlet-cycle", PLAYBOOK).await.expect("written");
        assert_eq!(written["ok"], true);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), PLAYBOOK);
        let again = PLAYBOOK.replace("echo cycle", "echo again");
        let Json(replaced) = put("outlet-cycle", &again).await.expect("replaced");
        assert_eq!(replaced["name"], "outlet-cycle");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), again);
        // Nothing is left beside it.
        let left: Vec<_> = std::fs::read_dir(dir.join("playbooks"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left.len(), 2, "{left:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
