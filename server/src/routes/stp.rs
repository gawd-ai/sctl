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

use crate::error::{codes, ApiError};
use crate::gawdxfer::types::{InitDownload, InitUpload, TransferError};
use crate::AppState;

type ApiResult<T> = Result<Json<T>, (StatusCode, Json<ApiError>)>;

/// `POST /api/stp/download` — init a chunked download.
pub async fn init_download(
    State(state): State<AppState>,
    Json(req): Json<InitDownload>,
) -> ApiResult<Value> {
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
) -> ApiResult<Value> {
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
) -> Result<Response, (StatusCode, Json<ApiError>)> {
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
) -> ApiResult<Value> {
    let chunk_hash = headers
        .get("X-Gx-Chunk-Hash")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    if chunk_hash.is_empty() {
        return Err(
            ApiError::new(codes::INVALID_REQUEST, "Missing X-Gx-Chunk-Hash header")
                .into_response_with(StatusCode::BAD_REQUEST),
        );
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
) -> ApiResult<Value> {
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
) -> ApiResult<Value> {
    let result = state
        .transfer_manager
        .status(&xfer)
        .await
        .map_err(transfer_error_to_http)?;
    Ok(Json(serde_json::to_value(&result).unwrap()))
}

/// `GET /api/stp/transfers` — list all transfers.
pub async fn list_transfers(State(state): State<AppState>) -> ApiResult<Value> {
    let result = state.transfer_manager.list().await;
    Ok(Json(serde_json::to_value(&result).unwrap()))
}

/// `DELETE /api/stp/{xfer}` — abort a transfer.
pub async fn abort_transfer(
    State(state): State<AppState>,
    AxumPath(xfer): AxumPath<String>,
) -> ApiResult<Value> {
    state
        .transfer_manager
        .abort(&xfer, "client abort")
        .await
        .map_err(transfer_error_to_http)?;
    Ok(Json(json!({"ok": true, "transfer_id": xfer})))
}

/// Convert a gawdxfer `TransferError` to an HTTP error response.
///
/// Transfer-specific detail (transfer_id, recoverable flag) lands in
/// `ApiError.detail` so the wire format stays unified with the rest of
/// the REST surface while keeping the gawdxfer-specific context.
#[allow(clippy::needless_pass_by_value)]
fn transfer_error_to_http(e: TransferError) -> (StatusCode, Json<ApiError>) {
    let status = match e.code.as_str() {
        "FILE_NOT_FOUND" | "TRANSFER_NOT_FOUND" => StatusCode::NOT_FOUND,
        codes::PERMISSION_DENIED | codes::PROTECTED_PATH => StatusCode::FORBIDDEN,
        "FILE_TOO_LARGE"
        | "INVALID_PATH"
        | "INVALID_REQUEST"
        | "HASH_MISMATCH"
        | "CHUNK_INTEGRITY"
        | "FILE_CHANGED"
        | codes::NOT_A_REGULAR_FILE => StatusCode::BAD_REQUEST,
        "DISK_FULL" => StatusCode::INSUFFICIENT_STORAGE,
        "MAX_TRANSFERS" => StatusCode::TOO_MANY_REQUESTS,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    ApiError::new(e.code, e.message)
        .with_detail(json!({
            "transfer_id": e.transfer_id,
            "recoverable": e.recoverable,
        }))
        .into_response_with(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status_of(code: &str) -> StatusCode {
        transfer_error_to_http(TransferError {
            transfer_id: String::new(),
            code: code.to_string(),
            message: String::new(),
            recoverable: false,
        })
        .0
    }

    #[test]
    fn the_gates_refusals_map_to_403_and_400() {
        assert_eq!(status_of("PROTECTED_PATH"), StatusCode::FORBIDDEN);
        assert_eq!(status_of("NOT_A_REGULAR_FILE"), StatusCode::BAD_REQUEST);
        assert_eq!(status_of("FILE_CHANGED"), StatusCode::BAD_REQUEST);
        assert_eq!(status_of("MAX_TRANSFERS"), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            status_of("SOMETHING_NEW"),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    /// `POST /api/stp/download` with a `chunk_size` too large for a `u32` is
    /// answered as the 4 MiB ceiling, as any size above it is (TRD-9 3.1).
    /// The request used to be refused, 422, before the init saw it.
    #[tokio::test]
    async fn a_download_init_answers_any_size_above_the_ceiling_as_the_ceiling() {
        use std::sync::Arc;
        use tower::ServiceExt as _;

        let dir = std::env::temp_dir().join(format!("sctl-stp-ceiling-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("data.bin");
        std::fs::write(&file, vec![7_u8; 3000]).unwrap();
        let mut config: crate::config::Config = toml::from_str("").unwrap();
        config.server.data_dir = dir.join("data").display().to_string();
        let gate = Arc::new(crate::file_access::Gate::for_agent(&config, None, None));
        let state = AppState::for_tests(config, gate);
        let router = axum::Router::new()
            .route("/api/stp/download", axum::routing::post(init_download))
            .with_state(state.clone());

        let ceiling = crate::routes::files::TUNNEL_READ_CAP as u64;
        for (asked, answered) in [
            (4096, 4096),
            (ceiling + 1, ceiling),
            (u64::from(u32::MAX), ceiling),
            (u64::from(u32::MAX) + 1, ceiling),
            (u64::from(u32::MAX) + 1001, ceiling),
            (u64::MAX, ceiling),
        ] {
            let body = json!({"path": file.display().to_string(), "chunk_size": asked});
            let resp = router
                .clone()
                .oneshot(
                    axum::http::Request::post("/api/stp/download")
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{asked}");
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let out: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(out["chunk_size"], answered, "{asked}");
            state
                .transfer_manager
                .abort(out["transfer_id"].as_str().unwrap(), "test")
                .await
                .unwrap();
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
