//! Release bundles on the relay: `<data_dir>/artifacts/<version>/`, served
//! to the relay's own devices. `docs/upgrade.md` "Artifacts on the relay".
//!
//! A bundle is accepted in order: `release.json` and `release.json.sig`
//! first (verified against the same keys the agents embed, so a bundle no
//! device would accept is refused at the door), then the files the manifest
//! names, each checked against its SHA-256 as it lands. Every change
//! publishes `artifacts.changed` on the event stream.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::RwLock;
use tracing::{info, warn};

use super::relay::RelayState;
use crate::upgrade::manifest::{is_valid_file_name, verify, Manifest};
use crate::upgrade::version::Version;

/// The largest file a bundle may hold.
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// One version's bundle as the relay holds it.
#[derive(Clone, Debug)]
pub struct Bundle {
    pub version: String,
    pub manifest: Option<Manifest>,
    pub verified: bool,
    pub files: Vec<String>,
    pub mirror: Option<String>,
}

impl Bundle {
    /// The files the manifest names that are not here yet.
    pub fn missing(&self) -> Vec<String> {
        self.manifest
            .as_ref()
            .map(|m| {
                m.expected_files()
                    .into_iter()
                    .filter(|n| !self.files.contains(n))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The listing entry.
    pub fn summary(&self) -> Value {
        let missing = self.missing();
        json!({
            "version": self.version,
            "verified": self.verified,
            "complete": self.verified && missing.is_empty(),
            "channel": self.manifest.as_ref().map(|m| m.channel.clone()),
            "targets": self.manifest.as_ref().map(|m| m.targets.keys().cloned().collect::<Vec<_>>()),
            "published_at": self.manifest.as_ref().and_then(|m| m.published_at.clone()),
            "min_from_version": self.manifest.as_ref().and_then(|m| m.min_from_version.clone()),
            "notes": self.manifest.as_ref().and_then(|m| m.notes.clone()),
            "files": self.files,
            "missing": missing,
            "mirror": self.mirror,
        })
    }
}

/// The store: a directory per version, scanned at start and kept in memory.
pub struct ArtifactStore {
    root: Option<PathBuf>,
    bundles: RwLock<BTreeMap<String, Bundle>>,
    keys: Vec<[u8; 32]>,
}

impl ArtifactStore {
    /// `root` is `<data_dir>/artifacts`; `None` disables uploads.
    pub fn new(root: Option<PathBuf>, keys: Vec<[u8; 32]>) -> Self {
        let mut bundles = BTreeMap::new();
        if let Some(root) = &root {
            if let Ok(entries) = std::fs::read_dir(root) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if entry.path().is_dir() && Version::parse(&name).is_some() {
                        if let Some(bundle) = Self::scan(&entry.path(), &name, &keys) {
                            bundles.insert(name, bundle);
                        }
                    }
                }
            }
            if !bundles.is_empty() {
                info!(
                    versions = bundles.len(),
                    "Artifacts: loaded from {}",
                    root.display()
                );
            }
        }
        Self {
            root,
            bundles: RwLock::new(bundles),
            keys,
        }
    }

    /// Whether the relay can hold bundles.
    pub fn enabled(&self) -> bool {
        self.root.is_some()
    }

    /// Where bundles live.
    pub fn root_dir(&self) -> Option<PathBuf> {
        self.root.clone()
    }

    fn dir(&self, version: &str) -> Option<PathBuf> {
        self.root.as_ref().map(|r| r.join(version))
    }

    fn scan(dir: &Path, version: &str, keys: &[[u8; 32]]) -> Option<Bundle> {
        let mut files: Vec<String> = std::fs::read_dir(dir)
            .ok()?
            .flatten()
            .filter(|e| e.path().is_file())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "mirror.txt")
            .collect();
        files.sort();
        let manifest_bytes = std::fs::read(dir.join("release.json")).ok();
        let sig = std::fs::read_to_string(dir.join("release.json.sig")).ok();
        let (manifest, verified) = match (&manifest_bytes, &sig) {
            (Some(bytes), Some(sig)) => match Manifest::parse_signed(bytes, sig, keys) {
                Ok(m) if m.version == version => (Some(m), true),
                Ok(m) => {
                    warn!(dir = %dir.display(), "Artifacts: manifest says {}, directory says {version}", m.version);
                    (Some(m), false)
                }
                Err(e) => {
                    warn!(dir = %dir.display(), "Artifacts: {e}");
                    (Manifest::parse(bytes).ok(), false)
                }
            },
            (Some(bytes), None) => (Manifest::parse(bytes).ok(), false),
            _ => (None, false),
        };
        let mirror = std::fs::read_to_string(dir.join("mirror.txt"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        Some(Bundle {
            version: version.to_string(),
            manifest,
            verified,
            files,
            mirror,
        })
    }

    /// Every bundle's summary, newest version first.
    pub async fn list(&self) -> Vec<Value> {
        let bundles = self.bundles.read().await;
        let mut all: Vec<&Bundle> = bundles.values().collect();
        all.sort_by(|a, b| Version::parse(&b.version).cmp(&Version::parse(&a.version)));
        all.iter().map(|b| b.summary()).collect()
    }

    /// The `artifacts` frame (replay) or `artifacts.changed` (live).
    pub async fn frame(&self, replay: bool) -> Value {
        json!({
            "type": if replay { "artifacts" } else { "artifacts.changed" },
            "versions": self.list().await,
            "replay": replay,
        })
    }

    async fn rescan(&self, version: &str) {
        let Some(dir) = self.dir(version) else { return };
        let mut bundles = self.bundles.write().await;
        match Self::scan(&dir, version, &self.keys) {
            Some(b) if !b.files.is_empty() => {
                bundles.insert(version.to_string(), b);
            }
            _ => {
                bundles.remove(version);
            }
        }
    }

    /// The bundle for `version`, if any.
    pub async fn bundle(&self, version: &str) -> Option<Bundle> {
        self.bundles.read().await.get(version).cloned()
    }
}

fn error(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    let message = message.into();
    (
        status,
        Json(json!({"error": message, "code": code, "message": message})),
    )
        .into_response()
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
}

/// Operator auth: the relay's own `api_key`.
fn operator(state: &RelayState, headers: &HeaderMap) -> Result<(), Box<Response>> {
    let provided = bearer(headers).unwrap_or_default();
    if state.operator_key.is_empty()
        || !crate::auth::constant_time_eq(state.operator_key.as_bytes(), provided.as_bytes())
    {
        return Err(Box::new(error(
            StatusCode::FORBIDDEN,
            "AUTH_INVALID_TOKEN",
            "Invalid API key",
        )));
    }
    Ok(())
}

/// Reader auth: the tunnel key (every device) or the operator key.
fn reader(state: &RelayState, headers: &HeaderMap) -> Result<(), Box<Response>> {
    let provided = bearer(headers).unwrap_or_default();
    let tunnel = crate::auth::constant_time_eq(state.tunnel_key.as_bytes(), provided.as_bytes());
    if tunnel || operator(state, headers).is_ok() {
        Ok(())
    } else {
        Err(Box::new(error(
            StatusCode::FORBIDDEN,
            "AUTH_INVALID_TOKEN",
            "Invalid key",
        )))
    }
}

fn valid_version(version: &str) -> bool {
    is_valid_file_name(version) && Version::parse(version).is_some()
}

/// `GET /api/tunnel/artifacts`.
pub async fn list(State(state): State<RelayState>, headers: HeaderMap) -> Response {
    if let Err(r) = reader(&state, &headers) {
        return *r;
    }
    Json(json!({"versions": state.artifacts.list().await, "enabled": state.artifacts.enabled()}))
        .into_response()
}

/// `PUT /api/tunnel/artifacts/{version}/{file}`.
pub async fn put_file(
    State(state): State<RelayState>,
    AxumPath((version, file)): AxumPath<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if let Err(r) = operator(&state, &headers) {
        return *r;
    }
    let store: &Arc<ArtifactStore> = &state.artifacts;
    let Some(dir) = store.dir(&version) else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "NO_DATA_DIR",
            "the relay has no data_dir to hold artifacts",
        );
    };
    if !valid_version(&version) {
        return error(
            StatusCode::BAD_REQUEST,
            "INVALID_VERSION",
            format!("'{version}' is not a version"),
        );
    }
    if !is_valid_file_name(&file) || file == "mirror.txt" {
        return error(
            StatusCode::BAD_REQUEST,
            "INVALID_FILE",
            format!("'{file}' is not a bundle file name"),
        );
    }
    let is_manifest = file == "release.json";
    let is_sig = file == "release.json.sig";
    let bundle = store.bundle(&version).await;
    if !is_manifest && !is_sig && !bundle.as_ref().is_some_and(|b| b.verified) {
        return error(
            StatusCode::CONFLICT,
            "MANIFEST_FIRST",
            "upload release.json and release.json.sig first",
        );
    }
    let expected = if is_manifest || is_sig {
        None
    } else {
        let Some(entry) = bundle
            .as_ref()
            .and_then(|b| b.manifest.as_ref())
            .and_then(|m| m.file_named(&file).cloned())
        else {
            return error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "NOT_IN_MANIFEST",
                format!("'{file}' is not in release {version}'s manifest"),
            );
        };
        Some(entry)
    };
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "IO",
            format!("{}: {e}", dir.display()),
        );
    }
    let target = dir.join(&file);
    let tmp = dir.join(format!(".{file}.upload"));
    let mut out = match tokio::fs::File::create(&tmp).await {
        Ok(f) => f,
        Err(e) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "IO",
                format!("{}: {e}", tmp.display()),
            )
        }
    };
    let mut written: u64 = 0;
    let limit = expected.as_ref().map_or(MAX_FILE_BYTES, |e| e.size);
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                return error(StatusCode::BAD_REQUEST, "BODY", e.to_string());
            }
        };
        written += chunk.len() as u64;
        if written > limit {
            let _ = tokio::fs::remove_file(&tmp).await;
            return error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "TOO_LARGE",
                format!("'{file}' is over {limit} bytes"),
            );
        }
        if let Err(e) = out.write_all(&chunk).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return error(StatusCode::INTERNAL_SERVER_ERROR, "IO", e.to_string());
        }
    }
    if let Err(e) = out.sync_all().await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return error(StatusCode::INTERNAL_SERVER_ERROR, "IO", e.to_string());
    }
    drop(out);
    // Check before it lands.
    if let Some(entry) = &expected {
        let hashed = tokio::task::spawn_blocking({
            let tmp = tmp.clone();
            move || crate::upgrade::sha256_file(&tmp)
        })
        .await;
        let Ok(Ok(actual)) = hashed else {
            let _ = tokio::fs::remove_file(&tmp).await;
            return error(StatusCode::INTERNAL_SERVER_ERROR, "IO", "hashing failed");
        };
        if actual != entry.sha256 || written != entry.size {
            let _ = tokio::fs::remove_file(&tmp).await;
            return error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "SHA256_MISMATCH",
                format!(
                    "'{file}': got {actual} ({written} bytes), the manifest says {} ({} bytes)",
                    entry.sha256, entry.size
                ),
            );
        }
    } else if is_manifest {
        let bytes = tokio::fs::read(&tmp).await.unwrap_or_default();
        match Manifest::parse(&bytes) {
            Ok(m) if m.version != version => {
                let _ = tokio::fs::remove_file(&tmp).await;
                return error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "VERSION_MISMATCH",
                    format!("the manifest is for {}, the path says {version}", m.version),
                );
            }
            Ok(_) => {}
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                return error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "MANIFEST_INVALID",
                    e.to_string(),
                );
            }
        }
        if let Ok(sig) = tokio::fs::read_to_string(dir.join("release.json.sig")).await {
            if let Err(e) = verify(&bytes, &sig, &store.keys) {
                let _ = tokio::fs::remove_file(&tmp).await;
                return error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "MANIFEST_BAD_SIGNATURE",
                    e.to_string(),
                );
            }
        }
    } else if is_sig {
        let sig = tokio::fs::read_to_string(&tmp).await.unwrap_or_default();
        if let Ok(bytes) = tokio::fs::read(dir.join("release.json")).await {
            if let Err(e) = verify(&bytes, &sig, &store.keys) {
                let _ = tokio::fs::remove_file(&tmp).await;
                return error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "MANIFEST_BAD_SIGNATURE",
                    e.to_string(),
                );
            }
        } else if crate::upgrade::manifest::parse_signature(&sig).is_err() {
            let _ = tokio::fs::remove_file(&tmp).await;
            return error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "MANIFEST_UNSIGNED",
                "not an ed25519 signature line",
            );
        }
    }
    if let Err(e) = tokio::fs::rename(&tmp, &target).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return error(StatusCode::INTERNAL_SERVER_ERROR, "IO", e.to_string());
    }
    store.rescan(&version).await;
    let bundle = store.bundle(&version).await;
    state.publish(&store.frame(false).await);
    info!(version, file, bytes = written, "Artifacts: stored");
    (
        StatusCode::OK,
        Json(json!({"stored": file, "bytes": written, "bundle": bundle.map(|b| b.summary())})),
    )
        .into_response()
}

/// `PUT /api/tunnel/artifacts/{version}/mirror` with `{"url": "..."}`.
pub async fn put_mirror(
    State(state): State<RelayState>,
    AxumPath(version): AxumPath<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Err(r) = operator(&state, &headers) {
        return *r;
    }
    let store = &state.artifacts;
    let Some(dir) = store.dir(&version) else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "NO_DATA_DIR",
            "the relay has no data_dir to hold artifacts",
        );
    };
    if store.bundle(&version).await.is_none() {
        return error(
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            format!("no bundle {version}"),
        );
    }
    let url = body["url"].as_str().unwrap_or("").trim().to_string();
    if !url.is_empty() && !url.starts_with("http://") && !url.starts_with("https://") {
        return error(
            StatusCode::BAD_REQUEST,
            "INVALID_URL",
            "url must be http(s)://",
        );
    }
    let path = dir.join("mirror.txt");
    let result = if url.is_empty() {
        tokio::fs::remove_file(&path).await.or_else(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Ok(())
            } else {
                Err(e)
            }
        })
    } else {
        tokio::fs::write(&path, format!("{url}\n")).await
    };
    if let Err(e) = result {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "IO", e.to_string());
    }
    store.rescan(&version).await;
    state.publish(&store.frame(false).await);
    Json(json!({"version": version, "mirror": (!url.is_empty()).then_some(url)})).into_response()
}

/// `DELETE /api/tunnel/artifacts/{version}`.
pub async fn delete_version(
    State(state): State<RelayState>,
    AxumPath(version): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = operator(&state, &headers) {
        return *r;
    }
    let store = &state.artifacts;
    let Some(dir) = store.dir(&version) else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "NO_DATA_DIR",
            "the relay has no data_dir to hold artifacts",
        );
    };
    if !valid_version(&version) || store.bundle(&version).await.is_none() {
        return error(
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            format!("no bundle {version}"),
        );
    }
    if let Err(e) = tokio::fs::remove_dir_all(&dir).await {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "IO", e.to_string());
    }
    store.rescan(&version).await;
    state.publish(&store.frame(false).await);
    info!(version, "Artifacts: removed");
    Json(json!({"removed": version})).into_response()
}

/// A single `bytes=start-[end]` range, clipped to `len`.
fn parse_range(header: Option<&str>, len: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(h) = header else { return Ok(None) };
    let spec = h.strip_prefix("bytes=").ok_or(())?;
    let (start, end) = spec.split_once('-').ok_or(())?;
    if start.is_empty() {
        // A suffix range: the last N bytes.
        let n: u64 = end.parse().map_err(|_| ())?;
        if n == 0 || len == 0 {
            return Err(());
        }
        return Ok(Some((len.saturating_sub(n), len - 1)));
    }
    let start: u64 = start.parse().map_err(|_| ())?;
    let end: u64 = if end.is_empty() {
        len.saturating_sub(1)
    } else {
        end.parse().map_err(|_| ())?
    };
    if start >= len || end < start {
        return Err(());
    }
    Ok(Some((start, end.min(len - 1))))
}

/// `GET /api/tunnel/artifacts/{version}/{file}`, with `Range`.
pub async fn get_file(
    State(state): State<RelayState>,
    AxumPath((version, file)): AxumPath<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = reader(&state, &headers) {
        return *r;
    }
    let store = &state.artifacts;
    if !valid_version(&version) || !is_valid_file_name(&file) || file == "mirror.txt" {
        return error(StatusCode::NOT_FOUND, "NOT_FOUND", "no such artifact");
    }
    let Some(dir) = store.dir(&version) else {
        return error(StatusCode::NOT_FOUND, "NOT_FOUND", "no such artifact");
    };
    let Some(bundle) = store.bundle(&version).await else {
        return error(
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            format!("no bundle {version}"),
        );
    };
    if !bundle.files.contains(&file) {
        return error(
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            format!("no file {file} in {version}"),
        );
    }
    let path = dir.join(&file);
    let Ok(mut f) = tokio::fs::File::open(&path).await else {
        return error(StatusCode::NOT_FOUND, "NOT_FOUND", "no such artifact");
    };
    let len = f.metadata().await.map_or(0, |m| m.len());
    let etag = bundle
        .manifest
        .as_ref()
        .and_then(|m| m.file_named(&file))
        .map(|e| format!("\"{}\"", e.sha256));
    let mut range = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    // `If-Range` names the file the client started; another file under the
    // same name is served whole.
    if let Some(if_range) = headers.get(header::IF_RANGE).and_then(|v| v.to_str().ok()) {
        if etag.as_deref() != Some(if_range.trim()) {
            range = None;
        }
    }
    let (status, start, end) = match parse_range(range, len) {
        Ok(Some((s, e))) => (StatusCode::PARTIAL_CONTENT, s, e),
        Ok(None) => (StatusCode::OK, 0, len.saturating_sub(1)),
        Err(()) => {
            return Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{len}"))
                .body(Body::empty())
                .unwrap_or_else(|_| StatusCode::RANGE_NOT_SATISFIABLE.into_response());
        }
    };
    let count = if len == 0 { 0 } else { end - start + 1 };
    if start > 0 && f.seek(std::io::SeekFrom::Start(start)).await.is_err() {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "IO", "seek failed");
    }
    let stream = tokio_util::io::ReaderStream::with_capacity(
        tokio::io::AsyncReadExt::take(f, count),
        64 * 1024,
    );
    let mut response = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, count.to_string())
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CACHE_CONTROL, "no-cache");
    if status == StatusCode::PARTIAL_CONTENT {
        response = response.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}"));
    }
    if let Some(etag) = etag {
        response = response.header(header::ETAG, etag);
    }
    response
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_parse_and_clip() {
        assert_eq!(parse_range(None, 100), Ok(None));
        assert_eq!(parse_range(Some("bytes=0-"), 100), Ok(Some((0, 99))));
        assert_eq!(parse_range(Some("bytes=50-"), 100), Ok(Some((50, 99))));
        assert_eq!(parse_range(Some("bytes=10-20"), 100), Ok(Some((10, 20))));
        assert_eq!(parse_range(Some("bytes=10-200"), 100), Ok(Some((10, 99))));
        assert_eq!(parse_range(Some("bytes=-10"), 100), Ok(Some((90, 99))));
        assert_eq!(parse_range(Some("bytes=100-"), 100), Err(()));
        assert_eq!(parse_range(Some("bytes=20-10"), 100), Err(()));
        assert_eq!(parse_range(Some("items=1-2"), 100), Err(()));
    }

    #[test]
    fn a_bundle_knows_what_is_missing() {
        let bytes = crate::upgrade::manifest::testing::manifest_json(
            "0.6.7.1",
            "x86_64",
            "sctl-x86_64",
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad", // secret-scan: allow
            3,
        );
        let manifest = Manifest::parse(bytes.as_bytes()).unwrap();
        let bundle = Bundle {
            version: "0.6.7.1".into(),
            manifest: Some(manifest),
            verified: true,
            files: vec!["release.json".into(), "release.json.sig".into()],
            mirror: None,
        };
        assert_eq!(bundle.missing(), vec!["sctl-x86_64"]);
        assert_eq!(bundle.summary()["complete"], false);
    }
}
