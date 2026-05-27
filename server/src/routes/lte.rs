//! LTE-compatible projections and controls backed by the external comms provider.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use sctl_comms_protocol::{capabilities, methods};
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
    let snapshot = comms_state
        .lock()
        .await
        .lte
        .clone()
        .unwrap_or_else(crate::comms::starting_lte_response);

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
            Duration::from_secs(300),
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
            Duration::from_secs(120),
        )
        .await
        .map_err(provider_error)?;

    Ok(Json(result))
}

/// `GET /api/lte/watchdog/history` — return the rolling watchdog history file.
pub async fn watchdog_history(State(state): State<AppState>) -> ApiResult<Value> {
    let path = std::path::Path::new(&state.config.server.data_dir).join("watchdog_history.jsonl");
    if !path.exists() {
        return Ok(Json(json!([])));
    }
    let contents = std::fs::read_to_string(&path).map_err(|e| {
        ApiError::new(codes::IO_ERROR, format!("read watchdog_history.jsonl: {e}"))
            .into_response_with(StatusCode::INTERNAL_SERVER_ERROR)
    })?;
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
