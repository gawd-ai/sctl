//! Unified error response shape for the HTTP/REST surface.
//!
//! Before this module existed, every route hand-rolled its own JSON error
//! payload via `json!({"error": ...})` or `json!({"code": ..., "message": ...})`
//! — the two shapes coexisted and the web client had to handle both.
//!
//! [`ApiError`] is the single shape every route now returns on failure. It
//! gets a `ts_rs::TS` derive so the web client gets a typed `ApiError`
//! definition through the existing `cargo test export_bindings` pipeline.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde::{Deserialize, Serialize};

/// Canonical error response for the REST surface.
///
/// Wire format: `{"code": "SCREAMING_SNAKE", "message": "human text", "detail"?: {...}}`.
/// Variants without structured detail simply omit the field.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, optional_fields))]
pub struct ApiError {
    /// Stable machine-readable identifier (e.g. `"AUTH_MISSING_TOKEN"`,
    /// `"SESSION_NOT_FOUND"`). Screaming snake case by convention.
    pub code: String,
    /// Human-readable explanation. Safe to display in UIs.
    pub message: String,
    /// Optional structured context — request inputs, downstream errors,
    /// retry hints. Renders as `unknown` in TS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(type = "unknown"))]
    pub detail: Option<serde_json::Value>,
}

impl ApiError {
    /// Build an error with just a code + message.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            detail: None,
        }
    }

    /// Attach structured detail (chunk index, retry-after, source error, etc.).
    #[must_use]
    pub fn with_detail(mut self, detail: serde_json::Value) -> Self {
        self.detail = Some(detail);
        self
    }

    /// Pair with a status code for return from a route handler.
    pub fn into_response_with(self, status: StatusCode) -> (StatusCode, Json<Self>) {
        (status, Json(self))
    }
}

impl IntoResponse for ApiError {
    /// Default conversion uses 500 — most code paths should call
    /// [`ApiError::into_response_with`] explicitly with the right code.
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(self)).into_response()
    }
}

/// Common error code constants, kept here so the catalog of codes lives in
/// one place. Routes can use either these or a literal string — the wire
/// format is identical.
pub mod codes {
    pub const AUTH_MISSING_TOKEN: &str = "AUTH_MISSING_TOKEN";
    pub const AUTH_INVALID_TOKEN: &str = "AUTH_INVALID_TOKEN";
    pub const INVALID_REQUEST: &str = "INVALID_REQUEST";
    pub const INVALID_PATH: &str = "INVALID_PATH";
    pub const INVALID_MODE: &str = "INVALID_MODE";
    pub const INVALID_CONTENT: &str = "INVALID_CONTENT";
    pub const FILE_NOT_FOUND: &str = "FILE_NOT_FOUND";
    pub const FILE_TOO_LARGE: &str = "FILE_TOO_LARGE";
    pub const IS_DIRECTORY: &str = "IS_DIRECTORY";
    pub const NOT_A_DIRECTORY: &str = "NOT_A_DIRECTORY";
    pub const NOT_FOUND: &str = "NOT_FOUND";
    pub const PERMISSION_DENIED: &str = "PERMISSION_DENIED";
    pub const IO_ERROR: &str = "IO_ERROR";
    pub const SESSION_NOT_FOUND: &str = "SESSION_NOT_FOUND";
    pub const EXEC_FAILED: &str = "EXEC_FAILED";
    pub const TIMEOUT: &str = "TIMEOUT";
    pub const BATCH_TOO_LARGE: &str = "BATCH_TOO_LARGE";
    /// The device's payload predates the generic `http.request` passthrough
    /// (relay-only code; the device answers again once it activates 0.6.0+).
    pub const DEVICE_PAYLOAD_OUTDATED: &str = "DEVICE_PAYLOAD_OUTDATED";
    /// The loaded comms plugin does not implement the requested capability.
    pub const COMMS_CAPABILITY_UNSUPPORTED: &str = "COMMS_CAPABILITY_UNSUPPORTED";
    pub const MULTIPART_ERROR: &str = "MULTIPART_ERROR";
    pub const AI_NOT_ALLOWED: &str = "AI_NOT_ALLOWED";
    pub const MODEM_UNAVAILABLE: &str = "MODEM_UNAVAILABLE";
    pub const MODEM_AT_FAILED: &str = "MODEM_AT_FAILED";
    pub const TUNNEL_CONNECTED: &str = "TUNNEL_CONNECTED";
    pub const SCAN_RUNNING: &str = "SCAN_RUNNING";
    pub const INVALID_URL: &str = "INVALID_URL";
    pub const FETCH_FAILED: &str = "FETCH_FAILED";
    /// The peer presented a certificate that does not match its recorded pin.
    /// Never retry through this — it is the one signal that distinguishes an
    /// interception from an ordinary connection failure.
    pub const CERT_PIN_MISMATCH: &str = "CERT_PIN_MISMATCH";
    /// No CA path, no pin, and TOFU was not requested.
    pub const CERT_UNTRUSTED: &str = "CERT_UNTRUSTED";
    /// The server's own configuration is broken (e.g. no API key set).
    pub const SERVER_CONFIG_ERROR: &str = "SERVER_CONFIG_ERROR";
    /// Connection-count limit reached (SSE, sessions).
    pub const TOO_MANY_CONNECTIONS: &str = "TOO_MANY_CONNECTIONS";
    /// The infra monitoring subsystem is not available on this device.
    pub const INFRA_UNAVAILABLE: &str = "INFRA_UNAVAILABLE";
}
