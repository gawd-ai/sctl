//! LTE signal, modem info, band control, and scan endpoints.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::lte;
use crate::AppState;

/// `GET /api/lte` — returns current LTE signal quality, modem identity, band history, and scan status.
///
/// Returns cached data by default. Pass `?refresh=true` to trigger an on-demand
/// signal poll (runs AT commands — avoid while tunnel is connected over LTE).
/// Returns 404 if LTE monitoring is not configured on this device.
pub async fn lte(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(lte_state) = &state.lte_state else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "LTE not configured on this device"})),
        ));
    };

    // Only trigger on-demand poll when explicitly requested
    if params.get("refresh").is_some_and(|v| v == "true") {
        if let Some(ref notify) = state.lte_poll_notify {
            notify.notify_one();
            // Brief wait for the poller to complete one cycle
            tokio::time::sleep(std::time::Duration::from_millis(3000)).await;
        }
    }

    let ls = lte_state.lock().await;

    let signal = ls.signal.as_ref().map(|sig| {
        json!({
            "rssi_dbm": sig.rssi_dbm,
            "rsrp": sig.rsrp,
            "rsrq": sig.rsrq,
            "sinr": sig.sinr,
            "band": sig.band,
            "operator": sig.operator,
            "technology": sig.technology,
            "cell_id": sig.cell_id,
            "pci": sig.pci,
            "earfcn": sig.earfcn,
            "freq_band": sig.freq_band,
            "tac": sig.tac,
            "plmn": sig.plmn,
            "enodeb_id": sig.enodeb_id,
            "sector": sig.sector,
            "ul_bw_mhz": sig.ul_bw_mhz,
            "dl_bw_mhz": sig.dl_bw_mhz,
            "connection_state": sig.connection_state,
            "duplex": sig.duplex,
            "neighbors": sig.neighbors,
            "band_config": sig.band_config,
            "signal_bars": sig.signal_bars,
            "recorded_at": sig.recorded_at,
        })
    });

    let modem = ls.modem.as_ref().map(|m| {
        json!({
            "model": m.model,
            "firmware": m.firmware,
            "imei": m.imei,
            "iccid": m.iccid,
        })
    });

    // Collect band history sorted by band number
    let mut band_history: Vec<&lte::BandHistoryEntry> = ls.band_history.values().collect();
    band_history.sort_by_key(|e| e.band);

    Ok(Json(json!({
        "signal": signal,
        "modem": modem,
        "errors_total": ls.errors_total,
        "last_error": ls.last_error,
        "band_history": band_history,
        "scan_status": ls.scan_status,
    })))
}

/// Request body for `POST /api/lte/bands`.
#[derive(Deserialize)]
pub struct SetBandsRequest {
    pub mode: String,
    pub bands: Option<Vec<u16>>,
    pub priority_band: Option<u16>,
}

/// `POST /api/lte/bands` — switch between locked and auto band modes.
pub async fn set_bands(
    State(state): State<AppState>,
    Json(req): Json<SetBandsRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(ref lte_state) = state.lte_state else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "LTE not configured on this device"})),
        ));
    };

    let Some(ref modem) = state.modem else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "modem not available"})),
        ));
    };

    // AT commands disrupt the QMI data path — block while tunnel is active
    if state
        .tunnel_stats
        .connected
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        return Err((
            StatusCode::CONFLICT,
            Json(
                json!({"error": "band changes blocked while tunnel is connected (AT commands disrupt LTE data path)"}),
            ),
        ));
    }

    // Validate request before touching the modem
    let (new_bands, new_priority): (Vec<u16>, Option<u16>) = match req.mode.as_str() {
        "auto" => ((1..=128).collect(), None),
        "locked" => {
            let Some(ref bands) = req.bands else {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": "bands required for locked mode"})),
                ));
            };
            if bands.is_empty() {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": "bands list cannot be empty"})),
                ));
            }
            for &b in bands {
                if !(1..=128).contains(&b) {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        Json(json!({"error": format!("invalid band number: {b}")})),
                    ));
                }
            }
            (bands.clone(), req.priority_band)
        }
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "mode must be 'locked' or 'auto'"})),
            ));
        }
    };

    // Snapshot current bands before change (for watchdog pre-change revert).
    // Done after validation so we don't waste AT commands on invalid requests.
    let (current_bands, current_priority) = {
        let band_resp = modem.command("AT+QCFG=\"band\"").await.unwrap_or_default();
        let pri_resp = modem
            .command("AT+QCFG=\"bandpri\"")
            .await
            .unwrap_or_default();
        (
            lte::parse_band_config(&band_resp),
            lte::parse_bandpri(&pri_resp),
        )
    };

    match lte::safe_set_bands(
        modem,
        &new_bands,
        new_priority,
        std::time::Duration::from_secs(30),
        Some(lte_state),
    )
    .await
    {
        Ok(config) => {
            let mut ls = lte_state.lock().await;
            ls.record_band_change(
                lte::BandChangeSource::User,
                &current_bands,
                current_priority,
                &new_bands,
            );
            Ok(Json(json!({
                "status": "ok",
                "mode": req.mode,
                "band_config": config,
            })))
        }
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e})))),
    }
}

/// Request body for `POST /api/lte/scan`.
#[derive(Deserialize)]
pub struct StartScanRequest {
    pub bands: Option<Vec<u16>>,
    pub include_speed_test: Option<bool>,
}

/// `POST /api/lte/scan` — start a background band scan.
pub async fn start_scan(
    State(state): State<AppState>,
    Json(req): Json<StartScanRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(ref lte_state) = state.lte_state else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "LTE not configured on this device"})),
        ));
    };

    let Some(ref modem) = state.modem else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "modem not available"})),
        ));
    };

    // AT commands disrupt the QMI data path — block while tunnel is active
    if state
        .tunnel_stats
        .connected
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        return Err((
            StatusCode::CONFLICT,
            Json(
                json!({"error": "band scan blocked while tunnel is connected (AT commands disrupt LTE data path)"}),
            ),
        ));
    }

    // Check if scan already running
    {
        let ls = lte_state.lock().await;
        if let Some(ref scan) = ls.scan_status {
            if scan.state == "running" {
                return Err((
                    StatusCode::CONFLICT,
                    Json(json!({"error": "scan already running"})),
                ));
            }
        }
    }

    // Determine bands to scan — default to all EC25-AF supported bands
    let bands_to_scan = req.bands.unwrap_or_else(|| {
        vec![
            1, 2, 3, 4, 5, 7, 8, 12, 13, 14, 17, 20, 25, 26, 28, 29, 30, 66, 71,
        ]
    });

    let include_speed_test = req.include_speed_test.unwrap_or(false);
    let speed_test_url = state
        .config
        .lte
        .as_ref()
        .and_then(|lc| lc.speed_test_url.clone());

    let interface = state
        .config
        .lte
        .as_ref()
        .map_or_else(|| "wwan0".to_string(), |lc| lc.interface.clone());

    lte::spawn_band_scan(
        modem.clone(),
        lte_state.clone(),
        bands_to_scan.clone(),
        include_speed_test,
        speed_test_url,
        state.config.server.data_dir.clone(),
        interface,
        state.tunnel_stats.clone(),
    );

    Ok(Json(json!({
        "status": "started",
        "bands_to_scan": bands_to_scan,
    })))
}
