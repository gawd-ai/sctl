//! File read, write, and directory listing endpoints.
//!
//! - `GET  /api/files?path=...`            — read a file
//! - `GET  /api/files?path=...&list=true`  — list a directory
//! - `PUT  /api/files`                     — write a file (atomic)
//!
//! ## Path validation
//!
//! All paths must be absolute and must not contain `..` components or null
//! bytes. This prevents path traversal attacks.
//!
//! ## The read gate
//!
//! A read (`GET /api/files`, `GET /api/files/raw`) passes
//! [`crate::file_access::Gate`] before anything is opened: the agent's keys,
//! a process's secrets, a kernel stream and a procfs or sysfs file off its
//! list are `PROTECTED_PATH`; a FIFO, a socket or a device is
//! `NOT_A_REGULAR_FILE` and is never opened (ADR-011). Each refusal is one
//! `FileRead` activity entry. Writes, deletes and listings keep their rules.
//!
//! ## Size limits
//!
//! Reads and writes are capped at `server.max_file_size` (default 50 MB).
//! Through the relay a read returns at most 4 MiB, half the tunnel's body.
//! Binary files are returned/accepted with base64 encoding.
//!
//! ## Atomicity
//!
//! File writes use a temp-file-then-rename pattern. On the same filesystem this
//! is atomic — readers never see a partially-written file. Cross-filesystem
//! renames will fail.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::SystemTime;

use crate::atomic::AtomicU64;

/// Monotonic counter to uniquify temp file names across concurrent writes.
pub(crate) static WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

use axum::{
    body::Body,
    extract::{Multipart, Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    Extension, Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::activity::{self, request_id_from_headers, ActivityType};
use crate::error::{codes, ApiError};
use crate::file_access::{self, Gate, Purpose, Refusal};
use crate::tunnel::client::{ViaTunnel, MAX_TUNNEL_BODY_BYTES};
use crate::AppState;

/// The most a read returns through the relay: half the tunnel's body, so
/// the answer fits even as base64 (ADR-011 decision 1).
pub const TUNNEL_READ_CAP: usize = MAX_TUNNEL_BODY_BYTES / 2;

type ApiResult<T> = Result<Json<T>, (StatusCode, Json<ApiError>)>;

/// Query parameters for `GET /api/files`.
#[derive(Deserialize)]
pub struct FilesQuery {
    /// Absolute path to the file or directory.
    pub path: String,
    /// When `true` (or when `path` ends with `/`), list directory contents
    /// instead of reading a file.
    #[serde(default)]
    pub list: bool,
    /// Byte offset to start reading from (for partial reads).
    #[serde(default)]
    pub offset: Option<u64>,
    /// Maximum number of bytes to read (for partial reads).
    #[serde(default)]
    pub limit: Option<usize>,
}

/// JSON response for a successful file read.
#[derive(Serialize)]
pub struct FileReadResponse {
    /// The path as given.
    pub path: String,
    /// File contents — UTF-8 text, or base64 if binary (see `encoding`).
    pub content: String,
    /// File size in bytes (total file size, not bytes returned). On a
    /// procfs or sysfs file, which reports no size, the bytes to where the
    /// read stopped.
    pub size: u64,
    /// Last-modified time as a Unix timestamp (seconds since epoch).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
    /// `"base64"` for binary files, absent for UTF-8 text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
    /// `true` when the file is larger than the returned content (partial read).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// JSON response for a directory listing.
#[derive(Serialize)]
pub struct DirListResponse {
    /// Directory that was listed.
    pub path: String,
    /// Sorted entries in the directory.
    pub entries: Vec<DirEntry>,
}

/// A single entry within a [`DirListResponse`].
#[derive(Serialize)]
pub struct DirEntry {
    /// File or directory name (basename only, no path).
    pub name: String,
    /// One of `"file"`, `"dir"`, `"symlink"`, or `"other"`.
    #[serde(rename = "type")]
    pub entry_type: String,
    /// Size in bytes (0 for directories).
    pub size: u64,
    /// Unix permission mode as octal string, e.g. `"0755"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Last-modified time as a Unix timestamp string.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
    /// For symlinks, the target path. Absent for other types.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symlink_target: Option<String>,
}

/// Request body for `DELETE /api/files`.
#[derive(Deserialize)]
pub struct FileDeleteRequest {
    /// Absolute path to the file to delete.
    pub path: String,
}

/// Query parameters for `GET /api/files/raw`.
#[derive(Deserialize)]
pub struct DownloadQuery {
    /// Absolute path to the file to download.
    pub path: String,
}

/// Query parameters for `POST /api/files/upload`.
#[derive(Deserialize)]
pub struct UploadQuery {
    /// Absolute path to the target directory.
    pub path: String,
}

/// Request body for `PUT /api/files`.
#[derive(Deserialize)]
pub struct FileWriteRequest {
    /// Absolute destination path.
    pub path: String,
    /// File contents — UTF-8 text, or base64 if `encoding` is `"base64"`.
    pub content: String,
    /// Create parent directories if they don't exist (default `false`).
    #[serde(default)]
    pub create_dirs: bool,
    /// Optional octal permission string, e.g. `"0644"`.
    pub mode: Option<String>,
    /// Set to `"base64"` if `content` is base64-encoded binary.
    pub encoding: Option<String>,
}

/// Validate that a user-supplied path is absolute, has no `..` traversal, and
/// contains no null bytes.
pub(crate) fn validate_path(path: &str) -> Result<PathBuf, (StatusCode, Json<ApiError>)> {
    let p = Path::new(path);
    if !p.is_absolute() {
        return Err(ApiError::new(codes::INVALID_PATH, "Path must be absolute")
            .into_response_with(StatusCode::BAD_REQUEST));
    }
    if path.contains('\0') {
        return Err(
            ApiError::new(codes::INVALID_PATH, "Path contains null bytes")
                .into_response_with(StatusCode::BAD_REQUEST),
        );
    }
    for component in p.components() {
        if let std::path::Component::ParentDir = component {
            return Err(
                ApiError::new(codes::INVALID_PATH, "Path traversal (..) not allowed")
                    .into_response_with(StatusCode::BAD_REQUEST),
            );
        }
    }
    Ok(p.to_path_buf())
}

/// Convert a [`SystemTime`] to a Unix epoch seconds string.
fn format_system_time(time: SystemTime) -> Option<String> {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs().to_string())
}

/// `GET /api/files` — read a file or list a directory.
///
/// A read passes the gate first (ADR-011). Through the tunnel (the
/// [`ViaTunnel`] mark) it returns at most [`TUNNEL_READ_CAP`] bytes.
///
/// # Error codes
///
/// | HTTP | Code                 | Meaning                                  |
/// |------|----------------------|------------------------------------------|
/// | 400  | `INVALID_PATH`       | Path is relative, has `..`, etc.         |
/// | 400  | `IS_DIRECTORY`       | Path is a dir but `list` is off          |
/// | 400  | `FILE_TOO_LARGE`     | File exceeds the cap, with no range      |
/// | 400  | `NOT_A_REGULAR_FILE` | A FIFO, a socket or a device             |
/// | 403  | `PROTECTED_PATH`     | The gate's rules refuse it               |
/// | 403  | `PERMISSION_DENIED`  | OS permission error                      |
/// | 404  | `FILE_NOT_FOUND`     | File or directory does not exist         |
/// | 500  | `IO_ERROR`           | Other I/O failure                        |
pub async fn get_file(
    State(state): State<AppState>,
    via_tunnel: Option<Extension<ViaTunnel>>,
    headers: HeaderMap,
    Query(query): Query<FilesQuery>,
) -> ApiResult<Value> {
    let source = activity::source_from_headers(&headers);
    let req_id = request_id_from_headers(&headers);
    let path = validate_path(&query.path)?;

    if query.list || query.path.ends_with('/') {
        let result = list_directory(&path).await?;
        state
            .activity_log
            .log(
                ActivityType::FileList,
                source,
                activity::truncate_str(&query.path, 80),
                None,
                req_id,
            )
            .await;
        return Ok(result);
    }

    let result = match read_file(
        state.file_gate.clone(),
        &path,
        ReadLimits {
            max_size: state.config.server.max_file_size,
            offset: query.offset,
            limit: query.limit,
            via_tunnel: via_tunnel.is_some(),
        },
    )
    .await
    {
        Ok(v) => v,
        Err(ReadFailure::Gate(refusal)) => {
            file_access::record(&state.activity_log, source, &query.path, &refusal, req_id).await;
            return Err(refusal_response(
                &refusal,
                "Path is a directory, use list=true",
            ));
        }
        Err(ReadFailure::Api(e)) => return Err(e),
    };
    state
        .activity_log
        .log(
            ActivityType::FileRead,
            source,
            activity::truncate_str(&query.path, 80),
            None,
            req_id,
        )
        .await;
    Ok(Json(result))
}

/// A gate refusal as the route's error; `directory` words `IS_DIRECTORY`.
fn refusal_response(refusal: &Refusal, directory: &str) -> (StatusCode, Json<ApiError>) {
    let status = match refusal {
        Refusal::Protected { .. } | Refusal::PermissionDenied => StatusCode::FORBIDDEN,
        Refusal::NotRegular(_) | Refusal::IsDirectory => StatusCode::BAD_REQUEST,
        Refusal::NotFound => StatusCode::NOT_FOUND,
        Refusal::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let message = match refusal {
        Refusal::IsDirectory => directory.to_string(),
        other => other.message(),
    };
    ApiError::new(refusal.code(), message).into_response_with(status)
}

/// What bounds one read.
#[derive(Clone, Copy, Debug)]
struct ReadLimits {
    /// `server.max_file_size`.
    max_size: usize,
    offset: Option<u64>,
    limit: Option<usize>,
    /// The request came through the tunnel.
    via_tunnel: bool,
}

impl ReadLimits {
    /// `max_file_size`, or the request's `limit` below it; through the
    /// tunnel at most [`TUNNEL_READ_CAP`].
    fn cap(self) -> usize {
        let cap = self.limit.map_or(self.max_size, |l| l.min(self.max_size));
        if self.via_tunnel {
            cap.min(TUNNEL_READ_CAP)
        } else {
            cap
        }
    }

    fn ranged(self) -> bool {
        self.offset.is_some() || self.limit.is_some()
    }
}

/// Why a read returned nothing: the gate refused it (recorded on the unit),
/// or the route did.
enum ReadFailure {
    Gate(Refusal),
    Api((StatusCode, Json<ApiError>)),
}

fn io_failure(e: &impl std::fmt::Display) -> ReadFailure {
    ReadFailure::Api(
        ApiError::new(codes::IO_ERROR, e.to_string())
            .into_response_with(StatusCode::INTERNAL_SERVER_ERROR),
    )
}

fn too_large(message: String) -> ReadFailure {
    ReadFailure::Api(
        ApiError::new(codes::FILE_TOO_LARGE, message).into_response_with(StatusCode::BAD_REQUEST),
    )
}

/// How many bytes a value serializes to, without building them.
fn json_len(value: &Value) -> usize {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    let _ = serde_json::to_writer(&mut count, value);
    count.0
}

/// Read a single file through the gate, returning UTF-8 text or base64 for
/// binary.
///
/// A regular file is read up to the size its stat gave (from `offset`,
/// within the cap); without a range, a file over the cap is refused before
/// it is read. A listed procfs or sysfs file is read until it ends or the
/// cap stops it: `size` is then the bytes to where the read stopped, and
/// `truncated` says the cap stopped it. Through the tunnel an answer that
/// would still not fit its body is `FILE_TOO_LARGE`, never the tunnel's 502.
async fn read_file(gate: Arc<Gate>, path: &Path, limits: ReadLimits) -> Result<Value, ReadFailure> {
    let cap = limits.cap();
    let judged = path.to_path_buf();
    let (out, modified) = tokio::task::spawn_blocking(move || {
        let mut opened = gate
            .open(&judged, Purpose::Read)
            .map_err(ReadFailure::Gate)?;
        if !limits.ranged() && opened.pseudo.is_none() && opened.size > cap as u64 {
            let size = opened.size;
            return Err(too_large(if limits.via_tunnel {
                format!("File too large ({size} bytes, max {cap} through the relay)")
            } else {
                format!("File too large ({size} bytes, max {cap})")
            }));
        }
        let out = opened
            .read(limits.offset.unwrap_or(0), cap)
            .map_err(|e| io_failure(&e))?;
        Ok((out, opened.modified))
    })
    .await
    .map_err(|e| io_failure(&e))??;

    let modified = modified.map(|secs| secs.to_string());
    let path_str = path.to_string_lossy().into_owned();
    let response = match String::from_utf8(out.bytes) {
        Ok(text) => FileReadResponse {
            path: path_str,
            content: text,
            size: out.size,
            modified,
            encoding: None,
            truncated: out.truncated,
        },
        Err(e) => {
            use base64::Engine;
            FileReadResponse {
                path: path_str,
                content: base64::engine::general_purpose::STANDARD.encode(e.as_bytes()),
                size: out.size,
                modified,
                encoding: Some("base64".to_string()),
                truncated: out.truncated,
            }
        }
    };
    let value = serde_json::to_value(response).map_err(|e| io_failure(&e))?;
    if limits.via_tunnel {
        let len = json_len(&value);
        if len > MAX_TUNNEL_BODY_BYTES {
            return Err(too_large(format!(
                "The answer would be {len} bytes, over the relay's {MAX_TUNNEL_BODY_BYTES}; ask a smaller limit"
            )));
        }
    }
    Ok(value)
}

/// List a directory's contents, sorted by name.
async fn list_directory(path: &Path) -> ApiResult<Value> {
    let mut read_dir = match tokio::fs::read_dir(path).await {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ApiError::new(codes::FILE_NOT_FOUND, "Directory not found")
                .into_response_with(StatusCode::NOT_FOUND));
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(ApiError::new(codes::PERMISSION_DENIED, "Permission denied")
                .into_response_with(StatusCode::FORBIDDEN));
        }
        Err(e) => {
            return Err(ApiError::new(codes::IO_ERROR, e.to_string())
                .into_response_with(StatusCode::INTERNAL_SERVER_ERROR));
        }
    };

    let mut entries = Vec::new();
    while let Ok(Some(entry)) = read_dir.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        // file_type() uses lstat (doesn't follow symlinks), so is_symlink() works.
        // metadata() uses stat (follows symlinks), so we use it only for size/modified.
        let file_type = entry.file_type().await.ok();
        let metadata = entry.metadata().await.ok();

        let (entry_type, symlink_target) = if file_type
            .as_ref()
            .is_some_and(std::fs::FileType::is_symlink)
        {
            let target = tokio::fs::read_link(entry.path())
                .await
                .ok()
                .map(|p: PathBuf| p.to_string_lossy().into_owned());
            ("symlink".to_string(), target)
        } else if file_type.as_ref().is_some_and(std::fs::FileType::is_dir) {
            ("dir".to_string(), None)
        } else if file_type.as_ref().is_some_and(std::fs::FileType::is_file) {
            ("file".to_string(), None)
        } else {
            ("other".to_string(), None)
        };

        let size = metadata.as_ref().map_or(0, std::fs::Metadata::len);
        let mode = metadata
            .as_ref()
            .map(|m| format!("{:04o}", m.permissions().mode() & 0o7777));
        let modified = metadata
            .as_ref()
            .and_then(|m: &std::fs::Metadata| m.modified().ok())
            .and_then(format_system_time);

        entries.push(DirEntry {
            name,
            entry_type,
            size,
            mode,
            modified,
            symlink_target,
        });
    }

    entries.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(Json(
        serde_json::to_value(DirListResponse {
            path: path.to_string_lossy().into_owned(),
            entries,
        })
        .unwrap(),
    ))
}

/// `PUT /api/files` — write a file atomically.
///
/// The file is first written to a temporary path in the same directory, then
/// renamed over the target. This ensures readers never see partial content.
///
/// # Error codes
///
/// | HTTP | Code               | Meaning                            |
/// |------|--------------------|----------------------------------  |
/// | 400  | `INVALID_PATH`     | Path validation failed             |
/// | 400  | `INVALID_CONTENT`  | base64 decoding failed             |
/// | 400  | `FILE_TOO_LARGE`   | Content exceeds `max_file_size`    |
/// | 403  | `PERMISSION_DENIED`| OS permission error                |
/// | 500  | `IO_ERROR`         | Write, chmod, or rename failure    |
pub async fn put_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<FileWriteRequest>,
) -> ApiResult<Value> {
    let source = activity::source_from_headers(&headers);
    let req_id = request_id_from_headers(&headers);
    let path = validate_path(&payload.path)?;

    let bytes = if payload.encoding.as_deref() == Some("base64") {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(&payload.content)
            .map_err(|e| {
                ApiError::new(codes::INVALID_CONTENT, format!("Invalid base64: {e}"))
                    .into_response_with(StatusCode::BAD_REQUEST)
            })?
    } else {
        payload.content.into_bytes()
    };

    if bytes.len() > state.config.server.max_file_size {
        return Err(ApiError::new(
            codes::FILE_TOO_LARGE,
            format!(
                "Content too large ({} bytes, max {})",
                bytes.len(),
                state.config.server.max_file_size
            ),
        )
        .into_response_with(StatusCode::BAD_REQUEST));
    }

    if payload.create_dirs {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e: std::io::Error| {
                    ApiError::new(codes::IO_ERROR, e.to_string())
                        .into_response_with(StatusCode::INTERNAL_SERVER_ERROR)
                })?;
        }
    }

    // Atomic write: write to temp file in same directory, then rename
    let parent = path.parent().unwrap_or(Path::new("/"));
    let seq = WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp_path = parent.join(format!(".sctl_tmp_{}_{}", std::process::id(), seq));

    tokio::fs::write(&temp_path, &bytes)
        .await
        .map_err(|e: std::io::Error| {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                ApiError::new(codes::PERMISSION_DENIED, "Permission denied")
                    .into_response_with(StatusCode::FORBIDDEN)
            } else {
                ApiError::new(codes::IO_ERROR, e.to_string())
                    .into_response_with(StatusCode::INTERNAL_SERVER_ERROR)
            }
        })?;

    // Set file mode if specified (octal string, e.g. "0644")
    if let Some(ref mode_str) = payload.mode {
        let mode = u32::from_str_radix(mode_str, 8).map_err(|_| {
            // Clean up temp file before returning error
            let tp = temp_path.clone();
            tokio::spawn(async move {
                let _ = tokio::fs::remove_file(&tp).await;
            });
            ApiError::new(
                codes::INVALID_MODE,
                format!("Invalid octal mode: {mode_str:?}"),
            )
            .into_response_with(StatusCode::BAD_REQUEST)
        })?;
        let perms = std::fs::Permissions::from_mode(mode);
        if let Err(e) = tokio::fs::set_permissions(&temp_path, perms).await {
            let _ = tokio::fs::remove_file(&temp_path).await;
            return Err(
                ApiError::new(codes::IO_ERROR, format!("Failed to set mode: {e}"))
                    .into_response_with(StatusCode::INTERNAL_SERVER_ERROR),
            );
        }
    }

    rename_temp_to_final(&temp_path, &path).await?;
    log_file_write(
        &state,
        source,
        &payload.path,
        bytes.len(),
        payload.mode.as_ref(),
        req_id,
    )
    .await;

    Ok(Json(json!({
        "path": path.to_string_lossy(),
        "size": bytes.len(),
        "ok": true
    })))
}

pub(crate) async fn rename_temp_to_final(
    temp_path: &Path,
    final_path: &Path,
) -> Result<(), (StatusCode, Json<ApiError>)> {
    tokio::fs::rename(temp_path, final_path).await.map_err(|e| {
        let tp = temp_path.to_path_buf();
        tokio::spawn(async move {
            let _ = tokio::fs::remove_file(&tp).await;
        });
        ApiError::new(codes::IO_ERROR, format!("Failed to rename: {e}"))
            .into_response_with(StatusCode::INTERNAL_SERVER_ERROR)
    })
}

async fn log_file_write(
    state: &AppState,
    source: activity::ActivitySource,
    path: &str,
    size: usize,
    mode: Option<&String>,
    request_id: Option<String>,
) {
    state
        .activity_log
        .log(
            ActivityType::FileWrite,
            source,
            activity::truncate_str(path, 80),
            Some(json!({ "size": size, "mode": mode })),
            request_id,
        )
        .await;
}

/// `DELETE /api/files` — delete a file.
///
/// # Error codes
///
/// | HTTP | Code               | Meaning                          |
/// |------|--------------------|----------------------------------|
/// | 400  | `INVALID_PATH`     | Path validation failed           |
/// | 403  | `PERMISSION_DENIED`| OS permission error              |
/// | 404  | `FILE_NOT_FOUND`   | File does not exist              |
/// | 500  | `IO_ERROR`         | Other I/O failure                |
pub async fn delete_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<FileDeleteRequest>,
) -> ApiResult<Value> {
    let source = activity::source_from_headers(&headers);
    let req_id = request_id_from_headers(&headers);
    let path = validate_path(&payload.path)?;

    tokio::fs::remove_file(&path).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ApiError::new(codes::FILE_NOT_FOUND, "File not found")
                .into_response_with(StatusCode::NOT_FOUND)
        } else if e.kind() == std::io::ErrorKind::PermissionDenied {
            ApiError::new(codes::PERMISSION_DENIED, "Permission denied")
                .into_response_with(StatusCode::FORBIDDEN)
        } else {
            ApiError::new(codes::IO_ERROR, e.to_string())
                .into_response_with(StatusCode::INTERNAL_SERVER_ERROR)
        }
    })?;

    state
        .activity_log
        .log(
            ActivityType::FileDelete,
            source,
            activity::truncate_str(&payload.path, 80),
            None,
            req_id,
        )
        .await;

    Ok(Json(json!({
        "ok": true,
        "path": path.to_string_lossy(),
    })))
}

/// `GET /api/files/raw` — stream a file as raw bytes (no base64, no size cap).
///
/// Only a regular file on a filesystem that reports its size moves, through
/// the gate (ADR-011): exactly the size its stat gave, with that
/// `Content-Length`, so a log that grows during the download is cut where
/// its size was read.
///
/// Returns `application/octet-stream` with `Content-Disposition: attachment`.
pub async fn download_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<DownloadQuery>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    use tokio::io::AsyncReadExt as _;

    let source = activity::source_from_headers(&headers);
    let req_id = request_id_from_headers(&headers);
    let path = validate_path(&query.path)?;

    let gate = state.file_gate.clone();
    let judged = path.clone();
    let opened = match tokio::task::spawn_blocking(move || gate.open(&judged, Purpose::Move)).await
    {
        Ok(Ok(opened)) => opened,
        Ok(Err(refusal)) => {
            file_access::record(&state.activity_log, source, &query.path, &refusal, req_id).await;
            return Err(refusal_response(&refusal, "Path is a directory"));
        }
        Err(e) => {
            return Err(ApiError::new(codes::IO_ERROR, e.to_string())
                .into_response_with(StatusCode::INTERNAL_SERVER_ERROR));
        }
    };

    let file_size = opened.size;
    let file = tokio::fs::File::from_std(opened.file);
    let basename = path.file_name().map_or_else(
        || "download".to_string(),
        |n| n.to_string_lossy().into_owned(),
    );

    state
        .activity_log
        .log(
            ActivityType::FileRead,
            source,
            activity::truncate_str(&query.path, 80),
            None,
            req_id,
        )
        .await;

    let stream = tokio_util::io::ReaderStream::new(file.take(file_size));
    let body = Body::from_stream(stream);

    Response::builder()
        .header("Content-Type", "application/octet-stream")
        .header(
            "Content-Disposition",
            content_disposition_attachment(&basename),
        )
        .header("Content-Length", file_size)
        .body(body)
        .map_err(|e| {
            ApiError::new(codes::IO_ERROR, format!("response build failed: {e}"))
                .into_response_with(StatusCode::INTERNAL_SERVER_ERROR)
        })
}

/// Build a `Content-Disposition: attachment` header value that is valid for
/// any filename, per RFC 6266/5987.
///
/// HTTP header values must be visible ASCII; a filename is arbitrary bytes.
/// The previous `format!("filename=\"{name}\"")` produced an invalid header
/// value for any non-ASCII or control character, which the response builder
/// turned into a panic — a remote panic reachable by downloading a file
/// someone else had written to disk.
///
/// Shape: an ASCII-sanitized `filename="..."` fallback always present, plus
/// a `filename*=UTF-8''...` parameter carrying the percent-encoded true name
/// whenever sanitization lost information. Compliant clients prefer
/// `filename*`; older ones use the fallback.
fn content_disposition_attachment(basename: &str) -> String {
    // RFC 5987 attr-char: ALPHA / DIGIT / !#$&+-.^_`|~ — everything else is
    // percent-encoded from its UTF-8 bytes.
    fn percent_encode(name: &str) -> String {
        use std::fmt::Write as _;
        let mut out = String::with_capacity(name.len() * 3);
        for byte in name.bytes() {
            match byte {
                b'A'..=b'Z'
                | b'a'..=b'z'
                | b'0'..=b'9'
                | b'!'
                | b'#'
                | b'$'
                | b'&'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~' => out.push(byte as char),
                _ => {
                    let _ = write!(out, "%{byte:02X}");
                }
            }
        }
        out
    }

    // Fallback: printable ASCII only, with the two characters that break a
    // quoted-string (`"` and `\`) replaced alongside anything non-printable.
    let fallback: String = basename
        .chars()
        .map(|c| match c {
            '"' | '\\' => '_',
            c if c.is_ascii_graphic() || c == ' ' => c,
            _ => '_',
        })
        .collect();

    if fallback == basename {
        format!("attachment; filename=\"{fallback}\"")
    } else {
        format!(
            "attachment; filename=\"{fallback}\"; filename*=UTF-8''{}",
            percent_encode(basename)
        )
    }
}

/// `POST /api/files/upload` — accept multipart file uploads into a directory.
///
/// Query param `path` is the target directory. Each form field named `files`
/// contains one file. Uses the same atomic temp-file-then-rename pattern as
/// `put_file`.
pub async fn upload_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<UploadQuery>,
    mut multipart: Multipart,
) -> ApiResult<Value> {
    let source = activity::source_from_headers(&headers);
    let req_id = request_id_from_headers(&headers);
    let dir_path = validate_path(&query.path)?;

    // Verify target is a directory
    let meta = tokio::fs::metadata(&dir_path)
        .await
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                ApiError::new(codes::FILE_NOT_FOUND, "Directory not found")
                    .into_response_with(StatusCode::NOT_FOUND)
            }
            std::io::ErrorKind::PermissionDenied => {
                ApiError::new(codes::PERMISSION_DENIED, "Permission denied")
                    .into_response_with(StatusCode::FORBIDDEN)
            }
            _ => ApiError::new(codes::IO_ERROR, e.to_string())
                .into_response_with(StatusCode::INTERNAL_SERVER_ERROR),
        })?;

    if !meta.is_dir() {
        return Err(
            ApiError::new(codes::NOT_A_DIRECTORY, "Target path is not a directory")
                .into_response_with(StatusCode::BAD_REQUEST),
        );
    }

    let max_size = state.config.server.max_file_size;
    let mut uploaded: Vec<Value> = Vec::new();

    while let Some(field) = multipart.next_field().await.map_err(|e| {
        ApiError::new(codes::MULTIPART_ERROR, format!("Multipart error: {e}"))
            .into_response_with(StatusCode::BAD_REQUEST)
    })? {
        let file_name = match field.file_name() {
            Some(name) => name.to_string(),
            None => continue,
        };

        // Reject path traversal in filenames
        if file_name.contains('/') || file_name.contains('\\') || file_name == ".." {
            return Err(ApiError::new(
                codes::INVALID_PATH,
                format!("Invalid filename: {file_name}"),
            )
            .into_response_with(StatusCode::BAD_REQUEST));
        }

        let bytes = field.bytes().await.map_err(|e| {
            ApiError::new(codes::MULTIPART_ERROR, format!("Failed to read field: {e}"))
                .into_response_with(StatusCode::BAD_REQUEST)
        })?;

        if bytes.len() > max_size {
            return Err(ApiError::new(
                codes::FILE_TOO_LARGE,
                format!(
                    "File '{}' too large ({} bytes, max {})",
                    file_name,
                    bytes.len(),
                    max_size
                ),
            )
            .into_response_with(StatusCode::BAD_REQUEST));
        }

        let final_path = dir_path.join(&file_name);
        let seq = WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp_path = dir_path.join(format!(".sctl_tmp_{}_{}", std::process::id(), seq));

        tokio::fs::write(&temp_path, &bytes).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                ApiError::new(codes::PERMISSION_DENIED, "Permission denied")
                    .into_response_with(StatusCode::FORBIDDEN)
            } else {
                ApiError::new(codes::IO_ERROR, e.to_string())
                    .into_response_with(StatusCode::INTERNAL_SERVER_ERROR)
            }
        })?;

        rename_temp_to_final(&temp_path, &final_path).await?;

        let full_path_str = final_path.to_string_lossy().to_string();
        let size = bytes.len();
        uploaded.push(json!({"path": full_path_str, "size": size}));

        state
            .activity_log
            .log(
                ActivityType::FileWrite,
                source,
                activity::truncate_str(&full_path_str, 80),
                Some(json!({"size": size, "upload": true})),
                req_id.clone(),
            )
            .await;
    }

    Ok(Json(json!({
        "ok": true,
        "files": uploaded
    })))
}

#[cfg(test)]
mod content_disposition_tests {
    use axum::http::HeaderValue;

    use super::content_disposition_attachment;

    /// Every produced value must be a legal HTTP header — this is the
    /// property whose absence was a remotely reachable panic.
    fn assert_valid_header(value: &str) {
        assert!(
            HeaderValue::from_str(value).is_ok(),
            "not a valid header value: {value:?}"
        );
    }

    #[test]
    fn plain_ascii_gets_no_extended_parameter() {
        let v = content_disposition_attachment("report.txt");
        assert_eq!(v, "attachment; filename=\"report.txt\"");
        assert_valid_header(&v);
    }

    #[test]
    fn spaces_survive_inside_the_quoted_fallback() {
        let v = content_disposition_attachment("my report.txt");
        assert_eq!(v, "attachment; filename=\"my report.txt\"");
        assert_valid_header(&v);
    }

    #[test]
    fn quotes_and_backslashes_cannot_break_the_quoted_string() {
        let v = content_disposition_attachment("a\"b\\c.txt");
        assert!(v.starts_with("attachment; filename=\"a_b_c.txt\";"));
        assert!(v.contains("filename*=UTF-8''a%22b%5Cc.txt"));
        assert_valid_header(&v);
    }

    #[test]
    fn utf8_names_ride_in_the_extended_parameter() {
        let v = content_disposition_attachment("résumé.pdf");
        assert!(v.starts_with("attachment; filename=\"r_sum_.pdf\";"));
        assert!(v.contains("filename*=UTF-8''r%C3%A9sum%C3%A9.pdf"));
        assert_valid_header(&v);
    }

    #[test]
    fn cjk_names_are_fully_encoded() {
        let v = content_disposition_attachment("日誌.log");
        assert!(v.contains("filename*=UTF-8''%E6%97%A5%E8%AA%8C.log"));
        assert_valid_header(&v);
    }

    #[test]
    fn control_characters_never_reach_the_header() {
        let v = content_disposition_attachment("evil\r\nInjected: header");
        assert!(!v.contains('\r') && !v.contains('\n'));
        assert!(v.contains("filename*=UTF-8''evil%0D%0AInjected%3A%20header"));
        assert_valid_header(&v);
    }

    #[test]
    fn del_byte_is_sanitized() {
        let v = content_disposition_attachment("a\u{7f}b.bin");
        assert!(v.starts_with("attachment; filename=\"a_b.bin\";"));
        assert_valid_header(&v);
    }
}

#[cfg(test)]
mod gate_tests {
    use std::sync::atomic::AtomicU32;

    use super::*;
    use crate::activity::ActivityEntry;
    use crate::config::Config;

    fn scratch(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "sctl-files-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A state whose data and state directory is `<dir>/data`.
    fn state_in(dir: &Path) -> AppState {
        let mut config: Config = toml::from_str("").unwrap();
        config.server.data_dir = dir.join("data").display().to_string();
        let gate = Arc::new(Gate::for_agent(&config, None, None));
        AppState::for_tests(config, gate)
    }

    fn query(path: &Path, offset: Option<u64>, limit: Option<usize>) -> FilesQuery {
        FilesQuery {
            path: path.display().to_string(),
            list: false,
            offset,
            limit,
        }
    }

    async fn read(
        state: &AppState,
        q: FilesQuery,
        via_tunnel: bool,
    ) -> Result<Value, (StatusCode, ApiError)> {
        get_file(
            State(state.clone()),
            via_tunnel.then_some(Extension(ViaTunnel)),
            HeaderMap::new(),
            Query(q),
        )
        .await
        .map(|Json(v)| v)
        .map_err(|(status, Json(e))| (status, e))
    }

    async fn refusals(state: &AppState) -> Vec<ActivityEntry> {
        state
            .activity_log
            .read_since(0, 100)
            .await
            .into_iter()
            .filter(|e| e.summary.starts_with("read refused"))
            .collect()
    }

    #[tokio::test]
    async fn each_refusal_answers_its_code_and_leaves_one_entry() {
        let dir = scratch("refusals");
        let state = state_in(&dir);
        std::fs::create_dir_all(dir.join("data")).unwrap();
        std::fs::write(dir.join("data/infra-monitor.json"), "{}").unwrap();
        let fifo = dir.join("pipe");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).unwrap();

        let (status, e) = read(
            &state,
            query(Path::new("/proc/self/environ"), None, None),
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(
            (status, e.code.as_str()),
            (StatusCode::FORBIDDEN, "PROTECTED_PATH")
        );
        assert_eq!(e.message, "Refused: a process's environment");

        let (status, e) = read(&state, query(&fifo, None, None), false)
            .await
            .unwrap_err();
        assert_eq!(
            (status, e.code.as_str()),
            (StatusCode::BAD_REQUEST, "NOT_A_REGULAR_FILE")
        );

        let (status, e) = read(
            &state,
            query(&dir.join("data/infra-monitor.json"), None, None),
            true,
        )
        .await
        .unwrap_err();
        assert_eq!(
            (status, e.code.as_str()),
            (StatusCode::FORBIDDEN, "PROTECTED_PATH")
        );

        // Not refusals: answered as before, with no entry.
        let (status, e) = read(&state, query(&dir.join("absent"), None, None), false)
            .await
            .unwrap_err();
        assert_eq!(
            (status, e.code.as_str()),
            (StatusCode::NOT_FOUND, "FILE_NOT_FOUND")
        );
        let (status, e) = read(&state, query(&dir, None, None), false)
            .await
            .unwrap_err();
        assert_eq!(
            (status, e.code.as_str()),
            (StatusCode::BAD_REQUEST, "IS_DIRECTORY")
        );

        let entries = refusals(&state).await;
        assert_eq!(entries.len(), 3, "{entries:?}");
        assert!(entries
            .iter()
            .all(|e| e.activity_type == ActivityType::FileRead));
        assert_eq!(
            entries[0].detail.as_ref().unwrap()["rule"],
            "a process's environment"
        );
        assert_eq!(
            entries[1].detail.as_ref().unwrap()["code"],
            "NOT_A_REGULAR_FILE"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn the_neighbour_table_reads_to_its_end() {
        let dir = scratch("arp");
        let state = state_in(&dir);
        for via_tunnel in [false, true] {
            let v = read(
                &state,
                query(Path::new("/proc/net/arp"), None, None),
                via_tunnel,
            )
            .await
            .unwrap();
            let content = v["content"].as_str().unwrap();
            assert!(content.starts_with("IP address"), "{content}");
            assert_eq!(v["size"].as_u64().unwrap(), content.len() as u64);
            assert!(v.get("truncated").is_none());
            assert_eq!(v["path"], "/proc/net/arp");
        }
        let v = read(
            &state,
            query(Path::new("/sys/class/net/lo/address"), None, None),
            false,
        )
        .await
        .unwrap();
        assert_eq!(v["content"], "00:00:00:00:00:00\n");
        assert_eq!(v["size"], 18);
        assert!(v.get("truncated").is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn through_the_tunnel_a_read_stays_within_half_its_body() {
        let dir = scratch("relay");
        let state = state_in(&dir);
        let big = dir.join("big.txt");
        std::fs::write(&big, vec![b'a'; 5 * 1024 * 1024]).unwrap();

        // No range: refused before it is read.
        let (status, e) = read(&state, query(&big, None, None), true)
            .await
            .unwrap_err();
        assert_eq!(
            (status, e.code.as_str()),
            (StatusCode::BAD_REQUEST, "FILE_TOO_LARGE")
        );
        assert!(e.message.contains("through the relay"), "{}", e.message);

        // A 1 MiB limit reads.
        let v = read(&state, query(&big, None, Some(1024 * 1024)), true)
            .await
            .unwrap();
        assert_eq!(v["content"].as_str().unwrap().len(), 1024 * 1024);
        assert_eq!(v["size"], 5 * 1024 * 1024);
        assert_eq!(v["truncated"], true);

        // A range with no limit is cut at the tunnel's cap.
        let v = read(&state, query(&big, Some(10), None), true)
            .await
            .unwrap();
        assert_eq!(v["content"].as_str().unwrap().len(), TUNNEL_READ_CAP);
        assert_eq!(v["truncated"], true);

        // On the LAN port the whole file reads.
        let v = read(&state, query(&big, None, None), false).await.unwrap();
        assert_eq!(v["content"].as_str().unwrap().len(), 5 * 1024 * 1024);
        assert!(v.get("truncated").is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn an_answer_that_would_not_fit_the_tunnel_is_too_large_not_a_502() {
        let dir = scratch("escape");
        let state = state_in(&dir);
        // Valid UTF-8 whose every byte JSON writes as six: 1.5 MiB in,
        // 9 MiB out.
        let file = dir.join("controls.txt");
        std::fs::write(&file, vec![1u8; 3 * 512 * 1024]).unwrap();
        let (status, e) = read(&state, query(&file, None, None), true)
            .await
            .unwrap_err();
        assert_eq!(
            (status, e.code.as_str()),
            (StatusCode::BAD_REQUEST, "FILE_TOO_LARGE")
        );
        // Off the tunnel it reads.
        assert!(read(&state, query(&file, None, None), false).await.is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn writes_and_listings_keep_their_rules() {
        let dir = scratch("writes");
        let state = state_in(&dir);
        let data = dir.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let script = data.join("watchdog.sh");

        // A write into the agent's own directory still works.
        let written = put_file(
            State(state.clone()),
            HeaderMap::new(),
            Json(FileWriteRequest {
                path: script.display().to_string(),
                content: "#!/bin/sh\n".to_string(),
                create_dirs: false,
                mode: Some("0755".to_string()),
                encoding: None,
            }),
        )
        .await;
        assert!(written.is_ok());
        assert_eq!(std::fs::read(&script).unwrap(), b"#!/bin/sh\n");

        // So does a listing; the read of the file it lists does not.
        let mut q = query(&data, None, None);
        q.list = true;
        let v = read(&state, q, false).await.unwrap();
        assert_eq!(v["entries"][0]["name"], "watchdog.sh");
        let (status, e) = read(&state, query(&script, None, None), false)
            .await
            .unwrap_err();
        assert_eq!(
            (status, e.code.as_str()),
            (StatusCode::FORBIDDEN, "PROTECTED_PATH")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    async fn download(state: &AppState, path: &Path) -> Result<Response, (StatusCode, ApiError)> {
        download_file(
            State(state.clone()),
            HeaderMap::new(),
            Query(DownloadQuery {
                path: path.display().to_string(),
            }),
        )
        .await
        .map_err(|(status, Json(e))| (status, e))
    }

    #[tokio::test]
    async fn raw_sends_exactly_the_size_its_stat_gave() {
        let dir = scratch("raw");
        let state = state_in(&dir);
        let log = dir.join("growing.log");
        std::fs::write(&log, vec![b'x'; 100_000]).unwrap();

        let resp = download(&state, &log).await.unwrap();
        assert_eq!(resp.headers()["content-length"], "100000");
        // The file grows while the body is still to be sent.
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        std::io::Write::write_all(&mut f, &vec![b'y'; 50_000]).unwrap();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(body.len(), 100_000);
        assert!(body.iter().all(|b| *b == b'x'));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn raw_moves_no_pseudo_file_and_no_device() {
        let dir = scratch("raw-refused");
        let state = state_in(&dir);
        let (status, e) = download(&state, Path::new("/proc/net/arp"))
            .await
            .unwrap_err();
        assert_eq!(
            (status, e.code.as_str()),
            (StatusCode::BAD_REQUEST, "NOT_A_REGULAR_FILE")
        );
        assert!(e.message.contains("procfs"), "{}", e.message);
        let (status, e) = download(&state, Path::new("/dev/zero")).await.unwrap_err();
        assert_eq!(
            (status, e.code.as_str()),
            (StatusCode::BAD_REQUEST, "NOT_A_REGULAR_FILE")
        );
        let (status, e) = download(&state, Path::new("/proc/self/environ"))
            .await
            .unwrap_err();
        assert_eq!(
            (status, e.code.as_str()),
            (StatusCode::FORBIDDEN, "PROTECTED_PATH")
        );
        let (status, e) = download(&state, &dir).await.unwrap_err();
        assert_eq!(
            (status, e.code.as_str()),
            (StatusCode::BAD_REQUEST, "IS_DIRECTORY")
        );
        assert_eq!(refusals(&state).await.len(), 3);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
