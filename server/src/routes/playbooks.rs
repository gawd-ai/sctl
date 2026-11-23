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

/// Why a playbook's file was not read.
enum Unread {
    /// The gate's refusal: not a regular file, not readable.
    Refused(Refusal),
    /// Longer than [`MAX_PLAYBOOK_SIZE`]: its size in bytes. Never read in
    /// part: a part of a script must not be run as if it were the script.
    TooLarge(u64),
    /// Not UTF-8 text.
    NotText,
}

/// A playbook's file, read whole through the file gate's handle and its
/// type judgment (ADR-011): a regular file of at most
/// [`MAX_PLAYBOOK_SIZE`] bytes is read, and a FIFO, a socket or a device
/// named `*.md` in the playbooks directory is `NOT_A_REGULAR_FILE` and is
/// never opened. A plain read waited on a FIFO for good, and 32 such reads
/// took every handler the tunnel runs. `Ok(None)` when no file is there.
///
/// A longer file is [`Unread::TooLarge`], never its first megabyte: it
/// was returned cut, with no mark, and a cut inside a character of several
/// bytes failed as a `500`.
async fn read_playbook_file(path: std::path::PathBuf) -> Result<Option<String>, Unread> {
    let io = |e: std::io::Error| Unread::Refused(Refusal::Io(e));
    tokio::task::spawn_blocking(move || {
        let mut opened = match file_access::open_own(&path) {
            Ok(opened) => opened,
            Err(Refusal::NotFound) => return Ok(None),
            Err(refusal) => return Err(Unread::Refused(refusal)),
        };
        if opened.size > MAX_PLAYBOOK_SIZE as u64 {
            return Err(Unread::TooLarge(opened.size));
        }
        // To its end, one byte past the bound at most: a file that grew
        // since its size was read is judged by what is there now.
        #[allow(clippy::cast_possible_truncation)]
        let bytes = file_access::read_up_to(
            &mut opened.file,
            MAX_PLAYBOOK_SIZE + 1,
            opened.size as usize,
        )
        .map_err(io)?;
        if bytes.len() > MAX_PLAYBOOK_SIZE {
            let size = opened
                .file
                .metadata()
                .map_or(bytes.len() as u64, |m| m.len());
            return Err(Unread::TooLarge(size));
        }
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| Unread::NotText)
    })
    .await
    .map_err(|e| io(std::io::Error::other(e)))?
}

/// The refusal of a playbook longer than the bound.
fn too_large_message(name: &str, size: u64) -> String {
    format!(
        "Playbook '{name}' is {size} bytes, over the {MAX_PLAYBOOK_SIZE} byte bound: none of \
         it is returned, since a part of a script is not the script."
    )
}

/// Why a playbook's file was not read, as `GET /api/playbooks/:name`
/// answers it.
fn playbook_unread(unread: &Unread, name: &str) -> (StatusCode, Json<ApiError>) {
    match unread {
        Unread::Refused(refusal) => playbook_refused(refusal, "read"),
        Unread::TooLarge(size) => {
            ApiError::new(codes::PLAYBOOK_TOO_LARGE, too_large_message(name, *size))
                .into_response_with(StatusCode::UNPROCESSABLE_ENTITY)
        }
        Unread::NotText => ApiError::new(
            codes::INVALID_CONTENT,
            "Invalid playbook: the file is not UTF-8 text",
        )
        .into_response_with(StatusCode::UNPROCESSABLE_ENTITY),
    }
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
    /// `true` for a file over [`MAX_PLAYBOOK_SIZE`], which is named in the
    /// list by its file name and is not read. Absent otherwise.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    too_large: bool,
    /// That file's size in bytes. Absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
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
                // Named, by its file name: neither hidden nor cut. A file
                // whose name is not a playbook's cannot be asked for, and
                // is skipped like any other file that is not one.
                Err(Unread::TooLarge(size)) => {
                    let name = path
                        .file_stem()
                        .map(|stem| stem.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    if validate_playbook_name(&name).is_ok() {
                        playbooks.push(PlaybookSummary {
                            description: format!(
                                "Too large to read: {size} bytes, over the \
                                 {MAX_PLAYBOOK_SIZE} byte bound"
                            ),
                            name,
                            params: Vec::new(),
                            too_large: true,
                            size: Some(size),
                        });
                    } else {
                        tracing::warn!(path = %path.display(), size, "Skipping a file over the playbook bound");
                    }
                    None
                }
                Err(Unread::Refused(refusal)) => {
                    tracing::warn!(path = %path.display(), "Skipping a playbook that cannot be read: {}", refusal.message());
                    None
                }
                Err(Unread::NotText) => {
                    tracing::warn!(path = %path.display(), "Skipping a playbook that is not UTF-8 text");
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
                            too_large: false,
                            size: None,
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
        .map_err(|unread| playbook_unread(&unread, &name))?
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

/// Write `document` to a new file beside `target` and rename it over
/// `target`.
///
/// The file is this request's own: created new and exclusive (`O_EXCL`),
/// never through a link (`O_NOFOLLOW`), under a name no other request has.
/// A fixed name was opened plainly, so a FIFO placed there held the write,
/// a link placed there was written through, and two writes of one playbook
/// shared it. It takes the mode of the regular file it replaces, and the
/// default for a new playbook. It is removed on every failure. A rename
/// replaces the name itself, so a playbook that was a link becomes a
/// regular file, its target left as it was.
fn write_beside_and_rename(
    dir: &std::path::Path,
    target: &std::path::Path,
    document: &str,
) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mode = std::fs::symlink_metadata(target)
        .ok()
        .filter(std::fs::Metadata::is_file)
        .map(|m| m.permissions().mode() & 0o7777);
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let beside = dir.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4().simple()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&beside)?;
    let written = (|| {
        file.write_all(document.as_bytes())?;
        if let Some(mode) = mode {
            file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        file.sync_all()?;
        std::fs::rename(&beside, target)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&beside);
    }
    written
}

/// `PUT /api/playbooks/:name` -- create or update a playbook.
///
/// A playbook is replaced whole: the document is written to a file of its
/// own and renamed over the name. A playbook that was a symbolic link is
/// replaced by a regular file; what the link named is not written.
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
    // Written to a file of this request's own beside the name, and renamed
    // over it: the name itself is never opened, so whatever takes its place
    // after the judgment holds nothing.
    let (dir_path, target, document) = (
        std::path::PathBuf::from(dir),
        std::path::PathBuf::from(&file_path),
        body,
    );
    tokio::task::spawn_blocking(move || write_beside_and_rename(&dir_path, &target, &document))
        .await
        .map_err(std::io::Error::other)
        .and_then(|written| written)
        .map_err(|e| {
            ApiError::new(codes::IO_ERROR, format!("Failed to write playbook: {e}"))
                .into_response_with(StatusCode::INTERNAL_SERVER_ERROR)
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

    /// The file a write goes through is new, exclusive and named for the
    /// request alone. A fixed name was opened plainly: a FIFO placed there
    /// held the write, and a link placed there was written through.
    #[tokio::test]
    async fn a_write_goes_through_a_file_of_its_own_whatever_sits_at_a_guessable_name() {
        let (dir, state) = bench("temp");
        let playbooks = dir.join("playbooks");
        let put = |body: String| {
            put_playbook(
                State(state.clone()),
                Path("outlet-cycle".to_string()),
                HeaderMap::new(),
                body,
            )
        };
        // A FIFO at the name the write used to go through.
        let fifo = playbooks.join(".outlet-cycle.md.tmp");
        mkfifo(&fifo);
        let Json(written) = at_once(&fifo, put(PLAYBOOK.to_string()))
            .await
            .expect("written beside the FIFO, not through it");
        assert_eq!(written["ok"], true);
        assert_eq!(
            std::fs::read_to_string(playbooks.join("outlet-cycle.md")).unwrap(),
            PLAYBOOK
        );
        std::fs::remove_file(&fifo).unwrap();

        // A link there, to a file that is not a playbook: not written through.
        let victim = dir.join("victim");
        std::fs::write(&victim, "untouched").unwrap();
        std::os::unix::fs::symlink(&victim, &fifo).unwrap();
        let again = PLAYBOOK.replace("echo cycle", "echo again");
        let _ = put(again.clone()).await.expect("written");
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "untouched");
        assert_eq!(
            std::fs::read_to_string(playbooks.join("outlet-cycle.md")).unwrap(),
            again
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A replaced playbook keeps its mode, a new one takes the default, and
    /// a playbook that was a link is replaced by a regular file, the link's
    /// target left as it was.
    #[tokio::test]
    async fn a_replaced_playbook_keeps_its_mode_and_a_link_becomes_a_file() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, state) = bench("mode");
        let playbooks = dir.join("playbooks");
        let put = |name: &str, body: String| {
            put_playbook(
                State(state.clone()),
                Path(name.to_string()),
                HeaderMap::new(),
                body,
            )
        };
        let mode = |path: &FsPath| std::fs::metadata(path).unwrap().permissions().mode() & 0o7777;

        let file = playbooks.join("outlet-cycle.md");
        std::fs::write(&file, PLAYBOOK).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let again = PLAYBOOK.replace("echo cycle", "echo again");
        let _ = put("outlet-cycle", again.clone()).await.expect("replaced");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), again);
        assert_eq!(mode(&file), 0o600, "the mode of the file it replaced");

        // A new one: the mode any new file gets here.
        let probe = dir.join("probe");
        std::fs::write(&probe, "").unwrap();
        let fresh = PLAYBOOK.replace("outlet-cycle", "fresh");
        let _ = put("fresh", fresh).await.expect("written");
        assert_eq!(mode(&playbooks.join("fresh.md")), mode(&probe));

        // A playbook that is a link to a regular file elsewhere.
        let elsewhere = dir.join("elsewhere.md");
        let linked = PLAYBOOK.replace("outlet-cycle", "linked");
        std::fs::write(&elsewhere, &linked).unwrap();
        let link = playbooks.join("linked.md");
        std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
        let replaced = linked.replace("echo cycle", "echo replaced");
        let _ = put("linked", replaced.clone()).await.expect("replaced");
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_file(),
            "a regular file where the link was"
        );
        assert_eq!(std::fs::read_to_string(&link).unwrap(), replaced);
        assert_eq!(
            std::fs::read_to_string(&elsewhere).unwrap(),
            linked,
            "its target is left"
        );

        // Nothing is left beside any of them.
        let left: Vec<_> = std::fs::read_dir(&playbooks)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with('.'))
            .collect();
        assert!(left.is_empty(), "{left:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two writes of one name at once share nothing: both are answered,
    /// and the playbook is one of the two, whole.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_writes_of_one_name_at_once_leave_one_whole_playbook() {
        let (dir, state) = bench("two");
        let playbooks = dir.join("playbooks");
        let bodies: Vec<String> = (0..8)
            .map(|n| PLAYBOOK.replace("echo cycle", &format!("echo {}", "x".repeat(4000 + n))))
            .collect();
        for _ in 0..10 {
            let writes: Vec<_> = bodies
                .iter()
                .cloned()
                .map(|body| {
                    let state = state.clone();
                    tokio::spawn(async move {
                        put_playbook(
                            State(state),
                            Path("outlet-cycle".to_string()),
                            HeaderMap::new(),
                            body,
                        )
                        .await
                        .is_ok()
                    })
                })
                .collect();
            for write in writes {
                assert!(write.await.unwrap(), "each write is answered");
            }
            let on_disk = std::fs::read_to_string(playbooks.join("outlet-cycle.md")).unwrap();
            assert!(bodies.contains(&on_disk), "one of them, whole");
            assert_eq!(
                std::fs::read_dir(&playbooks).unwrap().count(),
                1,
                "nothing beside it"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A playbook's document of exactly `len` bytes whose script is whole
    /// near its start: the rest is prose after the script block, of
    /// `filler` repeated.
    fn document(name: &str, len: usize, filler: &str) -> String {
        let mut doc = PLAYBOOK.replace("outlet-cycle", name);
        while doc.len() + filler.len() <= len {
            doc.push_str(filler);
        }
        while doc.len() < len {
            doc.push('.');
        }
        assert_eq!(doc.len(), len);
        doc
    }

    /// A playbook over the bound was returned cut, with no mark, and a
    /// client would run the cut script; a cut inside a character of
    /// several bytes was a 500. Over the bound is a refusal with its own
    /// code, never a part of a script, and the list names such a file as
    /// too large, neither hidden nor cut.
    #[tokio::test]
    async fn a_playbook_over_the_bound_is_refused_whole_and_the_list_names_it() {
        let (dir, state) = bench("large");
        let playbooks = dir.join("playbooks");
        let get = |name: &str| {
            get_playbook(
                State(state.clone()),
                Path(name.to_string()),
                HeaderMap::new(),
            )
        };

        // At the bound: read whole.
        let at_bound = document("at-bound", MAX_PLAYBOOK_SIZE, "prose\n");
        std::fs::write(playbooks.join("at-bound.md"), &at_bound).unwrap();
        let Json(read) = get("at-bound").await.expect("a playbook at the bound");
        assert_eq!(
            read["raw_content"].as_str().unwrap().len(),
            MAX_PLAYBOOK_SIZE
        );
        assert_eq!(read["script"], "echo cycle");

        // One byte over, and one whose bound falls inside a character of
        // three bytes.
        let over = document("over", MAX_PLAYBOOK_SIZE + 1, "prose\n");
        std::fs::write(playbooks.join("over.md"), &over).unwrap();
        let mut split = document("split", MAX_PLAYBOOK_SIZE - 1, "prose\n");
        split.push_str("\u{20ac}\u{20ac}\n");
        std::fs::write(playbooks.join("split.md"), &split).unwrap();
        for (name, size) in [("over", over.len()), ("split", split.len())] {
            let (status, Json(refusal)) = get(name).await.expect_err("over the bound");
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{name}");
            assert_eq!(
                refusal.code,
                codes::PLAYBOOK_TOO_LARGE,
                "{name}: {}",
                refusal.message
            );
            assert!(
                refusal.message.contains(&format!("{size} bytes"))
                    && refusal.message.contains("1048576"),
                "{}",
                refusal.message
            );
        }

        // The list: the playbook that fits as before, the two that do not
        // named as too large, with their sizes.
        let Json(list) = list_playbooks(State(state.clone()), HeaderMap::new())
            .await
            .expect("the list");
        let mut listed: Vec<Value> = list["playbooks"].as_array().unwrap().clone();
        listed.sort_by_key(|p| p["name"].as_str().unwrap().to_string());
        assert_eq!(listed.len(), 3, "{list}");
        assert_eq!(
            listed[0],
            json!({"name": "at-bound", "description": "Cycle an outlet", "params": []})
        );
        for (entry, name, size) in [
            (&listed[1], "over", over.len()),
            (&listed[2], "split", split.len()),
        ] {
            assert_eq!(entry["name"], name);
            assert_eq!(entry["too_large"], true, "{entry}");
            assert_eq!(entry["size"], size);
            assert_eq!(entry["params"], json!([]));
            assert!(
                entry["description"]
                    .as_str()
                    .unwrap()
                    .starts_with("Too large to read"),
                "{entry}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A playbook that is not UTF-8 text is a file that does not parse,
    /// not a failure of the agent.
    #[tokio::test]
    async fn a_playbook_that_is_not_text_does_not_parse() {
        let (dir, state) = bench("not-text");
        let mut bytes = PLAYBOOK.as_bytes().to_vec();
        bytes.extend_from_slice(&[0xff, 0xfe, b'\n']);
        std::fs::write(dir.join("playbooks/outlet-cycle.md"), bytes).unwrap();
        let (status, Json(refusal)) = get_playbook(
            State(state.clone()),
            Path("outlet-cycle".to_string()),
            HeaderMap::new(),
        )
        .await
        .expect_err("not text");
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(refusal.code, codes::INVALID_CONTENT);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
