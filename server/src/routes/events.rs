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
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use futures::stream::Stream;
use std::convert::Infallible;
use std::sync::atomic::Ordering;

use crate::AppState;

/// Maximum concurrent SSE connections before rejecting with 429.
const MAX_SSE_CONNECTIONS: u32 = 64;

/// `GET /api/events` — SSE event stream.
pub async fn event_stream(State(state): State<AppState>) -> impl IntoResponse {
    let current = state.sse_connections.load(Ordering::Relaxed);
    if current >= MAX_SSE_CONNECTIONS {
        return Err((StatusCode::TOO_MANY_REQUESTS, "Too many SSE connections"));
    }
    state.sse_connections.fetch_add(1, Ordering::Relaxed);

    let rx = state.session_events.subscribe();
    let counter = state.sse_connections.clone();

    let stream = futures::stream::unfold((rx, counter), |(mut rx, counter)| async move {
        match rx.recv().await {
            Ok(value) => {
                let event_type = value["type"].as_str().unwrap_or("message").to_string();
                let data = serde_json::to_string(&value).unwrap_or_default();
                let event = Event::default().event(event_type).data(data);
                Some((Ok(event), (rx, counter)))
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                // Notify the client they missed events
                let event = Event::default()
                    .event("error")
                    .data(format!(r#"{{"code":"LAGGED","missed":{n}}}"#));
                Some((Ok(event), (rx, counter)))
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                counter.fetch_sub(1, Ordering::Relaxed);
                None
            }
        }
    });

    // Wrap stream to decrement counter when the SSE stream is dropped
    let counter_for_drop = state.sse_connections.clone();
    let stream = DropCounterStream {
        inner: Box::pin(stream),
        counter: counter_for_drop,
        decremented: false,
    };

    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::default().interval(std::time::Duration::from_secs(15))))
}

/// Wrapper that decrements the SSE connection counter when the stream is dropped.
struct DropCounterStream<S> {
    inner: std::pin::Pin<Box<S>>,
    counter: std::sync::Arc<std::sync::atomic::AtomicU32>,
    decremented: bool,
}

impl<S: Stream<Item = Result<Event, Infallible>>> Stream for DropCounterStream<S> {
    type Item = Result<Event, Infallible>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let result = self.inner.as_mut().poll_next(cx);
        if let std::task::Poll::Ready(None) = &result {
            if !self.decremented {
                self.counter.fetch_sub(1, Ordering::Relaxed);
                self.decremented = true;
            }
        }
        result
    }
}

impl<S> Drop for DropCounterStream<S> {
    fn drop(&mut self) {
        if !self.decremented {
            self.counter.fetch_sub(1, Ordering::Relaxed);
            self.decremented = true;
        }
    }
}
