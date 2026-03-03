//! Reverse proxy for the sctlin web UI (adapter-node on localhost:3000).
//!
//! Mounted as a fallback on the relay server so that `/sctlin/*` is forwarded
//! to the Node.js process serving the `SvelteKit` build.

use axum::body::Body;
use axum::extract::Request;
use axum::response::{IntoResponse, Response};
use hyper::StatusCode;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

/// Reverse-proxy handler: forwards the request as-is to `http://127.0.0.1:3000`.
pub async fn sctlin_proxy(req: Request) -> Response {
    let client = Client::builder(TokioExecutor::new()).build_http();

    let path_and_query = req
        .uri()
        .path_and_query()
        .map_or("/", axum::http::uri::PathAndQuery::as_str);

    let uri: hyper::Uri = match format!("http://127.0.0.1:3000{path_and_query}").parse() {
        Ok(u) => u,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };

    // Rebuild the request with the upstream URI, preserving method + headers + body.
    let (parts, body) = req.into_parts();
    let mut builder = hyper::Request::builder().method(parts.method).uri(uri);
    *builder.headers_mut().unwrap() = parts.headers;
    let upstream_req = builder.body(body).unwrap();

    match client.request(upstream_req).await {
        Ok(resp) => {
            let (parts, body) = resp.into_parts();
            Response::from_parts(parts, Body::new(body))
        }
        Err(_) => StatusCode::BAD_GATEWAY.into_response(),
    }
}
