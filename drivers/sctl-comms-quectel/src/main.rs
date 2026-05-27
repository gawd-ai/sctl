#![deny(clippy::all)]
#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use sctl::config::{GpsConfig, LteConfig};
use sctl::gps::GpsState;
use sctl::lte::LteState;
use sctl::lte_watchdog::WatchdogSnapshot;
use sctl::modem::Modem;
use sctl::state::TunnelStats;
use sctl_comms_protocol::{capabilities, methods, CommsRequest, CommsResponse};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, watch, Mutex, RwLock};
use tracing::{info, warn};

#[derive(Default)]
struct Runtime {
    modem: Option<Modem>,
    modem_tx: Option<watch::Sender<Modem>>,
    detected_path: Option<String>,
    gps_state: Option<Arc<Mutex<GpsState>>>,
    lte_state: Option<Arc<Mutex<LteState>>>,
    watchdog_snapshot: Option<Arc<Mutex<WatchdogSnapshot>>>,
    tunnel_stats: Arc<TunnelStats>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    data_dir: String,
    lte_config: Option<LteConfig>,
    lte_poll_notify: Option<Arc<tokio::sync::Notify>>,
    modem_detected_path: Arc<RwLock<Option<String>>>,
}

#[derive(Deserialize)]
struct OpenParams {
    device: Option<String>,
    #[serde(default = "default_data_dir")]
    data_dir: String,
    gps: Option<GpsConfig>,
    lte: Option<LteConfig>,
    tunnel_url: Option<String>,
}

#[derive(Deserialize)]
struct LinkPollParams {
    #[serde(default)]
    refresh: bool,
    #[serde(default)]
    tunnel_connected: bool,
}

#[derive(Default, Deserialize)]
struct StatusParams {
    tunnel_connected: Option<bool>,
}

#[derive(Deserialize)]
struct SetBandsParams {
    mode: String,
    bands: Option<Vec<u16>>,
    priority_band: Option<u16>,
    #[serde(default)]
    force: bool,
    #[serde(default)]
    tunnel_connected: bool,
}

#[derive(Deserialize)]
struct ScanParams {
    bands: Vec<u16>,
    #[serde(default)]
    include_speed_test: bool,
    #[serde(default)]
    force: bool,
    #[serde(default)]
    tunnel_connected: bool,
}

#[derive(Deserialize)]
struct SpeedTestParams {
    interface: String,
    download_url: Option<String>,
    upload_url: Option<String>,
}

fn default_data_dir() -> String {
    "/var/lib/sctl".to_string()
}

#[tokio::main]
async fn main() {
    let log_filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());
    tracing_subscriber::fmt()
        .with_env_filter(log_filter)
        .with_writer(std::io::stderr)
        .init();

    let runtime = Arc::new(Mutex::new(Runtime {
        tunnel_stats: Arc::new(TunnelStats::new()),
        data_dir: default_data_dir(),
        modem_detected_path: Arc::new(RwLock::new(None)),
        ..Runtime::default()
    }));

    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut lines = BufReader::new(stdin).lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let response = match serde_json::from_str::<CommsRequest>(&line) {
            Ok(req) => handle_request(req, runtime.clone()).await,
            Err(err) => CommsResponse::err("0", "INVALID_REQUEST", err.to_string()),
        };
        match serde_json::to_string(&response) {
            Ok(raw) => {
                let _ = stdout.write_all(raw.as_bytes()).await;
                let _ = stdout.write_all(b"\n").await;
                let _ = stdout.flush().await;
            }
            Err(err) => {
                let fallback = CommsResponse::err(response.id, "ENCODE_FAILED", err.to_string());
                if let Ok(raw) = serde_json::to_string(&fallback) {
                    let _ = stdout.write_all(raw.as_bytes()).await;
                    let _ = stdout.write_all(b"\n").await;
                    let _ = stdout.flush().await;
                }
            }
        }
    }
}

async fn handle_request(req: CommsRequest, runtime: Arc<Mutex<Runtime>>) -> CommsResponse {
    let result = match req.method.as_str() {
        methods::HELLO | methods::CAPABILITIES => Ok(hello()),
        methods::DETECT => Ok(detect()),
        methods::OPEN => open(req.params, runtime).await,
        methods::STATUS => status(req.params, runtime).await,
        methods::LOCATION_POLL => location_poll(runtime).await,
        methods::LOCATION_DISABLE => location_disable(runtime).await,
        methods::LINK_POLL => link_poll(req.params, runtime).await,
        methods::LINK_SPEED_TEST => speed_test(req.params).await,
        methods::CELLULAR_SET_BANDS => set_bands(req.params, runtime).await,
        methods::CELLULAR_SCAN => scan(req.params, runtime).await,
        methods::RECOVERY_USB_CYCLE => usb_cycle(runtime).await,
        _ => Err((
            "COMMS_CAPABILITY_UNSUPPORTED",
            format!("unsupported method {}", req.method),
        )),
    };

    match result {
        Ok(value) => CommsResponse::ok(req.id, value),
        Err((code, message)) => CommsResponse::err(req.id, code, message),
    }
}

fn hello() -> Value {
    json!({
        "protocol_version": sctl_comms_protocol::PROTOCOL_VERSION,
        "provider": "quectel-at",
        "status": "ok",
        "capabilities": provider_capabilities(),
    })
}

fn provider_capabilities() -> Vec<&'static str> {
    vec![
        capabilities::LOCATION_GNSS,
        capabilities::LINK_CELLULAR,
        capabilities::CELLULAR_BAND_CONTROL,
        capabilities::CELLULAR_SCAN,
        capabilities::RECOVERY_USB_CYCLE,
        capabilities::RECOVERY_TUNNEL_WATCHDOG,
    ]
}

fn detect() -> Value {
    json!({
        "provider": "quectel-at",
        "detected_path": sctl::modem::detect_quectel_at_port_strict(),
        "capabilities": provider_capabilities(),
    })
}

async fn open(
    params: Value,
    runtime: Arc<Mutex<Runtime>>,
) -> Result<Value, (&'static str, String)> {
    let params: OpenParams =
        serde_json::from_value(params).map_err(|e| ("INVALID_REQUEST", e.to_string()))?;

    let resolved = sctl::modem::detect_quectel_at_port_strict().or_else(|| params.device.clone());
    let Some(path) = resolved else {
        return Err((
            "MODEM_UNAVAILABLE",
            "no Quectel AT port detected and no device hint configured".to_string(),
        ));
    };

    let mut modem = None;
    for attempt in 0..5 {
        match Modem::open(&path) {
            Ok(m) => {
                if attempt > 0 {
                    info!("modem {path}: opened on attempt {}", attempt + 1);
                }
                modem = Some(m);
                break;
            }
            Err(err) if attempt < 4 => {
                info!("modem {path}: not ready ({err}), retrying in 2s");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Err(err) => {
                return Err((
                    "MODEM_UNAVAILABLE",
                    format!("failed to open modem {path}: {err}"),
                ));
            }
        }
    }
    let modem = modem.expect("opened or returned");

    let mut guard = runtime.lock().await;
    for task in guard.tasks.drain(..) {
        task.abort();
    }

    let (tx, _rx) = watch::channel(modem.clone());
    guard.modem = Some(modem.clone());
    guard.modem_tx = Some(tx.clone());
    guard.detected_path = Some(path.clone());
    guard.data_dir.clone_from(&params.data_dir);
    guard.lte_config.clone_from(&params.lte);
    guard.tunnel_stats = Arc::new(TunnelStats::new());
    guard.lte_poll_notify = None;
    *guard.modem_detected_path.write().await = Some(path.clone());

    let (events, _) = broadcast::channel(256);

    if let Some(ref gps_cfg) = params.gps {
        let gps_state = Arc::new(Mutex::new(GpsState::new(gps_cfg.history_size)));
        let lte_iface = params.lte.as_ref().map(|lc| lc.interface.clone());
        let task = sctl::gps::spawn_gps_poller(
            gps_cfg.clone(),
            modem.clone(),
            gps_state.clone(),
            events.clone(),
            tx.subscribe(),
            lte_iface,
        );
        guard.gps_state = Some(gps_state);
        guard.tasks.push(task);
    } else {
        guard.gps_state = None;
    }

    if let Some(ref lte_cfg) = params.lte {
        let mut state = LteState::new();
        state.load_safe_bands(&params.data_dir).await;
        state.load_lte_data(&params.data_dir).await;
        let lte_state = Arc::new(Mutex::new(state));
        let notify = Arc::new(tokio::sync::Notify::new());
        let task = sctl::lte::spawn_lte_poller(
            lte_cfg.clone(),
            modem.clone(),
            lte_state.clone(),
            events.clone(),
            tx.subscribe(),
            params.data_dir.clone(),
            guard.tunnel_stats.clone(),
            notify.clone(),
        );
        guard.lte_state = Some(lte_state.clone());
        guard.lte_poll_notify = Some(notify);
        guard.tasks.push(task);

        if lte_cfg.watchdog && params.tunnel_url.is_some() {
            let snapshot = Arc::new(Mutex::new(WatchdogSnapshot::new()));
            let task = sctl::lte_watchdog::spawn_lte_watchdog(
                modem,
                tx,
                lte_state,
                guard.tunnel_stats.clone(),
                events,
                lte_cfg.clone(),
                params.data_dir,
                params.tunnel_url,
                snapshot.clone(),
                guard.modem_detected_path.clone(),
            );
            guard.watchdog_snapshot = Some(snapshot);
            guard.tasks.push(task);
        } else {
            guard.watchdog_snapshot = None;
        }
    } else {
        guard.lte_state = None;
        guard.watchdog_snapshot = None;
    }

    Ok(status_value(&guard))
}

async fn status(
    params: Value,
    runtime: Arc<Mutex<Runtime>>,
) -> Result<Value, (&'static str, String)> {
    let params: StatusParams =
        serde_json::from_value(params).map_err(|e| ("INVALID_REQUEST", e.to_string()))?;
    let guard = runtime.lock().await;
    if let Some(connected) = params.tunnel_connected {
        guard
            .tunnel_stats
            .connected
            .store(connected, Ordering::Relaxed);
    }
    Ok(status_value(&guard))
}

fn status_value(runtime: &Runtime) -> Value {
    json!({
        "provider": "quectel-at",
        "status": if runtime.modem.is_some() { "ok" } else { "not_open" },
        "detected_path": runtime.detected_path.clone(),
        "capabilities": provider_capabilities(),
    })
}

async fn location_poll(runtime: Arc<Mutex<Runtime>>) -> Result<Value, (&'static str, String)> {
    let gps_state = {
        let guard = runtime.lock().await;
        guard.gps_state.clone()
    };
    let Some(gps_state) = gps_state else {
        return Err((
            "COMMS_CAPABILITY_UNSUPPORTED",
            "location.gnss is not configured".to_string(),
        ));
    };
    let guard = gps_state.lock().await;
    Ok(snapshot_gps(&guard))
}

async fn location_disable(runtime: Arc<Mutex<Runtime>>) -> Result<Value, (&'static str, String)> {
    let modem = {
        let guard = runtime.lock().await;
        guard.modem.clone()
    };
    if let Some(modem) = modem {
        sctl::gps::disable_gnss(&modem).await;
    }
    Ok(json!({"status": "ok"}))
}

async fn link_poll(
    params: Value,
    runtime: Arc<Mutex<Runtime>>,
) -> Result<Value, (&'static str, String)> {
    let params: LinkPollParams =
        serde_json::from_value(params).map_err(|e| ("INVALID_REQUEST", e.to_string()))?;
    let (lte_state, watchdog_snapshot, notify, tunnel_stats) = {
        let guard = runtime.lock().await;
        (
            guard.lte_state.clone(),
            guard.watchdog_snapshot.clone(),
            guard.lte_poll_notify.clone(),
            guard.tunnel_stats.clone(),
        )
    };

    tunnel_stats
        .connected
        .store(params.tunnel_connected, Ordering::Relaxed);

    let Some(lte_state) = lte_state else {
        return Err((
            "COMMS_CAPABILITY_UNSUPPORTED",
            "link.cellular is not configured".to_string(),
        ));
    };

    if params.refresh {
        if let Some(notify) = notify {
            notify.notify_one();
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }

    Ok(snapshot_lte(&lte_state, watchdog_snapshot.as_ref()).await)
}

async fn speed_test(params: Value) -> Result<Value, (&'static str, String)> {
    let params: SpeedTestParams =
        serde_json::from_value(params).map_err(|e| ("INVALID_REQUEST", e.to_string()))?;
    let download_bps = if let Some(ref url) = params.download_url {
        sctl::lte::run_download_speed_test(url, &params.interface).await
    } else {
        None
    };
    let upload_bps = if let Some(ref url) = params.upload_url {
        sctl::lte::run_upload_speed_test(url, &params.interface).await
    } else {
        None
    };
    Ok(json!({
        "download_bps": download_bps,
        "upload_bps": upload_bps,
    }))
}

async fn set_bands(
    params: Value,
    runtime: Arc<Mutex<Runtime>>,
) -> Result<Value, (&'static str, String)> {
    let params: SetBandsParams =
        serde_json::from_value(params).map_err(|e| ("INVALID_REQUEST", e.to_string()))?;
    if params.tunnel_connected && !params.force {
        return Err((
            "TUNNEL_CONNECTED",
            "band changes blocked while tunnel is connected".to_string(),
        ));
    }

    let new_bands: Vec<u16> = match params.mode.as_str() {
        "auto" => (1..=128).collect(),
        "locked" => params.bands.clone().ok_or((
            "INVALID_REQUEST",
            "bands required for locked mode".to_string(),
        ))?,
        _ => {
            return Err((
                "INVALID_REQUEST",
                "mode must be 'locked' or 'auto'".to_string(),
            ));
        }
    };

    let (modem, lte_state, watchdog_snapshot, data_dir, lte_config, tunnel_stats) = {
        let guard = runtime.lock().await;
        (
            guard.modem.clone(),
            guard.lte_state.clone(),
            guard.watchdog_snapshot.clone(),
            guard.data_dir.clone(),
            guard.lte_config.clone(),
            guard.tunnel_stats.clone(),
        )
    };
    let Some(modem) = modem else {
        return Err(("MODEM_UNAVAILABLE", "modem not open".to_string()));
    };
    let Some(lte_state) = lte_state else {
        return Err((
            "COMMS_CAPABILITY_UNSUPPORTED",
            "cellular band control is not configured".to_string(),
        ));
    };
    let Some(lte_config) = lte_config else {
        return Err(("INVALID_REQUEST", "LTE config missing".to_string()));
    };

    let (cached_bands, cached_priority, serving_band) = {
        let state = lte_state.lock().await;
        let bands = state
            .signal
            .as_ref()
            .and_then(|signal| signal.band_config.as_ref())
            .map(|config| config.enabled_bands.clone())
            .unwrap_or_default();
        let priority = state
            .signal
            .as_ref()
            .and_then(|signal| signal.band_config.as_ref())
            .and_then(|config| config.priority_band);
        let serving = state.signal.as_ref().and_then(|signal| signal.freq_band);
        (bands, priority, serving)
    };

    let old_set: std::collections::HashSet<u16> = cached_bands.iter().copied().collect();
    let new_set: std::collections::HashSet<u16> = new_bands.iter().copied().collect();
    let is_additive = !cached_bands.is_empty() && old_set.is_subset(&new_set) && old_set != new_set;

    if is_additive {
        if let Some(serving) = serving_band {
            let _ = modem
                .command(&format!("AT+QCFG=\"bandpri\",{serving}"))
                .await;
            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        let hex = sctl::lte::bands_to_hex(&new_bands);
        modem
            .command(&format!("AT+QCFG=\"band\",260,{hex},0"))
            .await
            .map_err(|e| ("MODEM_AT_FAILED", format!("AT band write failed: {e}")))?;

        if let Some(priority) = params.priority_band {
            if params.priority_band != serving_band {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let _ = modem
                    .command(&format!("AT+QCFG=\"bandpri\",{priority}"))
                    .await;
            }
        }

        let effective_priority = params.priority_band.or(serving_band);
        let band_config = sctl::lte::BandConfig {
            enabled_bands: new_bands.clone(),
            priority_band: effective_priority,
        };

        {
            let mut state = lte_state.lock().await;
            state.last_user_action_at = Some(std::time::Instant::now());
            state.band_action_until = Some(std::time::Instant::now() + Duration::from_secs(15));
            state.record_band_change(
                sctl::lte::BandChangeSource::User,
                &cached_bands,
                cached_priority,
                &new_bands,
            );
            let rsrp = state.signal.as_ref().and_then(|signal| signal.rsrp);
            state
                .promote_safe_bands(
                    &data_dir,
                    &band_config.enabled_bands,
                    band_config.priority_band,
                    rsrp,
                )
                .await;
            if let Some(ref mut signal) = state.signal {
                signal.band_config = Some(band_config.clone());
            }
        }

        let snapshot = snapshot_lte(&lte_state, watchdog_snapshot.as_ref()).await;
        return Ok(json!({
            "status": "ok",
            "mode": params.mode,
            "band_config": band_config,
            "registration": "ok",
            "snapshot": snapshot,
        }));
    }

    let (band_config, old_bands, old_priority, did_deregister) =
        sctl::lte::apply_bands_fast(&modem, &new_bands, params.priority_band)
            .await
            .map_err(|e| ("MODEM_AT_FAILED", e))?;

    let current_bands = old_bands.clone();
    let current_priority = old_priority;

    let registration = if did_deregister {
        {
            let mut state = lte_state.lock().await;
            state.last_user_action_at = Some(std::time::Instant::now());
            state.registration_pending = true;
            state.band_action_until = Some(std::time::Instant::now() + Duration::from_secs(45));
            state.record_band_change(
                sctl::lte::BandChangeSource::User,
                &current_bands,
                current_priority,
                &new_bands,
            );
            if let Some(ref mut signal) = state.signal {
                signal.band_config = Some(band_config.clone());
            }
        }

        let modem_clone = modem.clone();
        let lte_state_clone = lte_state.clone();
        let expected_bands = new_bands.clone();
        let interface = lte_config.interface.clone();
        let interface_restart_cmd = lte_config.interface_restart_cmd.clone();
        tokio::spawn(async move {
            sctl::lte::monitor_registration(
                modem_clone,
                lte_state_clone,
                expected_bands,
                old_bands,
                old_priority,
                Duration::from_secs(30),
                interface,
                interface_restart_cmd,
            )
            .await;
        });
        "pending"
    } else {
        {
            let mut state = lte_state.lock().await;
            state.last_user_action_at = Some(std::time::Instant::now());
            state.registration_pending = true;
            state.band_action_until = Some(std::time::Instant::now() + Duration::from_secs(15));
            state.record_band_change(
                sctl::lte::BandChangeSource::User,
                &current_bands,
                current_priority,
                &new_bands,
            );
            if let Some(ref mut signal) = state.signal {
                signal.band_config = Some(band_config.clone());
            }
        }

        let modem_clone = modem.clone();
        let lte_state_clone = lte_state.clone();
        let expected = new_bands.clone();
        let interface = lte_config.interface.clone();
        let interface_restart_cmd = lte_config.interface_restart_cmd.clone();
        let tunnel_stats_clone = tunnel_stats.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let has_ip = sctl::lte_watchdog::interface_has_ipv4(&interface);
            let tunnel_ok = tunnel_stats_clone.connected.load(Ordering::Relaxed);

            if !(has_ip && tunnel_ok) {
                let openwrt = sctl::lte_watchdog::is_openwrt();
                sctl::lte::recover_data_path_pub(
                    &modem_clone,
                    &lte_state_clone,
                    &expected,
                    &interface,
                    openwrt,
                    interface_restart_cmd.as_deref(),
                )
                .await;
            }

            let mut state = lte_state_clone.lock().await;
            state.band_action_until = None;
            state.registration_pending = false;
        });
        "pending"
    };

    let snapshot = snapshot_lte(&lte_state, watchdog_snapshot.as_ref()).await;
    Ok(json!({
        "status": "ok",
        "mode": params.mode,
        "band_config": band_config,
        "registration": registration,
        "snapshot": snapshot,
    }))
}

async fn scan(
    params: Value,
    runtime: Arc<Mutex<Runtime>>,
) -> Result<Value, (&'static str, String)> {
    let params: ScanParams =
        serde_json::from_value(params).map_err(|e| ("INVALID_REQUEST", e.to_string()))?;
    if params.tunnel_connected && !params.force {
        return Err((
            "TUNNEL_CONNECTED",
            "band scan blocked while tunnel is connected".to_string(),
        ));
    }

    let (modem, lte_state, lte_config, data_dir, tunnel_stats) = {
        let guard = runtime.lock().await;
        (
            guard.modem.clone(),
            guard.lte_state.clone(),
            guard.lte_config.clone(),
            guard.data_dir.clone(),
            guard.tunnel_stats.clone(),
        )
    };
    let Some(modem) = modem else {
        return Err(("MODEM_UNAVAILABLE", "modem not open".to_string()));
    };
    let Some(lte_state) = lte_state else {
        return Err((
            "COMMS_CAPABILITY_UNSUPPORTED",
            "cellular scan is not configured".to_string(),
        ));
    };
    let Some(lte_config) = lte_config else {
        return Err(("INVALID_REQUEST", "LTE config missing".to_string()));
    };

    {
        let mut state = lte_state.lock().await;
        state.last_user_action_at = Some(std::time::Instant::now());
        if state
            .scan_status
            .as_ref()
            .is_some_and(|scan| scan.state == "running")
        {
            return Err(("SCAN_RUNNING", "scan already running".to_string()));
        }
    }

    sctl::lte::spawn_band_scan(
        modem,
        lte_state,
        params.bands.clone(),
        params.include_speed_test,
        lte_config.speed_test_url,
        lte_config.speed_test_upload_url,
        data_dir,
        lte_config.interface,
        tunnel_stats,
        params.force,
    );

    Ok(json!({
        "status": "started",
        "bands_to_scan": params.bands,
    }))
}

async fn usb_cycle(runtime: Arc<Mutex<Runtime>>) -> Result<Value, (&'static str, String)> {
    let (device_path, tx) = {
        let guard = runtime.lock().await;
        (
            guard
                .detected_path
                .clone()
                .unwrap_or_else(|| "/dev/ttyUSB2".to_string()),
            guard.modem_tx.clone(),
        )
    };

    let (action, new_modem) = sctl::lte_watchdog::action_usb_power_cycle(&device_path).await;
    let detected_path = new_modem.as_ref().map(|m| m.device().to_string());
    if let Some(new_modem) = new_modem {
        let mut guard = runtime.lock().await;
        guard.modem = Some(new_modem.clone());
        guard.detected_path.clone_from(&detected_path);
        *guard.modem_detected_path.write().await = detected_path.clone();
        if let Some(tx) = tx {
            let _ = tx.send(new_modem);
        }
    }
    Ok(json!({
        "action": action,
        "detected_path": detected_path,
    }))
}

fn snapshot_gps(gs: &GpsState) -> Value {
    let last_fix = gs.last_fix.as_ref().map(|fix| {
        json!({
            "latitude": fix.latitude,
            "longitude": fix.longitude,
            "altitude": fix.altitude,
            "speed_kmh": fix.speed_kmh,
            "course": fix.course,
            "hdop": fix.hdop,
            "satellites": fix.satellites,
            "utc": fix.utc.clone(),
            "date": fix.date.clone(),
            "fix_type": fix.fix_type,
            "recorded_at": fix.recorded_at,
        })
    });
    let history: Vec<Value> = gs
        .history
        .iter()
        .rev()
        .take(50)
        .map(|fix| {
            json!({
                "latitude": fix.latitude,
                "longitude": fix.longitude,
                "altitude": fix.altitude,
                "speed_kmh": fix.speed_kmh,
                "satellites": fix.satellites,
                "recorded_at": fix.recorded_at,
            })
        })
        .collect();
    json!({
        "status": gs.status,
        "last_fix": last_fix,
        "fix_age_secs": gs.last_fix_at.map(|t| t.elapsed().as_secs()),
        "history": history,
        "fixes_total": gs.fixes_total,
        "errors_total": gs.errors_total,
        "last_error": gs.last_error.clone(),
    })
}

async fn snapshot_lte(
    lte_state: &Arc<Mutex<LteState>>,
    watchdog_snapshot: Option<&Arc<Mutex<WatchdogSnapshot>>>,
) -> Value {
    let ls = lte_state.lock().await;
    let signal = ls.signal.as_ref().map(|sig| {
        json!({
            "rssi_dbm": sig.rssi_dbm,
            "rsrp": sig.rsrp,
            "rsrq": sig.rsrq,
            "sinr": sig.sinr,
            "band": sig.band.clone(),
            "operator": sig.operator.clone(),
            "technology": sig.technology.clone(),
            "cell_id": sig.cell_id.clone(),
            "pci": sig.pci,
            "earfcn": sig.earfcn,
            "freq_band": sig.freq_band,
            "tac": sig.tac.clone(),
            "plmn": sig.plmn.clone(),
            "enodeb_id": sig.enodeb_id,
            "sector": sig.sector,
            "ul_bw_mhz": sig.ul_bw_mhz.clone(),
            "dl_bw_mhz": sig.dl_bw_mhz.clone(),
            "connection_state": sig.connection_state.clone(),
            "duplex": sig.duplex.clone(),
            "neighbors": sig.neighbors.clone(),
            "band_config": sig.band_config.clone(),
            "signal_bars": sig.signal_bars,
            "recorded_at": sig.recorded_at,
        })
    });
    let modem = ls.modem.as_ref().map(|m| {
        json!({
            "model": m.model.clone(),
            "firmware": m.firmware.clone(),
            "imei": m.imei.clone(),
            "iccid": m.iccid.clone(),
            "imsi": m.imsi.clone(),
        })
    });
    let mut band_history: Vec<_> = ls.band_history.values().collect();
    band_history.sort_by_key(|entry| entry.band);
    let band_history = serde_json::to_value(band_history).unwrap_or_else(|_| json!([]));
    let scan_status = serde_json::to_value(&ls.scan_status).unwrap_or(Value::Null);
    let registration_pending = ls.registration_pending;
    let errors_total = ls.errors_total;
    let last_error = ls.last_error.clone();
    drop(ls);

    let watchdog = if let Some(snapshot) = watchdog_snapshot {
        let snapshot = snapshot.lock().await;
        Some(serde_json::to_value(&*snapshot).unwrap_or_default())
    } else {
        None
    };

    json!({
        "signal": signal,
        "modem": modem,
        "errors_total": errors_total,
        "last_error": last_error,
        "band_history": band_history,
        "scan_status": scan_status,
        "registration_pending": registration_pending,
        "watchdog": watchdog,
    })
}
