//! Transfer lifecycle manager — owns transfers, chunk I/O, disk space checks.
//!
//! Zero full-file buffering: only one chunk (256 KiB default) in memory at a time.
//! Uploads write chunks directly to a temp file via seek+write. Downloads serve
//! chunks by seek+read from the source file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{broadcast, RwLock};
use tracing::{info, warn};

use super::hasher;
use super::types::{
    ChunkAck, ChunkHeader, Complete, Direction, InitDownloadResult, InitUpload, InitUploadResult,
    ListResult, Phase, Progress, ResumeResult, StatusResult, TransferConfig, TransferError,
    TransferProgress, TransferSpec, TransferSummary,
};

/// Owns the set of active transfers and their lifecycle.
pub struct TransferManager {
    transfers: RwLock<HashMap<String, Transfer>>,
    config: TransferConfig,
    progress_tx: broadcast::Sender<Value>,
}

struct Transfer {
    spec: TransferSpec,
    progress: TransferProgress,
}

impl TransferManager {
    pub fn new(config: TransferConfig, progress_tx: broadcast::Sender<Value>) -> Self {
        Self {
            transfers: RwLock::new(HashMap::new()),
            config,
            progress_tx,
        }
    }

    // ─── Download Init ───────────────────────────────────────────────────────

    pub async fn init_download(
        &self,
        path: &str,
        chunk_size: Option<u32>,
    ) -> Result<InitDownloadResult, TransferError> {
        let validated = validate_transfer_path(path)?;

        let metadata = tokio::fs::metadata(&validated).await.map_err(|e| {
            let (code, msg) = match e.kind() {
                std::io::ErrorKind::NotFound => ("FILE_NOT_FOUND", "File not found"),
                std::io::ErrorKind::PermissionDenied => ("PERMISSION_DENIED", "Permission denied"),
                _ => ("IO_ERROR", "I/O error"),
            };
            make_error("", code, &format!("{msg}: {e}"), false)
        })?;

        if metadata.is_dir() {
            return Err(make_error("", "INVALID_PATH", "Path is a directory", false));
        }

        let file_size = metadata.len();
        if file_size > self.config.max_file_size {
            return Err(make_error(
                "",
                "FILE_TOO_LARGE",
                &format!(
                    "File too large ({file_size} bytes, max {})",
                    self.config.max_file_size
                ),
                false,
            ));
        }

        // Check concurrent transfer limit
        {
            let transfers = self.transfers.read().await;
            let active = transfers
                .values()
                .filter(|t| matches!(t.progress.phase, Phase::Init | Phase::Transferring))
                .count();
            if active >= self.config.max_concurrent {
                return Err(make_error(
                    "",
                    "MAX_TRANSFERS",
                    &format!(
                        "Concurrent transfer limit reached (max {})",
                        self.config.max_concurrent
                    ),
                    true,
                ));
            }
        }

        let chunk_size = chunk_size.unwrap_or(self.config.chunk_size);
        let total_chunks = compute_chunks(file_size, chunk_size);

        // Compute whole-file hash (streaming, 64KB blocks)
        let file_hash = hasher::hash_file(&validated)
            .await
            .map_err(|e| make_error("", "IO_ERROR", &format!("Failed to hash file: {e}"), false))?;

        let source_mtime = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_secs());

        let filename = validated.file_name().map_or_else(
            || "download".to_string(),
            |n| n.to_string_lossy().into_owned(),
        );

        let transfer_id = uuid::Uuid::new_v4().to_string();

        let spec = TransferSpec {
            transfer_id: transfer_id.clone(),
            direction: Direction::Download,
            path: validated,
            filename: filename.clone(),
            file_size,
            file_hash: file_hash.clone(),
            chunk_size,
            total_chunks,
            mode: None,
            created_at: Instant::now(),
            source_mtime,
        };

        let progress = TransferProgress {
            phase: Phase::Transferring,
            chunks_done: vec![false; total_chunks as usize],
            bytes_transferred: 0,
            last_activity: Instant::now(),
            temp_path: PathBuf::new(), // No temp file for downloads
            error_count: 0,
        };

        self.transfers
            .write()
            .await
            .insert(transfer_id.clone(), Transfer { spec, progress });

        info!(
            transfer_id = %transfer_id,
            filename = %filename,
            file_size,
            total_chunks,
            chunk_size,
            "Download init"
        );

        Ok(InitDownloadResult {
            transfer_id,
            file_size,
            file_hash,
            chunk_size,
            total_chunks,
            filename,
        })
    }

    // ─── Upload Init ─────────────────────────────────────────────────────────

    #[allow(clippy::too_many_lines)]
    pub async fn init_upload(&self, req: InitUpload) -> Result<InitUploadResult, TransferError> {
        let dir_path = validate_transfer_path(&req.path)?;

        // Verify target is a directory
        let meta = tokio::fs::metadata(&dir_path).await.map_err(|e| {
            let (code, msg) = match e.kind() {
                std::io::ErrorKind::NotFound => ("FILE_NOT_FOUND", "Directory not found"),
                std::io::ErrorKind::PermissionDenied => ("PERMISSION_DENIED", "Permission denied"),
                _ => ("IO_ERROR", "I/O error"),
            };
            make_error("", code, &format!("{msg}: {e}"), false)
        })?;

        if !meta.is_dir() {
            return Err(make_error(
                "",
                "INVALID_PATH",
                "Target path is not a directory",
                false,
            ));
        }

        // Validate filename
        if req.filename.is_empty()
            || req.filename.contains('/')
            || req.filename.contains('\\')
            || req.filename == ".."
        {
            return Err(make_error(
                "",
                "INVALID_PATH",
                &format!("Invalid filename: {}", req.filename),
                false,
            ));
        }

        if req.file_size > self.config.max_file_size {
            return Err(make_error(
                "",
                "FILE_TOO_LARGE",
                &format!(
                    "File too large ({} bytes, max {})",
                    req.file_size, self.config.max_file_size
                ),
                false,
            ));
        }

        // Check concurrent transfer limit
        {
            let transfers = self.transfers.read().await;
            let active = transfers
                .values()
                .filter(|t| matches!(t.progress.phase, Phase::Init | Phase::Transferring))
                .count();
            if active >= self.config.max_concurrent {
                return Err(make_error(
                    "",
                    "MAX_TRANSFERS",
                    &format!(
                        "Concurrent transfer limit reached (max {})",
                        self.config.max_concurrent
                    ),
                    true,
                ));
            }
        }

        // Disk space pre-check via statvfs
        check_disk_space(&dir_path, req.file_size)?;

        let chunk_size = req.chunk_size.max(1024); // Minimum 1 KiB
        let total_chunks = compute_chunks(req.file_size, chunk_size);

        // Verify caller's chunk count matches
        if req.total_chunks != total_chunks {
            return Err(make_error(
                "",
                "INVALID_REQUEST",
                &format!(
                    "Chunk count mismatch: client sent {}, expected {total_chunks}",
                    req.total_chunks
                ),
                false,
            ));
        }

        let transfer_id = uuid::Uuid::new_v4().to_string();

        // Create temp file and pre-allocate
        let temp_path = dir_path.join(format!(".gx_tmp_{transfer_id}"));
        let temp_file = tokio::fs::File::create(&temp_path).await.map_err(|e| {
            make_error(
                "",
                "IO_ERROR",
                &format!("Failed to create temp file: {e}"),
                false,
            )
        })?;
        temp_file.set_len(req.file_size).await.map_err(|e| {
            // Clean up orphaned temp file on allocation failure
            let _ = std::fs::remove_file(&temp_path);
            make_error(
                "",
                "DISK_FULL",
                &format!("Failed to pre-allocate {}: {e}", req.file_size),
                false,
            )
        })?;

        let spec = TransferSpec {
            transfer_id: transfer_id.clone(),
            direction: Direction::Upload,
            path: dir_path,
            filename: req.filename.clone(),
            file_size: req.file_size,
            file_hash: req.file_hash,
            chunk_size,
            total_chunks,
            mode: req.mode,
            created_at: Instant::now(),
            source_mtime: None,
        };

        let progress = TransferProgress {
            phase: Phase::Transferring,
            chunks_done: vec![false; total_chunks as usize],
            bytes_transferred: 0,
            last_activity: Instant::now(),
            temp_path: temp_path.clone(),
            error_count: 0,
        };

        self.transfers
            .write()
            .await
            .insert(transfer_id.clone(), Transfer { spec, progress });

        info!(
            transfer_id = %transfer_id,
            filename = %req.filename,
            file_size = req.file_size,
            total_chunks,
            chunk_size,
            "Upload init"
        );

        Ok(InitUploadResult {
            transfer_id,
            chunk_size,
            total_chunks,
        })
    }

    // ─── Serve Chunk (Download) ──────────────────────────────────────────────

    #[allow(clippy::too_many_lines)]
    pub async fn serve_chunk(
        &self,
        transfer_id: &str,
        chunk_index: u32,
    ) -> Result<(ChunkHeader, Vec<u8>), TransferError> {
        let transfers = self.transfers.read().await;
        let transfer = transfers.get(transfer_id).ok_or_else(|| {
            make_error(
                transfer_id,
                "TRANSFER_NOT_FOUND",
                "Transfer not found",
                false,
            )
        })?;

        if transfer.spec.direction != Direction::Download {
            return Err(make_error(
                transfer_id,
                "INVALID_REQUEST",
                "Not a download transfer",
                false,
            ));
        }

        if !matches!(transfer.progress.phase, Phase::Transferring | Phase::Paused) {
            return Err(make_error(
                transfer_id,
                "INVALID_REQUEST",
                &format!(
                    "Transfer in phase {:?}, cannot serve chunks",
                    transfer.progress.phase.as_str()
                ),
                false,
            ));
        }

        if chunk_index >= transfer.spec.total_chunks {
            return Err(make_error(
                transfer_id,
                "INVALID_REQUEST",
                &format!(
                    "Chunk index {chunk_index} out of range (total {})",
                    transfer.spec.total_chunks
                ),
                false,
            ));
        }

        // Check source file hasn't changed
        if let Some(original_mtime) = transfer.spec.source_mtime {
            if let Ok(meta) = tokio::fs::metadata(&transfer.spec.path).await {
                let current_mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs());
                if current_mtime != Some(original_mtime) {
                    return Err(make_error(
                        transfer_id,
                        "FILE_CHANGED",
                        "Source file modified during transfer",
                        false,
                    ));
                }
            }
        }

        let offset = u64::from(chunk_index) * u64::from(transfer.spec.chunk_size);
        #[allow(clippy::cast_possible_truncation)]
        let chunk_len = std::cmp::min(
            u64::from(transfer.spec.chunk_size),
            transfer.spec.file_size.saturating_sub(offset),
        ) as usize;
        let source_path = transfer.spec.path.clone();

        drop(transfers); // Release lock during I/O

        // Read chunk from disk
        let mut file = tokio::fs::File::open(&source_path).await.map_err(|e| {
            make_error(
                transfer_id,
                "IO_ERROR",
                &format!("Failed to open source: {e}"),
                false,
            )
        })?;
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|e| {
                make_error(transfer_id, "IO_ERROR", &format!("Seek failed: {e}"), false)
            })?;
        let mut buf = vec![0u8; chunk_len];
        file.read_exact(&mut buf).await.map_err(|e| {
            make_error(transfer_id, "IO_ERROR", &format!("Read failed: {e}"), false)
        })?;

        let chunk_hash = hasher::hash_bytes(&buf);

        // Update progress
        {
            let mut transfers = self.transfers.write().await;
            if let Some(t) = transfers.get_mut(transfer_id) {
                t.progress.last_activity = Instant::now();
                if let Some(slot) = t.progress.chunks_done.get_mut(chunk_index as usize) {
                    *slot = true;
                }
                t.progress.bytes_transferred += chunk_len as u64;

                // Mark download complete when all chunks have been served
                let all_done = t.progress.chunks_done.iter().all(|&v| v);
                if all_done {
                    t.progress.phase = Phase::Complete;
                    #[allow(clippy::cast_possible_truncation)]
                    let elapsed_ms = t.spec.created_at.elapsed().as_millis() as u64;
                    let complete = Complete {
                        transfer_id: transfer_id.to_string(),
                        direction: Direction::Download,
                        path: t.spec.path.to_string_lossy().into_owned(),
                        filename: t.spec.filename.clone(),
                        file_size: t.spec.file_size,
                        file_hash: t.spec.file_hash.clone(),
                        elapsed_ms,
                    };
                    let _ = self.progress_tx.send(json!({
                        "type": "gx.complete",
                        "data": serde_json::to_value(&complete).unwrap_or_default(),
                    }));
                    info!(
                        transfer_id = %transfer_id,
                        filename = %t.spec.filename,
                        file_size = t.spec.file_size,
                        elapsed_ms,
                        "Download complete"
                    );
                }
                self.emit_progress(t);
            }
        }

        Ok((
            ChunkHeader {
                transfer_id: transfer_id.to_string(),
                chunk_index,
                chunk_hash,
            },
            buf,
        ))
    }

    // ─── Receive Chunk (Upload) ──────────────────────────────────────────────

    #[allow(clippy::too_many_lines)]
    pub async fn receive_chunk(
        &self,
        transfer_id: &str,
        chunk_index: u32,
        chunk_hash: &str,
        data: &[u8],
    ) -> Result<ChunkAck, TransferError> {
        let (offset, _chunk_size, temp_path, total_chunks, file_hash, file_size, final_path, mode) = {
            let transfers = self.transfers.read().await;
            let transfer = transfers.get(transfer_id).ok_or_else(|| {
                make_error(
                    transfer_id,
                    "TRANSFER_NOT_FOUND",
                    "Transfer not found",
                    false,
                )
            })?;

            if transfer.spec.direction != Direction::Upload {
                return Err(make_error(
                    transfer_id,
                    "INVALID_REQUEST",
                    "Not an upload transfer",
                    false,
                ));
            }

            if !matches!(transfer.progress.phase, Phase::Transferring) {
                return Err(make_error(
                    transfer_id,
                    "INVALID_REQUEST",
                    &format!(
                        "Transfer in phase {}, cannot receive chunks",
                        transfer.progress.phase.as_str()
                    ),
                    false,
                ));
            }

            if chunk_index >= transfer.spec.total_chunks {
                return Err(make_error(
                    transfer_id,
                    "INVALID_REQUEST",
                    &format!(
                        "Chunk index {chunk_index} out of range (total {})",
                        transfer.spec.total_chunks
                    ),
                    false,
                ));
            }

            let offset = u64::from(chunk_index) * u64::from(transfer.spec.chunk_size);
            (
                offset,
                transfer.spec.chunk_size,
                transfer.progress.temp_path.clone(),
                transfer.spec.total_chunks,
                transfer.spec.file_hash.clone(),
                transfer.spec.file_size,
                transfer.spec.path.join(&transfer.spec.filename),
                transfer.spec.mode.clone(),
            )
        };

        // Verify chunk hash
        let actual_hash = hasher::hash_bytes(data);
        if actual_hash != chunk_hash {
            let mut transfers = self.transfers.write().await;
            if let Some(t) = transfers.get_mut(transfer_id) {
                t.progress.error_count += 1;
                t.progress.last_activity = Instant::now();
                if t.progress.error_count >= self.config.max_chunk_retries * total_chunks {
                    t.progress.phase = Phase::Failed("Too many chunk errors".to_string());
                }
            }
            return Ok(ChunkAck {
                transfer_id: transfer_id.to_string(),
                chunk_index,
                ok: false,
                error: Some("Chunk hash mismatch".to_string()),
            });
        }

        // Write chunk to temp file at correct offset
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(&temp_path)
            .await
            .map_err(|e| {
                make_error(
                    transfer_id,
                    "IO_ERROR",
                    &format!("Failed to open temp file: {e}"),
                    false,
                )
            })?;
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|e| {
                make_error(transfer_id, "IO_ERROR", &format!("Seek failed: {e}"), false)
            })?;
        file.write_all(data).await.map_err(|e| {
            make_error(
                transfer_id,
                "IO_ERROR",
                &format!("Write failed: {e}"),
                false,
            )
        })?;
        file.sync_data().await.map_err(|e| {
            make_error(transfer_id, "IO_ERROR", &format!("Sync failed: {e}"), false)
        })?;

        // Update progress
        let all_done = {
            let mut transfers = self.transfers.write().await;
            let t = transfers.get_mut(transfer_id).ok_or_else(|| {
                make_error(
                    transfer_id,
                    "TRANSFER_NOT_FOUND",
                    "Transfer not found",
                    false,
                )
            })?;
            if let Some(slot) = t.progress.chunks_done.get_mut(chunk_index as usize) {
                *slot = true;
            }
            t.progress.bytes_transferred += data.len() as u64;
            t.progress.last_activity = Instant::now();

            let all_done = t.progress.chunks_done.iter().all(|&v| v);
            if all_done {
                t.progress.phase = Phase::Verifying;
            }
            self.emit_progress(t);
            all_done
        };

        // If all chunks received, verify whole-file hash
        if all_done {
            self.verify_and_finalize(
                transfer_id,
                &temp_path,
                &file_hash,
                file_size,
                &final_path,
                mode.as_deref(),
            )
            .await?;
        }

        Ok(ChunkAck {
            transfer_id: transfer_id.to_string(),
            chunk_index,
            ok: true,
            error: None,
        })
    }

    /// Verify whole-file hash and atomically move temp → final.
    async fn verify_and_finalize(
        &self,
        transfer_id: &str,
        temp_path: &Path,
        expected_hash: &str,
        file_size: u64,
        final_path: &Path,
        mode: Option<&str>,
    ) -> Result<(), TransferError> {
        info!(transfer_id = %transfer_id, "Verifying upload hash...");

        let actual_hash = hasher::hash_file(temp_path).await.map_err(|e| {
            make_error(
                transfer_id,
                "IO_ERROR",
                &format!("Hash verification failed: {e}"),
                false,
            )
        })?;

        if expected_hash.is_empty() {
            // Client omitted hash — server computed it; update the transfer spec
            let mut transfers = self.transfers.write().await;
            if let Some(t) = transfers.get_mut(transfer_id) {
                t.spec.file_hash.clone_from(&actual_hash);
            }
            drop(transfers);
        } else if actual_hash != expected_hash {
            // Hash mismatch — delete temp file, mark failed
            let _ = tokio::fs::remove_file(temp_path).await;
            let mut transfers = self.transfers.write().await;
            if let Some(t) = transfers.get_mut(transfer_id) {
                t.progress.phase =
                    Phase::Failed("Whole-file hash mismatch after all chunks received".to_string());
            }
            return Err(make_error(
                transfer_id,
                "HASH_MISMATCH",
                &format!("Expected {expected_hash}, got {actual_hash}"),
                false,
            ));
        }

        // Set file permissions if specified
        if let Some(mode_str) = mode {
            if let Ok(mode_val) = u32::from_str_radix(mode_str, 8) {
                use std::os::unix::fs::PermissionsExt;
                let perms = std::fs::Permissions::from_mode(mode_val);
                let _ = tokio::fs::set_permissions(temp_path, perms).await;
            }
        }

        // Atomic rename
        if let Err(e) = tokio::fs::rename(temp_path, final_path).await {
            let _ = tokio::fs::remove_file(temp_path).await;
            return Err(make_error(
                transfer_id,
                "IO_ERROR",
                &format!("Failed to finalize: {e}"),
                false,
            ));
        }

        let mut transfers = self.transfers.write().await;
        if let Some(t) = transfers.get_mut(transfer_id) {
            t.progress.phase = Phase::Complete;
            #[allow(clippy::cast_possible_truncation)]
            let elapsed_ms = t.spec.created_at.elapsed().as_millis() as u64;
            let complete = Complete {
                transfer_id: transfer_id.to_string(),
                direction: Direction::Upload,
                path: final_path.to_string_lossy().into_owned(),
                filename: t.spec.filename.clone(),
                file_size,
                file_hash: t.spec.file_hash.clone(),
                elapsed_ms,
            };
            let _ = self.progress_tx.send(json!({
                "type": "gx.complete",
                "data": serde_json::to_value(&complete).unwrap_or_default(),
            }));
            info!(
                transfer_id = %transfer_id,
                filename = %t.spec.filename,
                file_size,
                elapsed_ms,
                "Upload complete"
            );
        }

        Ok(())
    }

    // ─── Resume ──────────────────────────────────────────────────────────────

    pub async fn resume(&self, transfer_id: &str) -> Result<ResumeResult, TransferError> {
        let mut transfers = self.transfers.write().await;
        let transfer = transfers.get_mut(transfer_id).ok_or_else(|| {
            make_error(
                transfer_id,
                "TRANSFER_NOT_FOUND",
                "Transfer not found",
                false,
            )
        })?;

        match &transfer.progress.phase {
            Phase::Paused | Phase::Transferring => {}
            phase => {
                return Err(make_error(
                    transfer_id,
                    "INVALID_REQUEST",
                    &format!("Cannot resume transfer in phase {}", phase.as_str()),
                    false,
                ));
            }
        }

        transfer.progress.phase = Phase::Transferring;
        transfer.progress.last_activity = Instant::now();

        let chunks_received: Vec<u32> = transfer
            .progress
            .chunks_done
            .iter()
            .enumerate()
            .filter(|(_, &done)| done)
            .map(|(i, _)| {
                #[allow(clippy::cast_possible_truncation)]
                let idx = i as u32;
                idx
            })
            .collect();

        Ok(ResumeResult {
            transfer_id: transfer_id.to_string(),
            direction: transfer.spec.direction,
            chunks_received,
            total_chunks: transfer.spec.total_chunks,
            chunk_size: transfer.spec.chunk_size,
            file_size: transfer.spec.file_size,
            file_hash: transfer.spec.file_hash.clone(),
        })
    }

    // ─── Abort ───────────────────────────────────────────────────────────────

    pub async fn abort(&self, transfer_id: &str, reason: &str) -> Result<(), TransferError> {
        let mut transfers = self.transfers.write().await;
        let transfer = transfers.get_mut(transfer_id).ok_or_else(|| {
            make_error(
                transfer_id,
                "TRANSFER_NOT_FOUND",
                "Transfer not found",
                false,
            )
        })?;

        transfer.progress.phase = Phase::Aborted;

        // Clean up temp file if it exists
        if !transfer.progress.temp_path.as_os_str().is_empty() {
            let _ = tokio::fs::remove_file(&transfer.progress.temp_path).await;
        }

        info!(transfer_id = %transfer_id, reason = %reason, "Transfer aborted");
        Ok(())
    }

    // ─── Status ──────────────────────────────────────────────────────────────

    pub async fn status(&self, transfer_id: &str) -> Result<StatusResult, TransferError> {
        let transfers = self.transfers.read().await;
        let transfer = transfers.get(transfer_id).ok_or_else(|| {
            make_error(
                transfer_id,
                "TRANSFER_NOT_FOUND",
                "Transfer not found",
                false,
            )
        })?;

        #[allow(clippy::cast_possible_truncation)]
        let chunks_done = transfer.progress.chunks_done.iter().filter(|&&v| v).count() as u32;
        #[allow(clippy::cast_possible_truncation)]
        let elapsed_ms = transfer.spec.created_at.elapsed().as_millis() as u64;

        Ok(StatusResult {
            transfer_id: transfer_id.to_string(),
            direction: transfer.spec.direction,
            phase: transfer.progress.phase.as_str().to_string(),
            filename: transfer.spec.filename.clone(),
            file_size: transfer.spec.file_size,
            chunks_done,
            total_chunks: transfer.spec.total_chunks,
            bytes_transferred: transfer.progress.bytes_transferred,
            elapsed_ms,
            error_count: transfer.progress.error_count,
        })
    }

    // ─── List ────────────────────────────────────────────────────────────────

    pub async fn list(&self) -> ListResult {
        let transfers = self.transfers.read().await;
        let summaries = transfers
            .values()
            .map(|t| {
                #[allow(clippy::cast_possible_truncation)]
                let chunks_done = t.progress.chunks_done.iter().filter(|&&v| v).count() as u32;
                TransferSummary {
                    transfer_id: t.spec.transfer_id.clone(),
                    direction: t.spec.direction,
                    filename: t.spec.filename.clone(),
                    file_size: t.spec.file_size,
                    phase: t.progress.phase.as_str().to_string(),
                    chunks_done,
                    total_chunks: t.spec.total_chunks,
                    bytes_transferred: t.progress.bytes_transferred,
                }
            })
            .collect();
        ListResult {
            transfers: summaries,
        }
    }

    // ─── Maintenance ─────────────────────────────────────────────────────────

    /// Pause all active transfers (called on tunnel disconnect).
    pub async fn pause_all(&self) {
        let mut transfers = self.transfers.write().await;
        let mut count = 0u32;
        for t in transfers.values_mut() {
            if matches!(t.progress.phase, Phase::Transferring | Phase::Init) {
                t.progress.phase = Phase::Paused;
                count += 1;
            }
        }
        if count > 0 {
            info!(count, "Paused active transfers");
        }
    }

    /// Remove stale transfers (paused/failed older than timeout). Returns removed IDs.
    pub async fn sweep_stale(&self) -> Vec<String> {
        let timeout = std::time::Duration::from_secs(self.config.stale_timeout_secs);
        let mut transfers = self.transfers.write().await;
        let mut removed = Vec::new();

        let stale_ids: Vec<String> = transfers
            .iter()
            .filter(|(_, t)| {
                matches!(
                    t.progress.phase,
                    Phase::Paused | Phase::Failed(_) | Phase::Aborted | Phase::Complete
                ) && t.progress.last_activity.elapsed() > timeout
            })
            .map(|(id, _)| id.clone())
            .collect();

        for id in stale_ids {
            if let Some(t) = transfers.remove(&id) {
                // Clean up temp file
                if !t.progress.temp_path.as_os_str().is_empty() {
                    let _ = tokio::fs::remove_file(&t.progress.temp_path).await;
                }
                removed.push(id);
            }
        }

        if !removed.is_empty() {
            info!(count = removed.len(), "Swept stale transfers");
        }
        removed
    }

    /// Get a progress snapshot for broadcasting.
    fn progress_snapshot(transfer: &Transfer) -> Progress {
        #[allow(clippy::cast_possible_truncation)]
        let chunks_done = transfer.progress.chunks_done.iter().filter(|&&v| v).count() as u32;
        #[allow(clippy::cast_possible_truncation)]
        let elapsed_ms = transfer.spec.created_at.elapsed().as_millis() as u64;
        let rate_bps = if elapsed_ms > 0 {
            transfer.progress.bytes_transferred * 1000 / elapsed_ms
        } else {
            0
        };

        Progress {
            transfer_id: transfer.spec.transfer_id.clone(),
            direction: transfer.spec.direction,
            path: transfer.spec.path.to_string_lossy().into_owned(),
            filename: transfer.spec.filename.clone(),
            chunks_done,
            total_chunks: transfer.spec.total_chunks,
            bytes_transferred: transfer.progress.bytes_transferred,
            file_size: transfer.spec.file_size,
            elapsed_ms,
            rate_bps,
        }
    }

    /// Emit a progress event through the broadcast channel.
    fn emit_progress(&self, transfer: &Transfer) {
        let progress = Self::progress_snapshot(transfer);
        let _ = self.progress_tx.send(json!({
            "type": "gx.progress",
            "data": serde_json::to_value(&progress).unwrap_or_default(),
        }));
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Compute total chunks for a file of given size.
pub fn compute_chunks(file_size: u64, chunk_size: u32) -> u32 {
    if file_size == 0 {
        return 1; // Empty files still have one (empty) chunk
    }
    #[allow(clippy::cast_possible_truncation)]
    {
        file_size.div_ceil(u64::from(chunk_size)) as u32
    }
}

/// Validate an absolute path (reuses logic from routes/files.rs).
fn validate_transfer_path(path: &str) -> Result<PathBuf, TransferError> {
    let p = Path::new(path);
    if !p.is_absolute() {
        return Err(make_error(
            "",
            "INVALID_PATH",
            "Path must be absolute",
            false,
        ));
    }
    if path.contains('\0') {
        return Err(make_error(
            "",
            "INVALID_PATH",
            "Path contains null bytes",
            false,
        ));
    }
    for component in p.components() {
        if let std::path::Component::ParentDir = component {
            return Err(make_error(
                "",
                "INVALID_PATH",
                "Path traversal (..) not allowed",
                false,
            ));
        }
    }
    Ok(p.to_path_buf())
}

/// Check available disk space via statvfs.
fn check_disk_space(path: &Path, required_bytes: u64) -> Result<(), TransferError> {
    match nix::sys::statvfs::statvfs(path) {
        Ok(stat) => {
            let available = stat.blocks_available() * stat.fragment_size();
            // Require file_size + 10% headroom
            let needed = required_bytes + required_bytes / 10;
            if available < needed {
                Err(make_error(
                    "",
                    "DISK_FULL",
                    &format!(
                        "Insufficient disk space: {available} bytes available, {needed} bytes needed"
                    ),
                    false,
                ))
            } else {
                Ok(())
            }
        }
        Err(e) => {
            warn!(
                "statvfs failed for {}: {e} — skipping disk space check",
                path.display()
            );
            Ok(()) // Don't fail on statvfs errors (e.g., /tmp on some systems)
        }
    }
}

fn make_error(transfer_id: &str, code: &str, message: &str, recoverable: bool) -> TransferError {
    TransferError {
        transfer_id: transfer_id.to_string(),
        code: code.to_string(),
        message: message.to_string(),
        recoverable,
    }
}
