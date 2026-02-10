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
//! ## Size limits
//!
//! Reads and writes are capped at `server.max_file_size` (default 2 MB).
//! Binary files are returned/accepted with base64 encoding.
//!
//! ## Atomicity
//!
//! File writes use a temp-file-then-rename pattern. On the same filesystem this
//! is atomic — readers never see a partially-written file. Cross-filesystem
//! renames will fail.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

/// Monotonic counter to uniquify temp file names across concurrent writes.
static WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::activity::{self, ActivityType};
use crate::AppState;

/// Query parameters for `GET /api/files`.
#[derive(Deserialize)]
pub struct FilesQuery {
    /// Absolute path to the file or directory.
    pub path: String,
    /// When `true` (or when `path` ends with `/`), list directory contents
    /// instead of reading a file.
    #[serde(default)]
    pub list: bool,
}

/// JSON response for a successful file read.
#[derive(Serialize)]
pub struct FileReadResponse {
    /// Canonical path that was read.
    pub path: String,
    /// File contents — UTF-8 text, or base64 if binary (see `encoding`).
    pub content: String,
    /// File size in bytes.
    pub size: u64,
    /// Last-modified time as a Unix timestamp (seconds since epoch).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
    /// `"base64"` for binary files, absent for UTF-8 text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
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
    /// Last-modified time as a Unix timestamp string.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
    /// For symlinks, the target path. Absent for other types.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symlink_target: Option<String>,
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
fn validate_path(path: &str) -> Result<PathBuf, (StatusCode, Json<Value>)> {
    let p = Path::new(path);
    if !p.is_absolute() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Path must be absolute", "code": "INVALID_PATH"})),
        ));
    }
    if path.contains('\0') {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Path contains null bytes", "code": "INVALID_PATH"})),
        ));
    }
    for component in p.components() {
        if let std::path::Component::ParentDir = component {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Path traversal (..) not allowed", "code": "INVALID_PATH"})),
            ));
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
/// # Error codes
///
/// | HTTP | Code               | Meaning                          |
/// |------|--------------------|----------------------------------|
/// | 400  | `INVALID_PATH`     | Path is relative, has `..`, etc. |
/// | 400  | `IS_DIRECTORY`     | Path is a dir but `list` is off  |
/// | 400  | `FILE_TOO_LARGE`   | File exceeds `max_file_size`     |
/// | 403  | `PERMISSION_DENIED`| OS permission error              |
/// | 404  | `FILE_NOT_FOUND`   | File or directory does not exist |
/// | 500  | `IO_ERROR`         | Other I/O failure                |
pub async fn get_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<FilesQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let source = activity::source_from_headers(&headers);
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
            )
            .await;
        return Ok(result);
    }

    let result = read_file(&path, state.config.server.max_file_size).await?;
    state
        .activity_log
        .log(
            ActivityType::FileRead,
            source,
            activity::truncate_str(&query.path, 80),
            None,
        )
        .await;
    Ok(result)
}

/// Read a single file, returning UTF-8 text or base64 for binary.
async fn read_file(path: &Path, max_size: usize) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let metadata = match tokio::fs::metadata(path).await {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(json!({"error": "File not found", "code": "FILE_NOT_FOUND"})),
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err((
                StatusCode::FORBIDDEN,
                Json(json!({"error": "Permission denied", "code": "PERMISSION_DENIED"})),
            ));
        }
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string(), "code": "IO_ERROR"})),
            ));
        }
    };

    if metadata.is_dir() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Path is a directory, use list=true", "code": "IS_DIRECTORY"})),
        ));
    }

    #[allow(clippy::cast_possible_truncation)]
    if metadata.len() as usize > max_size {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("File too large ({} bytes, max {})", metadata.len(), max_size),
                "code": "FILE_TOO_LARGE"
            })),
        ));
    }

    let modified = metadata.modified().ok().and_then(format_system_time);

    let bytes = match tokio::fs::read(path).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err((
                StatusCode::FORBIDDEN,
                Json(json!({"error": "Permission denied", "code": "PERMISSION_DENIED"})),
            ));
        }
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string(), "code": "IO_ERROR"})),
            ));
        }
    };

    // Try to interpret as UTF-8; fall back to base64 for binary files.
    // Use from_utf8 on the slice to avoid cloning up to 2 MB.
    let path_str = path.to_string_lossy().into_owned();
    if std::str::from_utf8(&bytes).is_ok() {
        // SAFETY: we just validated UTF-8 above.
        let text = unsafe { String::from_utf8_unchecked(bytes) };
        Ok(Json(
            serde_json::to_value(FileReadResponse {
                path: path_str,
                content: text,
                size: metadata.len(),
                modified,
                encoding: None,
            })
            .unwrap(),
        ))
    } else {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        Ok(Json(
            serde_json::to_value(FileReadResponse {
                path: path_str,
                content: encoded,
                size: metadata.len(),
                modified,
                encoding: Some("base64".to_string()),
            })
            .unwrap(),
        ))
    }
}

/// List a directory's contents, sorted by name.
async fn list_directory(path: &Path) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let mut read_dir = match tokio::fs::read_dir(path).await {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(json!({"error": "Directory not found", "code": "FILE_NOT_FOUND"})),
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err((
                StatusCode::FORBIDDEN,
                Json(json!({"error": "Permission denied", "code": "PERMISSION_DENIED"})),
            ));
        }
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string(), "code": "IO_ERROR"})),
            ));
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
        let modified = metadata
            .as_ref()
            .and_then(|m: &std::fs::Metadata| m.modified().ok())
            .and_then(format_system_time);

        entries.push(DirEntry {
            name,
            entry_type,
            size,
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
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let source = activity::source_from_headers(&headers);
    let path = validate_path(&payload.path)?;

    let bytes = if payload.encoding.as_deref() == Some("base64") {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(&payload.content)
            .map_err(|e| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(
                        json!({"error": format!("Invalid base64: {e}"), "code": "INVALID_CONTENT"}),
                    ),
                )
            })?
    } else {
        payload.content.into_bytes()
    };

    if bytes.len() > state.config.server.max_file_size {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("Content too large ({} bytes, max {})", bytes.len(), state.config.server.max_file_size),
                "code": "FILE_TOO_LARGE"
            })),
        ));
    }

    if payload.create_dirs {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e: std::io::Error| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": e.to_string(), "code": "IO_ERROR"})),
                    )
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
                (
                    StatusCode::FORBIDDEN,
                    Json(json!({"error": "Permission denied", "code": "PERMISSION_DENIED"})),
                )
            } else {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": e.to_string(), "code": "IO_ERROR"})),
                )
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
            (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!("Invalid octal mode: {mode_str:?}"),
                    "code": "INVALID_MODE"
                })),
            )
        })?;
        let perms = std::fs::Permissions::from_mode(mode);
        if let Err(e) = tokio::fs::set_permissions(&temp_path, perms).await {
            let _ = tokio::fs::remove_file(&temp_path).await;
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to set mode: {e}"), "code": "IO_ERROR"})),
            ));
        }
    }

    rename_temp_to_final(&temp_path, &path).await?;
    log_file_write(
        &state,
        source,
        &payload.path,
        bytes.len(),
        payload.mode.as_ref(),
    )
    .await;

    Ok(Json(json!({
        "path": path.to_string_lossy(),
        "size": bytes.len(),
        "ok": true
    })))
}

async fn rename_temp_to_final(
    temp_path: &Path,
    final_path: &Path,
) -> Result<(), (StatusCode, Json<Value>)> {
    tokio::fs::rename(temp_path, final_path).await.map_err(|e| {
        let tp = temp_path.to_path_buf();
        tokio::spawn(async move {
            let _ = tokio::fs::remove_file(&tp).await;
        });
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("Failed to rename: {e}"), "code": "IO_ERROR"})),
        )
    })
}

async fn log_file_write(
    state: &AppState,
    source: activity::ActivitySource,
    path: &str,
    size: usize,
    mode: Option<&String>,
) {
    state
        .activity_log
        .log(
            ActivityType::FileWrite,
            source,
            activity::truncate_str(path, 80),
            Some(json!({ "size": size, "mode": mode })),
        )
        .await;
}
