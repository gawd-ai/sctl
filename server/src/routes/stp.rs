//! STP (Secure Transfer Protocol) HTTP endpoints for direct device access.
//!
//! These endpoints call directly into `TransferManager` from `AppState`.
//! Chunk endpoints use raw `application/octet-stream` bodies — no JSON wrapping.

use axum::{
    body::Body,
    extract::{Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    Json,
};
use serde_json::{json, Value};

use crate::gawdxfer::types::{InitDownload, InitUpload, TransferError};
use crate::AppState;

/// `POST /api/stp/download` — init a chunked download.
pub async fn init_download(
    State(state): State<AppState>,
    Json(req): Json<InitDownload>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let result = state
        .transfer_manager
        .init_download(&req.path, req.chunk_size)
        .await
        .map_err(transfer_error_to_http)?;
    Ok(Json(serde_json::to_value(&result).unwrap()))
}

/// `POST /api/stp/upload` — init a chunked upload.
pub async fn init_upload(
    State(state): State<AppState>,
    Json(req): Json<InitUpload>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let result = state
        .transfer_manager
        .init_upload(req)
        .await
        .map_err(transfer_error_to_http)?;
    Ok(Json(serde_json::to_value(&result).unwrap()))
}

/// `GET /api/stp/chunk/{xfer}/{idx}` — serve a chunk (binary body + X-Gx-Chunk-Hash header).
pub async fn get_chunk(
    State(state): State<AppState>,
    AxumPath((xfer, idx)): AxumPath<(String, u32)>,
) -> Result<Response, (StatusCode, Json<Value>)> {
    let (header, data) = state
        .transfer_manager
        .serve_chunk(&xfer, idx)
        .await
        .map_err(transfer_error_to_http)?;

    Ok(Response::builder()
        .header("Content-Type", "application/octet-stream")
        .header("X-Gx-Chunk-Hash", &header.chunk_hash)
        .header("X-Gx-Chunk-Index", header.chunk_index.to_string())
        .header("X-Gx-Transfer-Id", &header.transfer_id)
        .header("Content-Length", data.len())
        .body(Body::from(data))
        .unwrap())
}

/// `POST /api/stp/chunk/{xfer}/{idx}` — receive a chunk (binary body, X-Gx-Chunk-Hash header).
pub async fn post_chunk(
    State(state): State<AppState>,
    AxumPath((xfer, idx)): AxumPath<(String, u32)>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let chunk_hash = headers
        .get("X-Gx-Chunk-Hash")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    if chunk_hash.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Missing X-Gx-Chunk-Hash header", "code": "INVALID_REQUEST"})),
        ));
    }

    let ack = state
        .transfer_manager
        .receive_chunk(&xfer, idx, &chunk_hash, &body)
        .await
        .map_err(transfer_error_to_http)?;
    Ok(Json(serde_json::to_value(&ack).unwrap()))
}

/// `POST /api/stp/resume/{xfer}` — resume a paused transfer.
pub async fn resume_transfer(
    State(state): State<AppState>,
    AxumPath(xfer): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let result = state
        .transfer_manager
        .resume(&xfer)
        .await
        .map_err(transfer_error_to_http)?;
    Ok(Json(serde_json::to_value(&result).unwrap()))
}

/// `GET /api/stp/status/{xfer}` — get transfer status.
pub async fn transfer_status(
    State(state): State<AppState>,
    AxumPath(xfer): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let result = state
        .transfer_manager
        .status(&xfer)
        .await
        .map_err(transfer_error_to_http)?;
    Ok(Json(serde_json::to_value(&result).unwrap()))
}

/// `GET /api/stp/transfers` — list all transfers.
pub async fn list_transfers(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let result = state.transfer_manager.list().await;
    Ok(Json(serde_json::to_value(&result).unwrap()))
}

/// `DELETE /api/stp/{xfer}` — abort a transfer.
pub async fn abort_transfer(
    State(state): State<AppState>,
    AxumPath(xfer): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    state
        .transfer_manager
        .abort(&xfer, "client abort")
        .await
        .map_err(transfer_error_to_http)?;
    Ok(Json(json!({"ok": true, "transfer_id": xfer})))
}

/// Convert a gawdxfer `TransferError` to an HTTP error response.
#[allow(clippy::needless_pass_by_value)]
fn transfer_error_to_http(e: TransferError) -> (StatusCode, Json<Value>) {
    let status = match e.code.as_str() {
        "FILE_NOT_FOUND" | "TRANSFER_NOT_FOUND" => StatusCode::NOT_FOUND,
        "PERMISSION_DENIED" => StatusCode::FORBIDDEN,
        "FILE_TOO_LARGE" | "INVALID_PATH" | "INVALID_REQUEST" | "HASH_MISMATCH"
        | "CHUNK_INTEGRITY" | "FILE_CHANGED" => StatusCode::BAD_REQUEST,
        "DISK_FULL" => StatusCode::INSUFFICIENT_STORAGE,
        "MAX_TRANSFERS" => StatusCode::TOO_MANY_REQUESTS,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (
        status,
        Json(json!({
            "error": e.message,
            "code": e.code,
            "transfer_id": e.transfer_id,
            "recoverable": e.recoverable,
        })),
    )
}
