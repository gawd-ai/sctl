//! Transfer lifecycle manager — owns transfers, chunk I/O, disk space checks.
//!
//! Zero full-file buffering: only one chunk (256 KiB default) in memory at a time.
//! Uploads write chunks directly to a temp file via seek+write. A download
//! passes the file gate at init (ADR-011), hashes exactly the size its stat
//! gave, and serves each chunk by seek+read from the file it hashed.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use serde_json::{json, Value};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{broadcast, RwLock};
use tracing::{info, warn};

use super::hasher;
use super::types::{
    ChunkAck, ChunkHeader, Complete, Direction, InitDownloadResult, InitUpload, InitUploadResult,
    ListResult, Phase, Progress, ResumeResult, StatusResult, TransferConfig, TransferError,
    TransferProgress, TransferSpec, TransferSummary,
};
use crate::activity::{ActivityLog, ActivitySource, ActivityType};
use crate::file_access::{self, FileId, Gate, Opened, Purpose, Refusal, Reopen};

/// Owns the set of active transfers and their lifecycle.
pub struct TransferManager {
    transfers: RwLock<HashMap<String, Transfer>>,
    config: TransferConfig,
    progress_tx: broadcast::Sender<Value>,
    activity_log: Arc<ActivityLog>,
    /// The file gate every download passes before its file is opened.
    gate: Arc<Gate>,
    /// Downloads past the limit check and not yet registered: they hash.
    /// Counted with the active transfers against `max_concurrent`.
    hashing: Arc<AtomicUsize>,
    /// Tests hold hashes here, inside the hash, with their places taken.
    #[cfg(test)]
    hash_hold: std::sync::Mutex<Option<Arc<tests::HashHold>>>,
}

/// The smallest chunk a transfer uses, an upload's or a download's: a
/// smaller one asked for is raised to it, so 0 never divides a size.
const MIN_CHUNK_SIZE: u32 = 1024;

/// A download's place under `max_concurrent` from its limit check until it
/// is registered. The hash owns it, so a place is freed only when its hash
/// has ended, even when the request that started it is gone.
struct Place(Arc<AtomicUsize>);

impl Drop for Place {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

struct Transfer {
    spec: TransferSpec,
    progress: TransferProgress,
}

impl TransferManager {
    pub fn new(
        config: TransferConfig,
        progress_tx: broadcast::Sender<Value>,
        activity_log: Arc<ActivityLog>,
        gate: Arc<Gate>,
    ) -> Self {
        Self {
            transfers: RwLock::new(HashMap::new()),
            config,
            progress_tx,
            activity_log,
            gate,
            hashing: Arc::new(AtomicUsize::new(0)),
            #[cfg(test)]
            hash_hold: std::sync::Mutex::new(None),
        }
    }

    /// Transfers that hold a place: the active ones, and the downloads
    /// still hashing.
    fn places_in_use(&self, transfers: &HashMap<String, Transfer>) -> usize {
        let active = transfers
            .values()
            .filter(|t| matches!(t.progress.phase, Phase::Init | Phase::Transferring))
            .count();
        active + self.hashing.load(Ordering::SeqCst)
    }

    fn max_transfers_error(&self) -> TransferError {
        make_error(
            "",
            "MAX_TRANSFERS",
            &format!(
                "Concurrent transfer limit reached (max {})",
                self.config.max_concurrent
            ),
            true,
        )
    }

    /// Take a place under `max_concurrent`, or answer `MAX_TRANSFERS`.
    async fn take_place(&self) -> Result<Place, TransferError> {
        // The write lock orders every check-and-take against the others.
        let transfers = self.transfers.write().await;
        if self.places_in_use(&transfers) >= self.config.max_concurrent {
            return Err(self.max_transfers_error());
        }
        self.hashing.fetch_add(1, Ordering::SeqCst);
        Ok(Place(self.hashing.clone()))
    }

    // ─── Download Init ───────────────────────────────────────────────────────

    pub async fn init_download(
        &self,
        path: &str,
        chunk_size: Option<u32>,
    ) -> Result<InitDownloadResult, TransferError> {
        let validated = validate_transfer_path(path)?;

        // The gate, before anything opens the file: only a regular file on
        // a filesystem that reports its size moves (ADR-011).
        let gate = self.gate.clone();
        let judged = validated.clone();
        let opened =
            match tokio::task::spawn_blocking(move || gate.open(&judged, Purpose::Move)).await {
                Ok(Ok(opened)) => opened,
                Ok(Err(refusal)) => {
                    file_access::record(
                        &self.activity_log,
                        ActivitySource::Rest,
                        path,
                        &refusal,
                        None,
                    )
                    .await;
                    return Err(refusal_error(&refusal));
                }
                Err(e) => {
                    return Err(make_error(
                        "",
                        "IO_ERROR",
                        &format!("I/O error: {e}"),
                        false,
                    ));
                }
            };
        let Opened {
            mut file,
            size: file_size,
            modified: source_mtime,
            id: source_file,
            ..
        } = opened;

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

        // The size asked for, raised to the floor an upload has.
        let chunk_size = chunk_size
            .unwrap_or(self.config.chunk_size)
            .max(MIN_CHUNK_SIZE);
        let total_chunks = compute_chunks(file_size, chunk_size);

        // A place under the limit before the hash, so at most that many
        // hashes run (MAX_TRANSFERS past it).
        let place = self.take_place().await?;

        // Whole-file hash of exactly the size the stat gave, from the file
        // the gate judged. The hash holds the place until it ends.
        #[cfg(test)]
        let hold = self.hash_hold.lock().unwrap().clone();
        let (hashed, place) = tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            if let Some(hold) = hold {
                hold.wait();
            }
            (hasher::hash_exact(&mut file, file_size), place)
        })
        .await
        .map_err(|e| make_error("", "IO_ERROR", &format!("Failed to hash file: {e}"), false))?;
        let file_hash = hashed
            .map_err(|e| make_error("", "IO_ERROR", &format!("Failed to hash file: {e}"), false))?;

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
            source_file: Some(source_file),
        };

        let progress = TransferProgress {
            phase: Phase::Transferring,
            chunks_done: vec![false; total_chunks as usize],
            bytes_transferred: 0,
            last_activity: Instant::now(),
            temp_path: PathBuf::new(), // No temp file for downloads
            error_count: 0,
        };

        {
            // Registered, it counts as active: its place goes back under
            // the same lock, so it is never counted twice or not at all.
            let mut transfers = self.transfers.write().await;
            transfers.insert(transfer_id.clone(), Transfer { spec, progress });
            drop(place);
        }

        info!(
            transfer_id = %transfer_id,
            filename = %filename,
            file_size,
            total_chunks,
            chunk_size,
            "Download init"
        );

        self.activity_log
            .log(
                ActivityType::TransferStart,
                ActivitySource::Rest,
                format!("download {filename} ({file_size} bytes)"),
                Some(json!({
                    "transfer_id": transfer_id,
                    "direction": "download",
                    "filename": filename,
                    "file_size": file_size,
                    "total_chunks": total_chunks,
                })),
                None,
            )
            .await;

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

        // Check concurrent transfer limit (a hashing download holds a place)
        {
            let transfers = self.transfers.read().await;
            if self.places_in_use(&transfers) >= self.config.max_concurrent {
                return Err(self.max_transfers_error());
            }
        }

        // Disk space pre-check via statvfs
        check_disk_space(&dir_path, req.file_size)?;

        let chunk_size = req.chunk_size.max(MIN_CHUNK_SIZE);
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
            source_file: None,
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

        self.activity_log
            .log(
                ActivityType::TransferStart,
                ActivitySource::Rest,
                format!("upload {} ({} bytes)", req.filename, req.file_size),
                Some(json!({
                    "transfer_id": transfer_id,
                    "direction": "upload",
                    "filename": req.filename,
                    "file_size": req.file_size,
                    "total_chunks": total_chunks,
                })),
                None,
            )
            .await;

        Ok(InitUploadResult {
            transfer_id,
            chunk_size,
            total_chunks,
        })
    }

    // ─── Serve Chunk (Download) ──────────────────────────────────────────────

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

        match &transfer.progress.phase {
            Phase::Transferring | Phase::Paused => {}
            // A download fails only when its file changed (ADR-011), and
            // every later chunk says so again.
            Phase::Failed(why) => {
                return Err(make_error(transfer_id, "FILE_CHANGED", why, false));
            }
            phase => {
                return Err(make_error(
                    transfer_id,
                    "INVALID_REQUEST",
                    &format!(
                        "Transfer in phase {:?}, cannot serve chunks",
                        phase.as_str()
                    ),
                    false,
                ));
            }
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
        let chunk_len = std::cmp::min(
            u64::from(transfer.spec.chunk_size),
            transfer.spec.file_size.saturating_sub(offset),
        ) as usize;
        let source_path = transfer.spec.path.clone();
        let source_mtime = transfer.spec.source_mtime;
        let source_file = transfer.spec.source_file.ok_or_else(|| {
            make_error(
                transfer_id,
                "INVALID_REQUEST",
                "Not a download transfer",
                false,
            )
        })?;

        drop(transfers); // Release lock during I/O

        // Read the chunk only from the file that was hashed.
        let id = transfer_id.to_string();
        let read = tokio::task::spawn_blocking(move || {
            read_chunk(
                &id,
                &source_path,
                source_file,
                source_mtime,
                offset,
                chunk_len,
            )
        })
        .await
        .map_err(|e| make_error(transfer_id, "IO_ERROR", &format!("Read failed: {e}"), false))?;
        let buf = match read {
            Ok(buf) => buf,
            Err(e) => {
                if e.code == "FILE_CHANGED" {
                    self.fail_download(transfer_id, &e.message).await;
                }
                return Err(e);
            }
        };

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
                    let _ = self.progress_tx.send(
                        crate::ws::messages::WsServerMsg::GxComplete {
                            data: complete.clone(),
                        }
                        .to_value(),
                    );
                    info!(
                        transfer_id = %transfer_id,
                        filename = %t.spec.filename,
                        file_size = t.spec.file_size,
                        elapsed_ms,
                        "Download complete"
                    );
                    self.activity_log
                        .log(
                            ActivityType::TransferComplete,
                            ActivitySource::Rest,
                            format!(
                                "download {} ({} bytes, {}ms)",
                                t.spec.filename, t.spec.file_size, elapsed_ms
                            ),
                            Some(json!({
                                "transfer_id": transfer_id,
                                "direction": "download",
                                "filename": t.spec.filename,
                                "file_size": t.spec.file_size,
                                "elapsed_ms": elapsed_ms,
                            })),
                            None,
                        )
                        .await;
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

    /// A download whose file changed fails with `FILE_CHANGED` (ADR-011):
    /// it gives back its place under `max_concurrent`, and the sweep
    /// removes it once it has been idle for `stale_timeout_secs`.
    async fn fail_download(&self, transfer_id: &str, why: &str) {
        let mut transfers = self.transfers.write().await;
        if let Some(t) = transfers.get_mut(transfer_id) {
            if matches!(t.progress.phase, Phase::Transferring | Phase::Paused) {
                t.progress.phase = Phase::Failed(why.to_string());
                t.progress.last_activity = Instant::now();
                info!(transfer_id = %transfer_id, reason = %why, "Download failed");
            }
        }
    }

    // ─── Receive Chunk (Upload) ──────────────────────────────────────────────

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
            let _ = self.progress_tx.send(
                crate::ws::messages::WsServerMsg::GxComplete {
                    data: complete.clone(),
                }
                .to_value(),
            );
            info!(
                transfer_id = %transfer_id,
                filename = %t.spec.filename,
                file_size,
                elapsed_ms,
                "Upload complete"
            );
            self.activity_log
                .log(
                    ActivityType::TransferComplete,
                    ActivitySource::Rest,
                    format!(
                        "upload {} ({} bytes, {}ms)",
                        t.spec.filename, file_size, elapsed_ms
                    ),
                    Some(json!({
                        "transfer_id": transfer_id,
                        "direction": "upload",
                        "filename": t.spec.filename,
                        "file_size": file_size,
                        "elapsed_ms": elapsed_ms,
                    })),
                    None,
                )
                .await;
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
            .map(|(i, _)| i as u32)
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

        let chunks_done = transfer.progress.chunks_done.iter().filter(|&&v| v).count() as u32;
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
        let chunks_done = transfer.progress.chunks_done.iter().filter(|&&v| v).count() as u32;
        let elapsed_ms = transfer.spec.created_at.elapsed().as_millis() as u64;
        let rate_bps = (transfer.progress.bytes_transferred * 1000)
            .checked_div(elapsed_ms)
            .unwrap_or(0);

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
        let _ = self
            .progress_tx
            .send(crate::ws::messages::WsServerMsg::GxProgress { data: progress }.to_value());
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Compute total chunks for a file of given size.
pub fn compute_chunks(file_size: u64, chunk_size: u32) -> u32 {
    if file_size == 0 {
        return 1; // Empty files still have one (empty) chunk
    }
    {
        file_size.div_ceil(u64::from(chunk_size)) as u32
    }
}

/// A gate refusal as a transfer error; a directory keeps its old code.
fn refusal_error(refusal: &Refusal) -> TransferError {
    let code = match refusal {
        Refusal::IsDirectory => "INVALID_PATH",
        other => other.code(),
    };
    make_error("", code, &refusal.message(), false)
}

/// Read one chunk of a download from the file it hashed: the same device
/// and inode, still a regular file, its mtime unchanged. Any other file at
/// the path is `FILE_CHANGED`, and is never opened for reading.
fn read_chunk(
    transfer_id: &str,
    path: &Path,
    id: FileId,
    mtime: Option<u64>,
    offset: u64,
    len: usize,
) -> Result<Vec<u8>, TransferError> {
    let (mut file, now) = file_access::reopen_judged(path, id).map_err(|e| match e {
        Reopen::Changed(why) => make_error(
            transfer_id,
            "FILE_CHANGED",
            &format!("Source file changed during transfer: {why}"),
            false,
        ),
        Reopen::Io(e) => make_error(
            transfer_id,
            "IO_ERROR",
            &format!("Failed to open source: {e}"),
            false,
        ),
    })?;
    if mtime.is_some() && now != mtime {
        return Err(make_error(
            transfer_id,
            "FILE_CHANGED",
            "Source file modified during transfer",
            false,
        ));
    }
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| make_error(transfer_id, "IO_ERROR", &format!("Seek failed: {e}"), false))?;
    let mut buf = vec![0u8; len];
    file.read_exact(&mut buf)
        .map_err(|e| make_error(transfer_id, "IO_ERROR", &format!("Read failed: {e}"), false))?;
    Ok(buf)
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
            #[allow(clippy::useless_conversion)]
            let available = u64::from(stat.blocks_available()) * u64::from(stat.fragment_size());
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

#[cfg(test)]
mod tests {
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::atomic::AtomicU32;
    use std::time::Duration;

    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "sctl-gx-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn manager(max_concurrent: usize) -> TransferManager {
        let (tx, _) = broadcast::channel(64);
        let log = Arc::new(ActivityLog::new(64, tx.clone()));
        let gate = Arc::new(Gate::new(
            vec![(PathBuf::from("/etc/sctl"), "the agent's directory")],
            None,
        ));
        TransferManager::new(
            TransferConfig::new(max_concurrent, 1024, 1 << 30, 60),
            tx,
            log,
            gate,
        )
    }

    fn path_str(p: &Path) -> String {
        p.display().to_string()
    }

    /// Holds downloads inside their hash, their places taken, until the
    /// test lets them through. Never longer than 10 s, so a test that fails
    /// while it holds some still ends.
    #[derive(Default)]
    pub(super) struct HashHold {
        /// Hashes held now, and hashes let through that have not left yet.
        counts: std::sync::Mutex<(usize, usize)>,
        let_through: std::sync::Condvar,
    }

    impl HashHold {
        /// In the hash, on its blocking thread: wait to be let through.
        pub(super) fn wait(&self) {
            let mut counts = self.counts.lock().unwrap();
            counts.0 += 1;
            let (mut counts, _) = self
                .let_through
                .wait_timeout_while(counts, Duration::from_secs(10), |c| c.1 == 0)
                .unwrap();
            counts.0 -= 1;
            counts.1 = counts.1.saturating_sub(1);
        }

        /// Let `n` held hashes go on.
        fn release(&self, n: usize) {
            self.counts.lock().unwrap().1 += n;
            self.let_through.notify_all();
        }

        /// Wait, at most 2 s, until `n` hashes are held.
        async fn until_held(&self, n: usize) {
            for _ in 0..1000 {
                if self.counts.lock().unwrap().0 == n {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            panic!("{} hashes held, not {n}", self.counts.lock().unwrap().0);
        }
    }

    /// A manager whose hashes wait in `HashHold` until let through.
    fn held_manager(max_concurrent: usize) -> (Arc<TransferManager>, Arc<HashHold>) {
        let tm = Arc::new(manager(max_concurrent));
        let hold = Arc::new(HashHold::default());
        *tm.hash_hold.lock().unwrap() = Some(hold.clone());
        (tm, hold)
    }

    /// Start a download init on its own task.
    fn spawn_init(
        tm: &Arc<TransferManager>,
        path: &Path,
    ) -> tokio::task::JoinHandle<Result<InitDownloadResult, TransferError>> {
        let (tm, path) = (tm.clone(), path_str(path));
        tokio::spawn(async move { tm.init_download(&path, Some(1024)).await })
    }

    #[tokio::test]
    async fn a_fifo_is_refused_at_init_without_waiting() {
        let dir = scratch("fifo");
        let fifo = dir.join("pipe");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).unwrap();
        let tm = manager(4);
        let answer = tokio::time::timeout(
            Duration::from_secs(5),
            tm.init_download(&path_str(&fifo), None),
        )
        .await;
        let Ok(answer) = answer else {
            // Give the stuck open a writer so the test run can end.
            let _ = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&fifo);
            panic!("init_download waited on a FIFO");
        };
        assert_eq!(answer.unwrap_err().code, "NOT_A_REGULAR_FILE");
        let entries = tm.activity_log.read_since(0, 10).await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].activity_type, ActivityType::FileRead);
        assert!(entries[0].summary.starts_with("read refused"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_pseudo_file_and_a_protected_path_are_refused_at_init() {
        let tm = manager(4);
        let e = tm.init_download("/proc/net/arp", None).await.unwrap_err();
        assert_eq!(e.code, "NOT_A_REGULAR_FILE");
        assert!(e.message.contains("procfs"), "{}", e.message);
        let e = tm
            .init_download("/proc/self/environ", None)
            .await
            .unwrap_err();
        assert_eq!(e.code, "PROTECTED_PATH");
        let e = tm.init_download("/dev/zero", None).await.unwrap_err();
        assert_eq!(e.code, "NOT_A_REGULAR_FILE");
        assert_eq!(tm.activity_log.read_since(0, 10).await.len(), 3);
        assert_eq!(tm.hashing.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_hash_holds_its_place_under_the_limit_while_it_runs() {
        let dir = scratch("places");
        let (tm, hold) = held_manager(4);

        let mut inits = Vec::new();
        for i in 0..4 {
            let file = dir.join(format!("f{i}.bin"));
            std::fs::write(&file, vec![b'0' + i; 3000]).unwrap();
            inits.push(spawn_init(&tm, &file));
        }
        // All four are inside their hash, each with its place.
        hold.until_held(4).await;
        assert_eq!(tm.hashing.load(Ordering::SeqCst), 4);

        // Four hashes hold the four places: a fifth init, and an upload,
        // are past the limit.
        let fifth = dir.join("f5.bin");
        std::fs::write(&fifth, b"five").unwrap();
        let e = tm.init_download(&path_str(&fifth), None).await.unwrap_err();
        assert_eq!(e.code, "MAX_TRANSFERS");
        let e = tm
            .init_upload(InitUpload {
                path: path_str(&dir),
                filename: "up.bin".to_string(),
                file_size: 10,
                file_hash: String::new(),
                chunk_size: 1024,
                total_chunks: 1,
                mode: None,
            })
            .await
            .unwrap_err();
        assert_eq!(e.code, "MAX_TRANSFERS");

        hold.release(4);
        for init in inits {
            let r = init.await.unwrap().unwrap();
            assert_eq!(r.file_size, 3000);
        }
        // Registered, each counts once, as active.
        assert_eq!(tm.hashing.load(Ordering::SeqCst), 0);
        assert_eq!(tm.list().await.transfers.len(), 4);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn the_hash_reads_the_file_judged_to_the_size_its_stat_gave() {
        let dir = scratch("judged");
        let (tm, hold) = held_manager(4);
        let original: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();

        // A log that grows while its hash waits is hashed to its first size.
        let grows = dir.join("grows.log");
        std::fs::write(&grows, &original).unwrap();
        let init = spawn_init(&tm, &grows);
        hold.until_held(1).await;
        let mut log = std::fs::OpenOptions::new()
            .append(true)
            .open(&grows)
            .unwrap();
        std::io::Write::write_all(&mut log, b"written after the stat").unwrap();
        hold.release(1);
        let init = init.await.unwrap().unwrap();
        assert_eq!(init.file_size, 3000);
        assert_eq!(init.file_hash, hasher::hash_bytes(&original));

        // Another file renamed over the path while the hash waits: the hash
        // still reads the file the gate judged.
        let swapped = dir.join("swapped.bin");
        std::fs::write(&swapped, &original).unwrap();
        let init = spawn_init(&tm, &swapped);
        hold.until_held(1).await;
        let other = dir.join("other.bin");
        std::fs::write(&other, vec![b'z'; 3000]).unwrap();
        std::fs::rename(&other, &swapped).unwrap();
        hold.release(1);
        let init = init.await.unwrap().unwrap();
        assert_eq!(init.file_size, 3000);
        assert_eq!(init.file_hash, hasher::hash_bytes(&original));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_chunk_size_below_the_floor_is_raised_to_it() {
        let dir = scratch("floor");
        let file = dir.join("data.bin");
        let data: Vec<u8> = (0..2500u32).map(|i| (i % 249) as u8).collect();
        std::fs::write(&file, &data).unwrap();
        let tm = manager(4);
        // 0 divided the size by zero once the place was taken, and the
        // caller got no answer; 1 made a chunk of every byte. Each is
        // raised to 1 KiB, as an upload's is.
        for asked in [0, 1, MIN_CHUNK_SIZE - 1] {
            let init = tm
                .init_download(&path_str(&file), Some(asked))
                .await
                .unwrap();
            assert_eq!(init.chunk_size, MIN_CHUNK_SIZE, "{asked}");
            assert_eq!(init.total_chunks, 3, "{asked}");
            let mut got = Vec::new();
            for i in 0..init.total_chunks {
                got.extend(tm.serve_chunk(&init.transfer_id, i).await.unwrap().1);
            }
            assert_eq!(got, data, "{asked}");
        }
        assert_eq!(tm.hashing.load(Ordering::SeqCst), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_download_hashes_and_serves_the_bytes_its_stat_gave() {
        let dir = scratch("roundtrip");
        let file = dir.join("data.bin");
        let data: Vec<u8> = (0..2500u32).map(|i| (i % 253) as u8).collect();
        std::fs::write(&file, &data).unwrap();
        let tm = manager(4);
        let init = tm
            .init_download(&path_str(&file), Some(1024))
            .await
            .unwrap();
        assert_eq!(init.total_chunks, 3);
        assert_eq!(init.file_hash, hasher::hash_bytes(&data));

        let mut got = Vec::new();
        for i in 0..3 {
            let (header, bytes) = tm.serve_chunk(&init.transfer_id, i).await.unwrap();
            assert_eq!(header.chunk_hash, hasher::hash_bytes(&bytes));
            got.extend(bytes);
        }
        assert_eq!(got, data);

        // An empty file is still one empty chunk.
        let empty = dir.join("empty.bin");
        std::fs::write(&empty, b"").unwrap();
        let init = tm.init_download(&path_str(&empty), None).await.unwrap();
        assert_eq!(init.total_chunks, 1);
        let (_, bytes) = tm.serve_chunk(&init.transfer_id, 0).await.unwrap();
        assert!(bytes.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_chunk_comes_only_from_the_file_that_was_hashed() {
        let dir = scratch("changed");
        let file = dir.join("data.bin");
        std::fs::write(&file, vec![b'a'; 3000]).unwrap();
        // One place: the transfer that fails must give it back.
        let tm = manager(1);
        let init = tm
            .init_download(&path_str(&file), Some(1024))
            .await
            .unwrap();
        tm.serve_chunk(&init.transfer_id, 0).await.unwrap();

        // Another inode at the path, with the same size and mtime.
        let mtime = std::fs::metadata(&file).unwrap().modified().unwrap();
        let next = dir.join("next.bin");
        std::fs::write(&next, vec![b'b'; 3000]).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&next)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        std::fs::rename(&next, &file).unwrap();
        let e = tm.serve_chunk(&init.transfer_id, 1).await.unwrap_err();
        assert_eq!(e.code, "FILE_CHANGED");

        // The transfer failed with it: every later chunk says so, and its
        // place is free for the next download.
        assert_eq!(tm.status(&init.transfer_id).await.unwrap().phase, "failed");
        let e = tm.serve_chunk(&init.transfer_id, 2).await.unwrap_err();
        assert_eq!(e.code, "FILE_CHANGED");
        let other = dir.join("other.bin");
        std::fs::write(&other, vec![b'c'; 3000]).unwrap();
        let init = tm
            .init_download(&path_str(&other), Some(1024))
            .await
            .unwrap();

        // A FIFO at the path is not opened.
        std::fs::remove_file(&other).unwrap();
        nix::unistd::mkfifo(&other, nix::sys::stat::Mode::S_IRWXU).unwrap();
        let answer =
            tokio::time::timeout(Duration::from_secs(5), tm.serve_chunk(&init.transfer_id, 0))
                .await;
        let Ok(answer) = answer else {
            let _ = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&other);
            panic!("serve_chunk waited on a FIFO");
        };
        assert_eq!(answer.unwrap_err().code, "FILE_CHANGED");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
