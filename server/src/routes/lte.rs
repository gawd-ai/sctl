//! LTE-compatible projections and controls backed by the external comms provider.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use sctl_comms_abi::{capabilities, methods};
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::info;

use std::time::Duration;

use crate::error::{codes, ApiError};
use crate::AppState;

type ApiErrorResponse = (StatusCode, Json<ApiError>);
type ApiResult<T> = Result<Json<T>, ApiErrorResponse>;
type BandSelection = (Vec<u16>, Option<u16>);

/// `GET /api/lte` — returns current cellular link quality, modem identity,
/// band history, and scan status when the active comms provider supports it.
pub async fn lte(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> ApiResult<Value> {
    ensure_lte_configured(&state)?;
    ensure_capability(&state, capabilities::LINK_CELLULAR).await?;

    if params.get("refresh").is_some_and(|v| v == "true") {
        if let (Some(client), Some(comms_state)) = (&state.comms_client, &state.comms_state) {
            crate::comms::poll_link(client, comms_state, &state.tunnel_stats, true).await;
        }
    }

    let Some(comms_state) = &state.comms_state else {
        return comms_unavailable();
    };
    let (mut snapshot, watchdog) = {
        let guard = comms_state.lock().await;
        (
            guard
                .lte
                .clone()
                .unwrap_or_else(crate::comms::starting_lte_response),
            guard.watchdog.clone(),
        )
    };
    // The plugin emits `"watchdog": null`; the watchdog lives server-side, so
    // overlay its snapshot here.
    if let (Some(obj), Some(wd)) = (snapshot.as_object_mut(), watchdog) {
        obj.insert("watchdog".to_string(), wd);
    }

    Ok(Json(snapshot))
}

#[derive(Deserialize)]
pub struct SetBandsRequest {
    pub mode: String,
    pub bands: Option<Vec<u16>>,
    pub priority_band: Option<u16>,
    pub force: Option<bool>,
}

/// `POST /api/lte/bands` — switch between locked and auto band modes when the
/// active provider supports cellular band control.
pub async fn set_bands(
    State(state): State<AppState>,
    Json(req): Json<SetBandsRequest>,
) -> ApiResult<Value> {
    ensure_lte_configured(&state)?;
    ensure_capability(&state, capabilities::CELLULAR_BAND_CONTROL).await?;

    if !req.force.unwrap_or(false)
        && state
            .tunnel_stats
            .connected
            .load(std::sync::atomic::Ordering::Relaxed)
    {
        return Err(ApiError::new(
            codes::TUNNEL_CONNECTED,
            "band changes blocked while tunnel is connected. Use force:true to override.",
        )
        .into_response_with(StatusCode::CONFLICT));
    }

    let (bands, priority_band) = validate_band_request(&req)?;
    let client = state.comms_client.as_ref().ok_or_else(unavailable_pair)?;
    let result = client
        .call(
            methods::CELLULAR_SET_BANDS,
            json!({
                "mode": req.mode,
                "bands": bands,
                "priority_band": priority_band,
                "force": req.force.unwrap_or(false),
                "tunnel_connected": state.tunnel_stats.connected.load(std::sync::atomic::Ordering::Relaxed),
            }),
        )
        .await
        .map_err(provider_error)?;

    if let (Some(comms_state), Some(snapshot)) = (&state.comms_state, result.get("snapshot")) {
        comms_state.lock().await.lte = Some(snapshot.clone());
    }

    Ok(Json(result))
}

#[derive(Deserialize)]
pub struct StartScanRequest {
    pub bands: Option<Vec<u16>>,
    pub include_speed_test: Option<bool>,
    pub force: Option<bool>,
}

/// `POST /api/lte/scan` — start a background cellular band scan.
pub async fn start_scan(
    State(state): State<AppState>,
    Json(req): Json<StartScanRequest>,
) -> ApiResult<Value> {
    ensure_lte_configured(&state)?;
    ensure_capability(&state, capabilities::CELLULAR_SCAN).await?;

    if !req.force.unwrap_or(false)
        && state
            .tunnel_stats
            .connected
            .load(std::sync::atomic::Ordering::Relaxed)
    {
        return Err(ApiError::new(
            codes::TUNNEL_CONNECTED,
            "band scan blocked while tunnel is connected. Use force:true to override.",
        )
        .into_response_with(StatusCode::CONFLICT));
    }

    let bands_to_scan = req.bands.unwrap_or_else(default_lte_bands);
    let client = state.comms_client.as_ref().ok_or_else(unavailable_pair)?;
    let result = client
        .call(
            methods::CELLULAR_SCAN,
            json!({
                "bands": bands_to_scan,
                "include_speed_test": req.include_speed_test.unwrap_or(false),
                "force": req.force.unwrap_or(false),
                "tunnel_connected": state.tunnel_stats.connected.load(std::sync::atomic::Ordering::Relaxed),
            }),
        )
        .await
        .map_err(provider_error)?;

    Ok(Json(result))
}

/// `POST /api/lte/speedtest` — run a throughput test through the configured
/// link interface.
pub async fn speed_test(State(state): State<AppState>) -> ApiResult<Value> {
    ensure_lte_configured(&state)?;
    let client = state.comms_client.as_ref().ok_or_else(unavailable_pair)?;
    let lte_config = state.config.lte.as_ref().expect("checked above");
    if lte_config.speed_test_url.is_none() && lte_config.speed_test_upload_url.is_none() {
        return Err(ApiError::new(
            codes::INVALID_REQUEST,
            "no speed_test_url or speed_test_upload_url configured in [lte]",
        )
        .into_response_with(StatusCode::BAD_REQUEST));
    }
    let result = client
        .call_with_timeout(
            methods::LINK_SPEED_TEST,
            json!({
                "interface": lte_config.interface.clone(),
                "download_url": lte_config.speed_test_url.clone(),
                "upload_url": lte_config.speed_test_upload_url.clone(),
            }),
            Duration::from_mins(5),
        )
        .await
        .map_err(provider_error)?;

    Ok(Json(result))
}

/// `POST /api/lte/usb_cycle` — manually trigger a provider recovery action.
pub async fn manual_usb_cycle(State(state): State<AppState>) -> ApiResult<Value> {
    ensure_lte_configured(&state)?;
    ensure_capability(&state, capabilities::RECOVERY_USB_CYCLE).await?;

    let client = state.comms_client.as_ref().ok_or_else(unavailable_pair)?;
    info!("api.lte.manual_usb_cycle: delegating to comms provider");
    let result = client
        .call_with_timeout(
            methods::RECOVERY_USB_CYCLE,
            json!({}),
            Duration::from_mins(2),
        )
        .await
        .map_err(provider_error)?;

    Ok(Json(result))
}

/// The most of the watchdog's history one read returns: the size it
/// rotates at, and one more entry's room. A file that is not the
/// watchdog's own is cut here.
const WATCHDOG_HISTORY_READ_CAP: usize =
    crate::lte_watchdog::HISTORY_MAX_BYTES as usize + 64 * 1024;

/// `GET /api/lte/watchdog/history` — return the rolling watchdog history file.
pub async fn watchdog_history(State(state): State<AppState>) -> ApiResult<Value> {
    let path = std::path::Path::new(&state.config.server.data_dir).join("watchdog_history.jsonl");
    // Through the file gate's handle and its type judgment (ADR-011), on
    // the blocking pool: a regular file is read, and a FIFO or a device at
    // the name is refused without an open. A plain read here ran on a
    // runtime worker, and four of them waiting on a FIFO stopped the agent.
    let read = tokio::task::spawn_blocking(move || {
        crate::file_access::read_own(&path, WATCHDOG_HISTORY_READ_CAP)
    })
    .await
    .map_err(|e| crate::file_access::Refusal::Io(std::io::Error::other(e)))
    .and_then(|read| read)
    .map_err(|refusal| {
        let status = match refusal {
            crate::file_access::Refusal::NotRegular(_)
            | crate::file_access::Refusal::IsDirectory => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        ApiError::new(
            refusal.code(),
            format!("read watchdog_history.jsonl: {}", refusal.message()),
        )
        .into_response_with(status)
    })?;
    let Some(bytes) = read else {
        return Ok(Json(json!([])));
    };
    let contents = String::from_utf8_lossy(&bytes);
    let entries: Vec<Value> = contents
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect();
    Ok(Json(json!(entries)))
}

fn ensure_lte_configured(state: &AppState) -> Result<(), (StatusCode, Json<ApiError>)> {
    if state.config.lte.is_some() {
        Ok(())
    } else {
        Err(
            ApiError::new(codes::NOT_FOUND, "LTE not configured on this device")
                .into_response_with(StatusCode::NOT_FOUND),
        )
    }
}

async fn ensure_capability(
    state: &AppState,
    capability: &str,
) -> Result<(), (StatusCode, Json<ApiError>)> {
    let Some(comms_state) = &state.comms_state else {
        return Err(unavailable_pair());
    };
    let guard = comms_state.lock().await;
    if guard.capabilities.is_empty() && state.comms_client.is_none() {
        return Err(unavailable_pair());
    }
    if guard.has_capability(capability) {
        Ok(())
    } else {
        Err(ApiError::new(
            "COMMS_CAPABILITY_UNSUPPORTED",
            format!("active comms provider does not support {capability}"),
        )
        .into_response_with(StatusCode::NOT_IMPLEMENTED))
    }
}

fn comms_unavailable<T>() -> Result<T, (StatusCode, Json<ApiError>)> {
    Err(unavailable_pair())
}

fn unavailable_pair() -> (StatusCode, Json<ApiError>) {
    ApiError::new(codes::MODEM_UNAVAILABLE, "comms provider not available")
        .into_response_with(StatusCode::SERVICE_UNAVAILABLE)
}

fn provider_error(err: crate::comms::CommsCallError) -> (StatusCode, Json<ApiError>) {
    let status = match err.code.as_str() {
        "COMMS_CAPABILITY_UNSUPPORTED" | "UNSUPPORTED" => StatusCode::NOT_IMPLEMENTED,
        "SCAN_RUNNING" | "TUNNEL_CONNECTED" => StatusCode::CONFLICT,
        "MODEM_UNAVAILABLE" => StatusCode::SERVICE_UNAVAILABLE,
        "INVALID_REQUEST" => StatusCode::BAD_REQUEST,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    ApiError::new(err.code, err.message).into_response_with(status)
}

fn validate_band_request(req: &SetBandsRequest) -> Result<BandSelection, ApiErrorResponse> {
    match req.mode.as_str() {
        "auto" => Ok(((1..=128).collect(), None)),
        "locked" => {
            let Some(ref bands) = req.bands else {
                return Err(ApiError::new(
                    codes::INVALID_REQUEST,
                    "bands required for locked mode",
                )
                .into_response_with(StatusCode::BAD_REQUEST));
            };
            if bands.is_empty() {
                return Err(
                    ApiError::new(codes::INVALID_REQUEST, "bands list cannot be empty")
                        .into_response_with(StatusCode::BAD_REQUEST),
                );
            }
            for &band in bands {
                if !(1..=128).contains(&band) {
                    return Err(ApiError::new(
                        codes::INVALID_REQUEST,
                        format!("invalid band number: {band}"),
                    )
                    .into_response_with(StatusCode::BAD_REQUEST));
                }
            }
            Ok((bands.clone(), req.priority_band))
        }
        _ => Err(
            ApiError::new(codes::INVALID_REQUEST, "mode must be 'locked' or 'auto'")
                .into_response_with(StatusCode::BAD_REQUEST),
        ),
    }
}

fn default_lte_bands() -> Vec<u16> {
    vec![
        1, 2, 3, 4, 5, 7, 8, 12, 13, 14, 17, 20, 25, 26, 28, 29, 30, 66, 71,
    ]
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::config::Config;
    use crate::file_access::Gate;

    /// The history as the route reads it, asked on a thread of its own: a
    /// plain read ran on the runtime's worker, where no timeout of that
    /// runtime could end it. On a timeout the FIFO gets a writer, opened
    /// without waiting, before the test fails.
    fn history(state: &AppState, fifo: Option<&std::path::Path>) -> ApiResult<Value> {
        let (tx, rx) = std::sync::mpsc::channel();
        let state = state.clone();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let _ = tx.send(runtime.block_on(watchdog_history(State(state))));
        });
        let Ok(answer) = rx.recv_timeout(Duration::from_secs(5)) else {
            if let Some(fifo) = fifo {
                let _ = std::fs::OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(fifo);
            }
            panic!("the history read waited");
        };
        answer
    }

    /// A FIFO at the history's name held a runtime worker for good, and
    /// four of them stopped the agent. It is judged by its type and never
    /// opened; the watchdog's own file is read as before.
    #[test]
    fn the_watchdog_history_refuses_a_fifo_at_once_and_reads_its_own_file() {
        let dir = std::env::temp_dir().join(format!("sctl-lte-history-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("data")).unwrap();
        let mut config: Config = toml::from_str("").unwrap();
        config.server.data_dir = dir.join("data").display().to_string();
        let gate = Arc::new(Gate::for_agent(&config, None, None));
        let state = AppState::for_tests(config, gate);
        let path = dir.join("data/watchdog_history.jsonl");

        // No file yet: an empty history.
        let Json(none) = history(&state, None).expect("no history yet");
        assert_eq!(none, json!([]));

        nix::unistd::mkfifo(&path, nix::sys::stat::Mode::S_IRWXU).unwrap();
        for _ in 0..8 {
            let (status, Json(error)) =
                history(&state, Some(&path)).expect_err("a FIFO is not the history");
            assert_eq!(
                (status, error.code.as_str()),
                (StatusCode::BAD_REQUEST, "NOT_A_REGULAR_FILE")
            );
        }

        std::fs::remove_file(&path).unwrap();
        std::fs::write(
            &path,
            "{\"action\":\"soft_reset\"}\n\nnot json\n{\"action\":\"none\"}\n",
        )
        .unwrap();
        let Json(entries) = history(&state, None).expect("the watchdog's own file");
        assert_eq!(
            entries,
            json!([{"action": "soft_reset"}, {"action": "none"}])
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
