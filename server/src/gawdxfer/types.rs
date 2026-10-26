//! Protocol types for the gawdxfer chunked resumable file transfer protocol.
//!
//! All message types are plain data structs with serde support. The module knows
//! nothing about HTTP, `WebSockets`, or axum — integration layers adapt these types
//! to their transport.

use std::path::PathBuf;
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// Transfer direction from the device's perspective.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, optional_fields))]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Upload,
    Download,
}

/// Transfer lifecycle phase.
/// Immutable metadata set at transfer init time.
pub struct TransferSpec {
    pub transfer_id: String,
    pub direction: Direction,
    pub path: PathBuf,
    pub filename: String,
    pub file_size: u64,
    pub file_hash: String,
    pub chunk_size: u32,
    pub total_chunks: u32,
    pub mode: Option<String>,
    pub created_at: Instant,
    /// Source file mtime at init (download only) — detect `FILE_CHANGED`.
    pub source_mtime: Option<u64>,
    /// The file a download hashed (download only): every chunk is read
    /// from it or from nothing (`FILE_CHANGED`).
    pub source_file: Option<crate::file_access::FileId>,
}

/// Mutable progress state for a transfer.
pub struct TransferProgress {
    pub phase: Phase,
    pub chunks_done: Vec<bool>,
    pub bytes_transferred: u64,
    pub last_activity: Instant,
    pub temp_path: PathBuf,
    pub error_count: u32,
}

// ─── Protocol Request/Response Messages ──────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
// Not dead — `Progress.phase` and the manager's state machine live on this.
// The allow silences UNCONSTRUCTED VARIANTS (Paused/Verifying are parsed and
// matched but never built by current code paths), which dead_code also flags.
#[allow(dead_code)]
pub enum Phase {
    Init,
    Transferring,
    Paused,
    Verifying,
    Complete,
    Failed(String),
    Aborted,
}

#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, optional_fields))]
pub struct InitDownload {
    pub path: String,
    // The chunk size asked for. Any whole number is taken, one above
    // `u32::MAX` as `u32::MAX`: the init answers every size above the
    // ceiling as the ceiling, never refusing it (TRD-9 3.1).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "asked_chunk_size"
    )]
    #[cfg_attr(test, ts(as = "Option<u32>"))]
    pub chunk_size: Option<u32>,
}

/// A download's `chunk_size` as asked, held in a `u32`: a size above
/// `u32::MAX` is `u32::MAX`, still above the ceiling the init answers it
/// with, never a smaller size that a narrowing cast would give.
pub fn saturating_chunk_size(asked: u64) -> u32 {
    u32::try_from(asked).unwrap_or(u32::MAX)
}

/// [`InitDownload::chunk_size`] on the wire: read as a whole number of any
/// size and saturated ([`saturating_chunk_size`]), written as it is.
mod asked_chunk_size {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    // serde's `with` takes the field by reference.
    #[allow(clippy::ref_option, clippy::trivially_copy_pass_by_ref)]
    pub fn serialize<S: Serializer>(value: &Option<u32>, serializer: S) -> Result<S::Ok, S::Error> {
        value.serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<u32>, D::Error> {
        Ok(Option::<u64>::deserialize(deserializer)?.map(super::saturating_chunk_size))
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, optional_fields))]
pub struct InitDownloadResult {
    pub transfer_id: String,
    pub file_size: u64,
    pub file_hash: String,
    pub chunk_size: u32,
    pub total_chunks: u32,
    pub filename: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, optional_fields))]
pub struct InitUpload {
    pub path: String,
    pub filename: String,
    pub file_size: u64,
    /// Whole-file SHA-256 hash. If empty, the server computes it after all chunks are received.
    #[serde(default)]
    pub file_hash: String,
    pub chunk_size: u32,
    pub total_chunks: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, optional_fields))]
pub struct InitUploadResult {
    pub transfer_id: String,
    pub chunk_size: u32,
    pub total_chunks: u32,
}

#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, optional_fields))]
pub struct ChunkHeader {
    pub transfer_id: String,
    pub chunk_index: u32,
    pub chunk_hash: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, optional_fields))]
pub struct ChunkAck {
    pub transfer_id: String,
    pub chunk_index: u32,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, optional_fields))]
pub struct Progress {
    pub transfer_id: String,
    pub direction: Direction,
    pub path: String,
    pub filename: String,
    pub chunks_done: u32,
    pub total_chunks: u32,
    pub bytes_transferred: u64,
    pub file_size: u64,
    pub elapsed_ms: u64,
    pub rate_bps: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, optional_fields))]
pub struct Complete {
    pub transfer_id: String,
    pub direction: Direction,
    pub path: String,
    pub filename: String,
    pub file_size: u64,
    pub file_hash: String,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, optional_fields))]
pub struct TransferError {
    pub transfer_id: String,
    pub code: String,
    pub message: String,
    pub recoverable: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, optional_fields))]
// Never constructed in Rust — the frame is built as raw JSON — but this
// struct is the ts-rs source of the web client's generated binding. The
// allow is load-bearing: delete the type and the web loses its wire type.
#[allow(dead_code)]
pub struct Abort {
    pub transfer_id: String,
    pub reason: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, optional_fields))]
// Never constructed in Rust — the frame is built as raw JSON — but this
// struct is the ts-rs source of the web client's generated binding. The
// allow is load-bearing: delete the type and the web loses its wire type.
#[allow(dead_code)]
pub struct Resume {
    pub transfer_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, optional_fields))]
pub struct ResumeResult {
    pub transfer_id: String,
    pub direction: Direction,
    pub chunks_received: Vec<u32>,
    pub total_chunks: u32,
    pub chunk_size: u32,
    pub file_size: u64,
    pub file_hash: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, optional_fields))]
pub struct StatusResult {
    pub transfer_id: String,
    pub direction: Direction,
    pub phase: String,
    pub filename: String,
    pub file_size: u64,
    pub chunks_done: u32,
    pub total_chunks: u32,
    pub bytes_transferred: u64,
    pub elapsed_ms: u64,
    pub error_count: u32,
}

#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, optional_fields))]
pub struct TransferSummary {
    pub transfer_id: String,
    pub direction: Direction,
    pub filename: String,
    pub file_size: u64,
    pub phase: String,
    pub chunks_done: u32,
    pub total_chunks: u32,
    pub bytes_transferred: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, optional_fields))]
pub struct ListResult {
    pub transfers: Vec<TransferSummary>,
}

/// Configuration for the transfer manager.
pub struct TransferConfig {
    pub max_concurrent: usize,
    pub chunk_size: u32,
    pub max_file_size: u64,
    pub stale_timeout_secs: u64,
    pub max_chunk_retries: u32,
}

impl TransferConfig {
    pub fn new(
        max_concurrent: usize,
        chunk_size: u32,
        max_file_size: u64,
        stale_timeout_secs: u64,
    ) -> Self {
        Self {
            max_concurrent,
            chunk_size,
            max_file_size,
            stale_timeout_secs,
            max_chunk_retries: 3,
        }
    }
}

impl Phase {
    pub fn as_str(&self) -> &str {
        match self {
            Phase::Init => "init",
            Phase::Transferring => "transferring",
            Phase::Paused => "paused",
            Phase::Verifying => "verifying",
            Phase::Complete => "complete",
            Phase::Failed(_) => "failed",
            Phase::Aborted => "aborted",
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::InitDownload;

    fn asked(body: &serde_json::Value) -> Option<u32> {
        serde_json::from_value::<InitDownload>(body.clone())
            .unwrap()
            .chunk_size
    }

    /// A `chunk_size` too large for a `u32` is read as `u32::MAX`, above the
    /// ceiling the init answers it with: never refused, and never wrapped to
    /// a small size.
    #[test]
    fn a_chunk_size_of_any_size_is_read_and_saturated() {
        assert_eq!(asked(&json!({"path": "/x"})), None, "absent");
        assert_eq!(asked(&json!({"path": "/x", "chunk_size": null})), None);
        assert_eq!(
            asked(&json!({"path": "/x", "chunk_size": 65536})),
            Some(65536)
        );
        for big in [
            u64::from(u32::MAX),
            u64::from(u32::MAX) + 1,
            u64::from(u32::MAX) + 1001,
            u64::MAX,
        ] {
            assert_eq!(
                asked(&json!({"path": "/x", "chunk_size": big})),
                Some(u32::MAX),
                "{big}"
            );
        }
        assert!(
            serde_json::from_value::<InitDownload>(json!({"path": "/x", "chunk_size": -1}))
                .is_err(),
            "not a size"
        );

        // Written as it is, and absent when there is none.
        let init = InitDownload {
            path: "/x".into(),
            chunk_size: Some(2048),
        };
        assert_eq!(
            serde_json::to_value(&init).unwrap(),
            json!({"path": "/x", "chunk_size": 2048})
        );
        let init = InitDownload {
            path: "/x".into(),
            chunk_size: None,
        };
        assert_eq!(serde_json::to_value(&init).unwrap(), json!({"path": "/x"}));
    }
}
