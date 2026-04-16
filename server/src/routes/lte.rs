//! LTE signal, modem info, band control, and scan endpoints.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::info;

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
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
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
            "imsi": m.imsi,
        })
    });

    // Collect band history sorted by band number
    let mut band_history: Vec<&lte::BandHistoryEntry> = ls.band_history.values().collect();
    band_history.sort_by_key(|e| e.band);

    // Include watchdog snapshot if available
    let watchdog = if let Some(ref wd) = state.watchdog_snapshot {
        let snap = wd.lock().await;
        Some(serde_json::to_value(&*snap).unwrap_or_default())
    } else {
        None
    };

    Ok(Json(json!({
        "signal": signal,
        "modem": modem,
        "errors_total": ls.errors_total,
        "last_error": ls.last_error,
        "band_history": band_history,
        "scan_status": ls.scan_status,
        "registration_pending": ls.registration_pending,
        "watchdog": watchdog,
    })))
}

/// Request body for `POST /api/lte/bands`.
#[derive(Deserialize)]
pub struct SetBandsRequest {
    pub mode: String,
    pub bands: Option<Vec<u16>>,
    pub priority_band: Option<u16>,
    pub force: Option<bool>,
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

    // AT commands disrupt the QMI data path — block while tunnel is active (unless forced)
    if !req.force.unwrap_or(false)
        && state
            .tunnel_stats
            .connected
            .load(std::sync::atomic::Ordering::Relaxed)
    {
        return Err((
            StatusCode::CONFLICT,
            Json(
                json!({"error": "band changes blocked while tunnel is connected (AT commands disrupt LTE data path). Use force:true to override."}),
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

    // Detect additive changes BEFORE touching the modem, using cached state.
    // Additive = only adding bands, not removing any. The modem applies these on the
    // fly — a single AT write is enough, no reads/verify/deregister needed.
    let (cached_bands, cached_priority, serving_band) = {
        let ls = lte_state.lock().await;
        let bands = ls
            .signal
            .as_ref()
            .and_then(|s| s.band_config.as_ref())
            .map(|bc| bc.enabled_bands.clone())
            .unwrap_or_default();
        let pri = ls
            .signal
            .as_ref()
            .and_then(|s| s.band_config.as_ref())
            .and_then(|bc| bc.priority_band);
        let serving = ls.signal.as_ref().and_then(|s| s.freq_band);
        (bands, pri, serving)
    };
    let old_set: std::collections::HashSet<u16> = cached_bands.iter().copied().collect();
    let new_set: std::collections::HashSet<u16> = new_bands.iter().copied().collect();
    let is_additive = !cached_bands.is_empty() && old_set.is_subset(&new_set) && old_set != new_set;

    if is_additive {
        // Additive fast path: anchor to current serving band first so the modem
        // doesn't switch cells when the band list expands, then write new bands.
        // Two AT commands with 1s spacing — same as gentle polling, safe during tunnel.
        if let Some(serving) = serving_band {
            let pri_cmd = format!("AT+QCFG=\"bandpri\",{serving}");
            let _ = modem.command(&pri_cmd).await;
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }

        let hex = lte::bands_to_hex(&new_bands);
        let cmd = format!("AT+QCFG=\"band\",260,{hex},0");
        modem.command(&cmd).await.map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("AT band write failed: {e}")})),
            )
        })?;

        // If user requested a different priority than the serving anchor, set it
        if let Some(pri) = new_priority {
            if new_priority != serving_band {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                let pri_cmd = format!("AT+QCFG=\"bandpri\",{pri}");
                let _ = modem.command(&pri_cmd).await;
            }
        }

        info!(
            "Band change: additive ({} → {} bands), anchored to serving B{}",
            cached_bands.len(),
            new_bands.len(),
            serving_band.unwrap_or(0)
        );

        // Priority is either user-requested, or the serving band we anchored to
        let effective_priority = new_priority.or(serving_band);
        let config = lte::BandConfig {
            enabled_bands: new_bands.clone(),
            priority_band: effective_priority,
        };

        // Suppress watchdog + update safe_bands so it won't revert
        {
            let mut ls = lte_state.lock().await;
            ls.last_user_action_at = Some(std::time::Instant::now());
            ls.band_action_until =
                Some(std::time::Instant::now() + std::time::Duration::from_secs(15));
            ls.record_band_change(
                lte::BandChangeSource::User,
                &cached_bands,
                cached_priority,
                &new_bands,
            );
            let rsrp = ls.signal.as_ref().and_then(|s| s.rsrp);
            ls.promote_safe_bands(
                &state.config.server.data_dir,
                &config.enabled_bands,
                config.priority_band,
                rsrp,
            );
        }

        return Ok(Json(json!({
            "status": "ok",
            "mode": req.mode,
            "band_config": config,
            "registration": "ok",
        })));
    }

    // Non-additive path: full verified write + priority set.
    // Background task monitors registration only if deregistration was needed.
    let (config, old_bands, old_priority, did_deregister) =
        lte::apply_bands_fast(modem, &new_bands, new_priority)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))))?;

    let current_bands = old_bands.clone();
    let current_priority = old_priority;

    let registration = if did_deregister {
        // Modem was deregistered — need background CEREG polling + interface restart
        {
            let mut ls = lte_state.lock().await;
            ls.last_user_action_at = Some(std::time::Instant::now());
            ls.registration_pending = true;
            ls.band_action_until =
                Some(std::time::Instant::now() + std::time::Duration::from_secs(45));
            ls.record_band_change(
                lte::BandChangeSource::User,
                &current_bands,
                current_priority,
                &new_bands,
            );
        }

        let modem_clone = modem.clone();
        let lte_state_clone = lte_state.clone();
        let expected_bands = new_bands;
        let interface = state
            .config
            .lte
            .as_ref()
            .map_or_else(|| "wwan0".to_string(), |lc| lc.interface.clone());
        let interface_restart_cmd = state
            .config
            .lte
            .as_ref()
            .and_then(|lc| lc.interface_restart_cmd.clone());
        tokio::spawn(async move {
            lte::monitor_registration(
                modem_clone,
                lte_state_clone,
                expected_bands,
                old_bands,
                old_priority,
                std::time::Duration::from_secs(30),
                interface,
                interface_restart_cmd,
            )
            .await;
        });
        "pending"
    } else {
        // Non-additive direct write (removing/switching bands without deregister).
        // Check if data path is still up; only restart interface if it broke.
        {
            let mut ls = lte_state.lock().await;
            ls.last_user_action_at = Some(std::time::Instant::now());
            ls.registration_pending = true;
            ls.band_action_until =
                Some(std::time::Instant::now() + std::time::Duration::from_secs(15));
            ls.record_band_change(
                lte::BandChangeSource::User,
                &current_bands,
                current_priority,
                &new_bands,
            );
        }

        let modem_clone = modem.clone();
        let lte_state_clone = lte_state.clone();
        let expected = new_bands;
        let interface = state
            .config
            .lte
            .as_ref()
            .map_or_else(|| "wwan0".to_string(), |lc| lc.interface.clone());
        let interface_restart_cmd = state
            .config
            .lte
            .as_ref()
            .and_then(|lc| lc.interface_restart_cmd.clone());
        let tunnel_stats_clone = state.tunnel_stats.clone();
        tokio::spawn(async move {
            // Wait for modem to settle after spaced AT write commands
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;

            // Check if data path survived the AT write commands
            let has_ip = crate::lte_watchdog::interface_has_ipv4(&interface);
            let tunnel_ok = tunnel_stats_clone
                .connected
                .load(std::sync::atomic::Ordering::Relaxed);

            if has_ip && tunnel_ok {
                info!("Band change: data path still up, skipping interface restart");
            } else {
                info!(
                    "Band change: data path disrupted (ipv4={has_ip}, tunnel={tunnel_ok}), restarting interface"
                );
                let openwrt = crate::lte_watchdog::is_openwrt();
                lte::recover_data_path_pub(
                    &modem_clone,
                    &lte_state_clone,
                    &expected,
                    &interface,
                    openwrt,
                    interface_restart_cmd.as_deref(),
                )
                .await;
            }
            let mut ls = lte_state_clone.lock().await;
            ls.band_action_until = None;
            ls.registration_pending = false;
        });
        "pending"
    };

    Ok(Json(json!({
        "status": "ok",
        "mode": req.mode,
        "band_config": config,
        "registration": registration,
    })))
}

/// Request body for `POST /api/lte/scan`.
#[derive(Deserialize)]
pub struct StartScanRequest {
    pub bands: Option<Vec<u16>>,
    pub include_speed_test: Option<bool>,
    pub force: Option<bool>,
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

    // AT commands disrupt the QMI data path — block while tunnel is active (unless forced)
    if !req.force.unwrap_or(false)
        && state
            .tunnel_stats
            .connected
            .load(std::sync::atomic::Ordering::Relaxed)
    {
        return Err((
            StatusCode::CONFLICT,
            Json(
                json!({"error": "band scan blocked while tunnel is connected (AT commands disrupt LTE data path). Use force:true to override."}),
            ),
        ));
    }

    // Check if scan already running
    {
        let mut ls = lte_state.lock().await;
        ls.last_user_action_at = Some(std::time::Instant::now());
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
    let speed_test_upload_url = state
        .config
        .lte
        .as_ref()
        .and_then(|lc| lc.speed_test_upload_url.clone());

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
        speed_test_upload_url,
        state.config.server.data_dir.clone(),
        interface,
        state.tunnel_stats.clone(),
        req.force.unwrap_or(false),
    );

    Ok(Json(json!({
        "status": "started",
        "bands_to_scan": bands_to_scan,
    })))
}

/// `POST /api/lte/speedtest` — run a quick download+upload speed test on the current band.
///
/// No AT commands needed — just measures throughput through the LTE interface.
/// Safe to use while tunnel is connected.
pub async fn speed_test(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let lte_config = state.config.lte.as_ref().ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "LTE not configured on this device"})),
        )
    })?;

    let interface = lte_config.interface.clone();
    let dl_url = lte_config.speed_test_url.clone();
    let ul_url = lte_config.speed_test_upload_url.clone();

    if dl_url.is_none() && ul_url.is_none() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(
                json!({"error": "no speed_test_url or speed_test_upload_url configured in [lte]"}),
            ),
        ));
    }

    let download_bps = if let Some(ref url) = dl_url {
        lte::run_download_speed_test(url, &interface).await
    } else {
        None
    };

    let upload_bps = if let Some(ref url) = ul_url {
        lte::run_upload_speed_test(url, &interface).await
    } else {
        None
    };

    Ok(Json(json!({
        "download_bps": download_bps,
        "upload_bps": upload_bps,
    })))
}
