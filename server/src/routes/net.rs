//! Network state endpoint.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;

use crate::error::{codes, ApiError};
use crate::netwatch;
use crate::tunnel::net_state::{self, NetStateMessage};
use crate::AppState;

/// `GET /api/net`: the device's network dumped from the kernel now, as the
/// `net.state` message the tunnel pushes. `seq` is 0: a fresh dump is
/// outside the pushed sequence.
///
/// Returns 503 in relay mode, which does not watch the network, and when the
/// kernel's route socket is unavailable.
pub async fn net(
    State(state): State<AppState>,
) -> Result<Json<NetStateMessage>, (StatusCode, Json<ApiError>)> {
    let Some(watch) = &state.netwatch else {
        return Err(ApiError::new(
            codes::NET_UNAVAILABLE,
            "the network is not watched in relay mode",
        )
        .into_response_with(StatusCode::SERVICE_UNAVAILABLE));
    };
    let mut fresh = netwatch::dump().await.map_err(|e| {
        ApiError::new(
            codes::NET_UNAVAILABLE,
            format!("the kernel's route socket is unavailable: {e}"),
        )
        .into_response_with(StatusCode::SERVICE_UNAVAILABLE)
    })?;
    // The published state carries the agent's start; before the first one,
    // derive it from the uptime.
    let published_boot = watch.borrow().as_ref().map(|s| s.boot);
    fresh.boot = published_boot.unwrap_or_else(|| {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        now_ms.saturating_sub(state.start_time.elapsed().as_millis() as u64)
    });
    let lookups = net_state::look_up(&fresh, state.tunnel_stats.path()).await;
    Ok(Json(net_state::build(
        &fresh,
        lookups,
        crate::infra::now_iso(),
    )))
}
