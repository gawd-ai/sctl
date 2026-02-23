//! Server-Sent Events (SSE) endpoint.
//!
//! `GET /api/events` — push-based event stream for dashboards, monitoring, and
//! webhooks. Subscribes to the same `session_events` broadcast channel that WS
//! clients use, so all session lifecycle, activity, and AI status events flow
//! through.
//!
//! Not proxied through the tunnel relay (SSE is a long-lived streaming response
//! incompatible with the REST-over-WS relay pattern).

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::stream::Stream;
use std::convert::Infallible;

use crate::AppState;

/// `GET /api/events` — SSE event stream.
pub async fn event_stream(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.session_events.subscribe();

    let stream = futures::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(value) => {
                    let event_type = value["type"].as_str().unwrap_or("message").to_string();
                    let data = serde_json::to_string(&value).unwrap_or_default();
                    let event = Event::default().event(event_type).data(data);
                    return Some((Ok(event), rx));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // Channel overflow — skip missed messages, loop again
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    return None;
                }
            }
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::default().interval(std::time::Duration::from_secs(15)))
}
