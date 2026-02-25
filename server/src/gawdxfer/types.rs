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
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Upload,
    Download,
}

/// Transfer lifecycle phase.
#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Serialize, Deserialize)]
pub struct InitDownload {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk_size: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InitDownloadResult {
    pub transfer_id: String,
    pub file_size: u64,
    pub file_hash: String,
    pub chunk_size: u32,
    pub total_chunks: u32,
    pub filename: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InitUpload {
    pub path: String,
    pub filename: String,
    pub file_size: u64,
    /// Whole-file SHA-256 hash. If empty, the server computes it after all chunks are received.
    #[serde(default)]
    pub file_hash: String,
    pub chunk_size: u32,
    pub total_chunks: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InitUploadResult {
    pub transfer_id: String,
    pub chunk_size: u32,
    pub total_chunks: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ChunkHeader {
    pub transfer_id: String,
    pub chunk_index: u32,
    pub chunk_hash: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ChunkAck {
    pub transfer_id: String,
    pub chunk_index: u32,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
pub struct TransferError {
    pub transfer_id: String,
    pub code: String,
    pub message: String,
    pub recoverable: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct Abort {
    pub transfer_id: String,
    pub reason: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct Resume {
    pub transfer_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
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
