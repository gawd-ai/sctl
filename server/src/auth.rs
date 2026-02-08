//! Pre-shared API key authentication.
//!
//! All endpoints except `/api/health` and the WebSocket upgrade require a
//! `Authorization: Bearer <key>` header. The WebSocket path uses a `?token=`
//! query parameter instead (browsers can't set headers on WebSocket upgrades).

use axum::{
    extract::Request,
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

/// Axum middleware that rejects requests without a valid `Authorization: Bearer`
/// header. The expected key is injected via the [`ApiKey`] extension.
///
/// # Error responses
///
/// - `401 Unauthorized` — header missing or malformed
/// - `403 Forbidden` — key present but invalid
/// - `500 Internal Server Error` — [`ApiKey`] extension not found (misconfiguration)
pub async fn require_api_key(request: Request, next: Next) -> Response {
    let api_key = match request.extensions().get::<ApiKey>() {
        Some(key) => key.0.clone(),
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Server configuration error"})),
            )
                .into_response();
        }
    };

    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok());

    let provided = match auth_header {
        Some(h) if h.starts_with("Bearer ") => &h[7..],
        _ => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "Missing or invalid Authorization header"})),
            )
                .into_response();
        }
    };

    if !constant_time_eq(api_key.as_bytes(), provided.as_bytes()) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "Invalid API key"})),
        )
            .into_response();
    }

    next.run(request).await
}

/// Constant-time byte comparison to prevent timing side-channel attacks.
///
/// Always iterates over the full length of `expected` regardless of `provided`
/// length, so an attacker cannot determine the key length from response times.
pub fn constant_time_eq(expected: &[u8], provided: &[u8]) -> bool {
    let mut diff = u8::from(expected.len() != provided.len());
    // Always iterate over the expected key length to avoid timing leak
    for i in 0..expected.len() {
        let p = if i < provided.len() {
            provided[i]
        } else {
            0xff
        };
        diff |= expected[i] ^ p;
    }
    diff == 0
}

/// Extension type carrying the expected API key, injected into the router
/// layer so [`require_api_key`] can access it without touching `AppState`.
#[derive(Clone)]
pub struct ApiKey(pub String);
