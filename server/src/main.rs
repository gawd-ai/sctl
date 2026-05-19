#![deny(clippy::all)]
#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

//! # sctl
//!
//! Remote shell control service for Linux devices.
//!
//! sctl exposes HTTP and WebSocket APIs on port 1337 that allow an AI agent
//! (or any authenticated client) to execute commands, manage interactive shell
//! sessions, read/write files, and query device status — all protected by a
//! pre-shared API key.
//!
//! ## Subcommands
//!
//! - `sctl serve` (default) — run the HTTP/WS server
//! - `sctl supervise` — run as supervisor: starts server and restarts on crash

mod sctlin_proxy;
mod supervisor;

use std::path::Path;
use std::sync::atomic::AtomicU32;
use std::sync::Arc;
use std::time::Instant;

use sctl::gawdxfer::manager::TransferManager;
use sctl::gawdxfer::types::TransferConfig;

use axum::{
    middleware,
    routing::{delete, get, post},
    Extension, Router,
};
use clap::{Parser, Subcommand};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, watch};
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing::{info, warn};

use sctl::{
    activity::ActivityLog,
    auth::ApiKey,
    config::Config,
    gps, infra, lte, routes, sessions,
    sessions::SessionManager,
    state::{AppState, TunnelStats},
    tunnel, ws, ExecResultsCache,
};

use sctl::VERSION;

/// Remote shell control service for Linux devices.
#[derive(Parser)]
#[command(name = "sctl", version = VERSION)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the HTTP/WS server (default when no subcommand given).
    Serve {
        /// Path to TOML config file.
        #[arg(long)]
        config: Option<String>,
        /// Skip the process singleton lock (used internally by supervisor).
        #[arg(long, hide = true)]
        skip_lock: bool,
    },
    /// Run as supervisor: starts server and restarts on crash.
    Supervise {
        /// Path to TOML config file.
        #[arg(long)]
        config: Option<String>,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Supervise { config }) => {
            run_supervisor_mode(config.as_deref()).await;
        }
        Some(Commands::Serve { config, skip_lock }) => {
            run_server(config.as_deref(), skip_lock).await;
        }
        None => {
            // Backward compat: no subcommand but --config may be passed
            let args: Vec<String> = std::env::args().collect();
            let config_path = args
                .windows(2)
                .find(|w| w[0] == "--config")
                .map(|w| w[1].clone());
            run_server(config_path.as_deref(), false).await;
        }
    }
}

/// Install a panic hook that persists the panic trace to disk for post-mortem.
///
/// Writes `<data_dir>/last_panic.log` with the panic message, thread name, and
/// backtrace. Keeps the default tracing output (so logread still shows it).
fn install_panic_hook(data_dir: &str) {
    use std::backtrace::Backtrace;
    use std::time::{SystemTime, UNIX_EPOCH};

    let log_path = std::path::Path::new(data_dir).join("last_panic.log");
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let thread = std::thread::current();
        let thread_name = thread.name().unwrap_or("<unnamed>");
        let bt = Backtrace::force_capture();
        let payload =
            format!("panic at unix={ts}\nthread={thread_name}\n{info}\nbacktrace:\n{bt}\n");
        // Best-effort — never panic inside the panic hook.
        let _ = std::fs::write(&log_path, &payload);
        prev(info);
    }));
}

/// Persist the currently-detected modem path for observability/post-mortem.
///
/// This file is NOT read on next boot — startup always re-detects via sysfs.
/// It exists so an operator inspecting a wedged box can see what sctl was
/// last running against.
fn persist_modem_state(data_dir: &str, detected_path: &str) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let payload = serde_json::json!({
        "detected_path": detected_path,
        "at_unix": ts,
    });
    let path = std::path::Path::new(data_dir).join("modem_state.json");
    let tmp = path.with_extension("json.tmp");
    if let Err(e) = std::fs::write(&tmp, payload.to_string()) {
        warn!("modem_state.json: write tmp failed: {e}");
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        warn!("modem_state.json: rename failed: {e}");
    }
}

/// Acquire exclusive process lock. Returns the held file (must not be dropped).
/// Retries 3 times with 1s delay to handle the race between old process dying
/// and new process starting (e.g. during upgrades). Exits with code 99 if still locked.
#[cfg(unix)]
fn acquire_process_lock(data_dir: &str) -> std::fs::File {
    use std::os::unix::io::AsRawFd;

    let lock_path = format!("{data_dir}/sctl.lock");
    let f = std::fs::File::create(&lock_path).unwrap_or_else(|e| {
        eprintln!("Failed to create lock file {lock_path}: {e}");
        std::process::exit(1);
    });
    for attempt in 0..3 {
        let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return f;
        }
        if attempt < 2 {
            eprintln!(
                "Lock held by another instance, retrying in 1s ({}/3)",
                attempt + 1
            );
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
    eprintln!("Another sctl instance is already running (lock: {lock_path})");
    std::process::exit(99);
}

async fn run_supervisor_mode(config_path: Option<&str>) -> ! {
    let config = Config::load(config_path);

    // Initialize tracing for supervisor
    let log_filter = std::env::var("RUST_LOG").unwrap_or_else(|_| config.logging.level.clone());
    tracing_subscriber::fmt().with_env_filter(log_filter).init();

    // Acquire lock at supervisor level — prevents two supervisors from running
    #[cfg(unix)]
    let _lock = acquire_process_lock(&config.server.data_dir);

    info!("sctl supervisor starting");
    supervisor::run_supervisor(config_path, &config.supervisor).await
}

#[allow(clippy::too_many_lines)]
async fn run_server(config_path: Option<&str>, skip_lock: bool) {
    let config = Config::load(config_path);

    // Initialize tracing
    let log_filter = std::env::var("RUST_LOG").unwrap_or_else(|_| config.logging.level.clone());
    tracing_subscriber::fmt().with_env_filter(log_filter).init();

    // Install panic hook early so panics in any spawned subsystem leave a
    // persisted trace on disk for post-mortem. Without this, the supervisor
    // restarts blindly and the underlying cause is lost.
    install_panic_hook(&config.server.data_dir);

    // Honor safe-mode flag if the supervisor wrote one (crash-loop). When set
    // we skip every optional subsystem (modem, GPS, LTE, watchdog, infra) and
    // keep only the management plane (HTTP + tunnel + sessions) live so an
    // operator can reach the box, inspect logs, and clear the flag.
    let safe_mode_flag_path = std::path::Path::new(&config.server.data_dir).join("safe_mode.flag");
    let safe_mode_active = safe_mode_flag_path.exists();
    if safe_mode_active {
        warn!(
            "==== SAFE MODE ACTIVE ==== modem/GPS/LTE/watchdog/infra subsystems will be skipped. \
             Flag: {} — clear via DELETE /api/safe_mode/flag (auth required).",
            safe_mode_flag_path.display()
        );
    }

    // Validate config before proceeding
    let validation_errors = config.validate();
    if !validation_errors.is_empty() {
        for err in &validation_errors {
            tracing::error!("Config error: {err}");
        }
        std::process::exit(1);
    }

    // Acquire exclusive lock — prevents dual instances (e.g. upgrade race, cron watchdog).
    // Skipped when launched by supervisor (which holds its own lock).
    #[cfg(unix)]
    let _lock = if skip_lock {
        None
    } else {
        Some(acquire_process_lock(&config.server.data_dir))
    };

    info!("sctl v{} starting", VERSION);
    info!("Device serial: {}", config.device.serial);
    info!("Listening on {}", config.server.listen);

    if let Some(tc) = &config.tunnel {
        if !tc.relay && tc.heartbeat_interval_secs > 15 {
            warn!(
                configured_secs = tc.heartbeat_interval_secs,
                effective_secs = config.effective_client_heartbeat_interval_secs(),
                "Tunnel client heartbeat interval too high for LTE/CGNAT; clamping to safe keepalive interval"
            );
        }
    }

    if config.auth.api_key == "change-me" {
        warn!("Using default API key — set SCTL_API_KEY or update config");
    }

    let journal_enabled = config.server.journal_enabled;
    let data_dir = config.server.data_dir.clone();
    let journal_max_age_hours = config.server.journal_max_age_hours;

    let session_manager = if journal_enabled {
        info!("Journaling enabled, data_dir: {data_dir}");
        SessionManager::with_journal(
            config.server.max_sessions,
            config.server.session_buffer_size,
            &data_dir,
        )
    } else {
        SessionManager::new(
            config.server.max_sessions,
            config.server.session_buffer_size,
        )
    };

    // Recover archived sessions from journal and clean up orphans
    if journal_enabled {
        // Kill any shell processes orphaned by a previous crash
        sessions::journal::kill_orphaned_processes(Path::new(&data_dir)).await;
        // Reload output history from journals
        session_manager
            .recover_from_journal(Path::new(&data_dir))
            .await;
        // Delete stale journal files
        sessions::journal::cleanup_old_journals(Path::new(&data_dir), journal_max_age_hours).await;
    }

    let (session_events, _) = broadcast::channel(256);
    let activity_log = Arc::new(ActivityLog::new(
        config.server.activity_log_max_entries,
        session_events.clone(),
    ));

    let exec_results_cache = Arc::new(ExecResultsCache::new(config.server.exec_result_cache_size));

    let transfer_config = TransferConfig::new(
        config.server.max_concurrent_transfers,
        config.server.transfer_chunk_size,
        config.server.transfer_max_file_size,
        config.server.transfer_stale_timeout_secs,
    );
    let transfer_manager = Arc::new(TransferManager::new(
        transfer_config,
        session_events.clone(),
    ));

    // GPS state (only when [gps] config is present)
    let gps_state = config.gps.as_ref().map(|gc| {
        info!(
            "GPS tracking enabled (poll: {}s, hint: {:?})",
            gc.poll_interval_secs, gc.device
        );
        Arc::new(tokio::sync::Mutex::new(gps::GpsState::new(gc.history_size)))
    });

    // LTE state (only when [lte] config is present)
    let lte_state = config.lte.as_ref().map(|lc| {
        info!(
            "LTE monitoring enabled (poll: {}s, hint: {:?})",
            lc.poll_interval_secs, lc.device
        );
        let mut ls = lte::LteState::new();
        ls.load_safe_bands(&data_dir);
        ls.load_lte_data(&data_dir);
        Arc::new(tokio::sync::Mutex::new(ls))
    });

    // Tunnel event persistence: load previous events from disk
    let events_path = std::path::Path::new(&data_dir).join("tunnel_events.json");
    let mut tun_stats = TunnelStats::new();
    tun_stats.events = tokio::sync::Mutex::new(TunnelStats::load_events(&events_path));
    tun_stats.events_path = Some(events_path);

    // ─── Infra monitoring state ───────────────────────────────────
    let infra_state = {
        let mut is = infra::InfraState::new(&config.server.data_dir);
        is.load_config();
        Arc::new(tokio::sync::Mutex::new(is))
    };

    let modem_detected_path = Arc::new(tokio::sync::RwLock::new(None::<String>));

    let mut state = AppState {
        session_manager,
        config: Arc::new(config),
        start_time: Instant::now(),
        session_events,
        activity_log,
        exec_results_cache,
        tunnel_stats: Arc::new(tun_stats),
        transfer_manager,
        sse_connections: Arc::new(AtomicU32::new(0)),
        gps_state,
        lte_state,
        modem: None,
        modem_detected_path: modem_detected_path.clone(),
        lte_poll_notify: None,
        relay_history: None,
        device_snapshots: None,
        relay_state: None,
        watchdog_snapshot: None,
        infra_state: Some(infra_state.clone()),
    };

    // Build router
    let public_routes = Router::new().route("/api/health", get(routes::health::health));

    let authed_routes = Router::new()
        .route("/api/info", get(routes::info::info))
        .route(
            "/api/safe_mode/flag",
            get(routes::safe_mode::get_flag).delete(routes::safe_mode::clear_flag),
        )
        .route("/api/diagnostics", get(routes::diagnostics::diagnostics))
        .route("/api/exec", post(routes::exec::exec))
        .route("/api/exec/batch", post(routes::exec::batch_exec))
        .route(
            "/api/files",
            get(routes::files::get_file)
                .put(routes::files::put_file)
                .delete(routes::files::delete_file),
        )
        .route("/api/files/raw", get(routes::files::download_file))
        .route("/api/files/upload", post(routes::files::upload_file))
        .route("/api/activity", get(routes::activity::get_activity))
        .route(
            "/api/activity/{id}/result",
            get(routes::activity::get_exec_result),
        )
        .route("/api/sessions", get(routes::sessions::list_sessions))
        .route(
            "/api/sessions/{id}",
            delete(routes::sessions::kill_session).patch(routes::sessions::patch_session),
        )
        .route(
            "/api/sessions/{id}/signal",
            post(routes::sessions::signal_session),
        )
        .route("/api/shells", get(routes::shells::list_shells))
        .route("/api/events", get(routes::events::event_stream))
        .route("/api/stp/download", post(routes::stp::init_download))
        .route("/api/stp/upload", post(routes::stp::init_upload))
        .route(
            "/api/stp/chunk/{xfer}/{idx}",
            get(routes::stp::get_chunk).post(routes::stp::post_chunk),
        )
        .route("/api/stp/resume/{xfer}", post(routes::stp::resume_transfer))
        .route("/api/stp/status/{xfer}", get(routes::stp::transfer_status))
        .route("/api/stp/transfers", get(routes::stp::list_transfers))
        .route("/api/stp/{xfer}", delete(routes::stp::abort_transfer))
        .route("/api/playbooks", get(routes::playbooks::list_playbooks))
        .route(
            "/api/playbooks/{name}",
            get(routes::playbooks::get_playbook)
                .put(routes::playbooks::put_playbook)
                .delete(routes::playbooks::delete_playbook),
        )
        .route("/api/gps", get(routes::gps::gps))
        .route("/api/lte", get(routes::lte::lte))
        .route("/api/lte/bands", post(routes::lte::set_bands))
        .route("/api/lte/scan", post(routes::lte::start_scan))
        .route("/api/lte/speedtest", post(routes::lte::speed_test))
        .route("/api/lte/usb_cycle", post(routes::lte::manual_usb_cycle))
        .route(
            "/api/lte/watchdog/history",
            get(routes::lte::watchdog_history),
        )
        .route(
            "/api/infra/config",
            post(infra::routes::push_config).delete(infra::routes::delete_config),
        )
        .route("/api/infra/results", get(infra::routes::get_results))
        .route(
            "/api/infra/check/{target_id}",
            post(infra::routes::check_target),
        )
        .route("/api/infra/discover", post(infra::discovery::discover))
        .route(
            "/api/infra/discover/progress",
            get(infra::routes::discover_progress),
        )
        .route(
            "/api/infra/discover/subnets",
            get(infra::routes::discover_subnets),
        )
        .layer(middleware::from_fn(sctl::auth::require_api_key));

    let ws_route = Router::new().route("/api/ws", get(ws::ws_upgrade));

    // GUARD: Headers must be listed explicitly — `allow_headers(Any)` works in
    // Chrome but Firefox rejects credentialed requests without explicit listing.
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers([
            axum::http::header::AUTHORIZATION,
            axum::http::header::CONTENT_TYPE,
            axum::http::header::ACCEPT,
            axum::http::HeaderName::from_static("x-gx-chunk-hash"),
            axum::http::HeaderName::from_static("x-gx-chunk-index"),
            axum::http::HeaderName::from_static("x-gx-transfer-id"),
        ])
        .expose_headers([
            axum::http::HeaderName::from_static("x-gx-chunk-hash"),
            axum::http::HeaderName::from_static("x-gx-chunk-index"),
            axum::http::HeaderName::from_static("x-gx-transfer-id"),
        ]);

    // Tunnel: create relay state early so relay_history is set before .with_state() clones
    let tunnel_config = state.config.tunnel.clone();
    let mut relay_state_opt: Option<tunnel::relay::RelayState> = None;
    if let Some(ref tc) = tunnel_config {
        if tc.relay {
            info!("Tunnel relay mode enabled");
            let relay_state = tunnel::relay::RelayState::new(
                tc.tunnel_key.clone(),
                tc.heartbeat_timeout_secs,
                tc.tunnel_proxy_timeout_secs,
                Some(&data_dir),
            );
            // Seed connection history from journald (survives restarts)
            relay_state.history.seed_from_journal().await;
            state.relay_history = Some(relay_state.history.clone());
            state.device_snapshots = Some(relay_state.device_snapshots.clone());
            state.relay_state = Some(relay_state.clone());
            relay_state_opt = Some(relay_state);
        }
    }

    // Open the cellular modem (shared by GPS + LTE). There is at most one
    // Quectel module per BPI; sysfs is authoritative for the actual ttyUSB
    // path, with the configured hint as a fallback only when detection fails.
    //
    // `modem_tx` broadcasts handle replacements after a USB power cycle (see
    // lte_watchdog). It is created lazily on first successful open and taken
    // by the watchdog if one is spawned. If no watchdog is spawned, it is
    // dropped — pollers self-recover via `detect_quectel_at_port` on
    // channel-closed.
    //
    // In safe mode the modem (and everything that depends on it) is skipped
    // entirely; the management plane comes up regardless.
    let mut modem_handle: Option<sctl::modem::Modem> = None;
    let mut modem_tx: Option<watch::Sender<sctl::modem::Modem>> = None;

    let gps_config = state.config.gps.clone();
    let lte_config = state.config.lte.clone();

    // Either subsystem being configured is reason enough to attempt to open
    // the modem. Hint is taken from whichever config has one (LTE first).
    let want_modem = (gps_config.is_some() || lte_config.is_some()) && !safe_mode_active;
    let modem_hint: Option<String> = lte_config
        .as_ref()
        .and_then(|c| c.device.clone())
        .or_else(|| gps_config.as_ref().and_then(|c| c.device.clone()));

    if want_modem {
        // Detection → hint → None. Three open attempts at the resolved path.
        let resolved = sctl::modem::detect_quectel_at_port_strict().or_else(|| modem_hint.clone());
        match resolved {
            Some(path) => {
                if let Some(ref hint) = modem_hint {
                    if hint != &path {
                        info!("Modem: detected {path} (config hint: {hint}) — using detected path");
                    }
                }
                for attempt in 0..5 {
                    match sctl::modem::Modem::open(&path) {
                        Ok(m) => {
                            if attempt > 0 {
                                info!("Modem {path}: opened on attempt {}", attempt + 1);
                            }
                            let (tx, _rx) = watch::channel(m.clone());
                            modem_tx = Some(tx);
                            modem_handle = Some(m);
                            *modem_detected_path.write().await = Some(path.clone());
                            // Persist for post-mortem visibility — NOT for next-boot authority.
                            persist_modem_state(&data_dir, &path);
                            break;
                        }
                        Err(e) => {
                            if attempt < 4 {
                                info!("Modem {path}: not ready ({e}), retrying in 2s...");
                                std::thread::sleep(std::time::Duration::from_secs(2));
                            } else {
                                warn!("Failed to open modem {path} after 5 attempts: {e}");
                            }
                        }
                    }
                }
            }
            None => {
                warn!(
                    "No Quectel modem detected and no [lte]/[gps] device hint configured — \
                     GPS/LTE/watchdog will be disabled this boot. Management plane unaffected."
                );
            }
        }
    }

    let gps_modem = modem_handle.clone();
    let lte_modem = modem_handle.clone();
    state.modem = lte_modem.clone();

    if let (Some(ref modem), Some(ref ls)) = (&lte_modem, &state.lte_state) {
        // SIM change detection — must run BEFORE safe-bands restore so stale
        // carrier-specific bands are cleared before we try to apply them.
        let lte_cfg = state.config.lte.as_ref().unwrap();
        let sim_changed = lte::detect_sim_change(modem, ls, &data_dir, lte_cfg).await;

        if sim_changed {
            info!("Startup: SIM changed, skipping safe-bands restore");
        } else {
            let safe = ls.lock().await.safe_bands.clone();
            if let Some(ref safe_cfg) = safe {
                let safe_hex = lte::bands_to_hex(&safe_cfg.bands);
                match modem.command("AT+QCFG=\"band\"").await {
                    Ok(resp) => {
                        let current = lte::parse_band_config(&resp);
                        let current_hex = lte::bands_to_hex(&current);
                        if current_hex == safe_hex {
                            info!(bands = %current_hex, "Startup: bands match safe config");
                        } else {
                            info!(
                                current = %current_hex,
                                safe = %safe_hex,
                                "Startup: bands differ from safe config, restoring"
                            );
                            match lte::verified_set_bands(modem, &safe_cfg.bands).await {
                                Ok(actual) => {
                                    info!("Startup: safe bands restored and verified: {actual:?}");
                                }
                                Err(e) => warn!("Startup: safe bands restore failed: {e}"),
                            }
                            if let Some(pri) = safe_cfg.priority_band {
                                let pri_cmd = format!("AT+QCFG=\"bandpri\",{pri}");
                                if let Err(e) = modem.command(&pri_cmd).await {
                                    warn!("Startup: failed to set band priority: {e}");
                                }
                            }
                            if let Err(e) = modem.command("AT+COPS=0").await {
                                warn!("Startup: failed to re-register network: {e}");
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Startup: failed to query current bands: {e}");
                    }
                }
            } else {
                info!("Startup: no safe bands saved, skipping restore");
            }
        }
    }

    // Pre-create watchdog snapshot so the router clone of state includes it.
    // The actual watchdog task is spawned later; it receives the same Arc.
    let will_run_watchdog = state.config.lte.as_ref().is_some_and(|lc| lc.watchdog)
        && tunnel_config
            .as_ref()
            .is_some_and(|tc| tc.url.is_some() && !tc.relay);
    if will_run_watchdog {
        state.watchdog_snapshot = Some(Arc::new(tokio::sync::Mutex::new(
            sctl::lte_watchdog::WatchdogSnapshot::new(),
        )));
    }

    let mut app = Router::new()
        .merge(public_routes)
        .merge(authed_routes)
        .merge(ws_route)
        .layer(Extension(ApiKey(state.config.auth.api_key.clone())))
        .with_state(state.clone());

    // Tunnel: add relay routes if configured (before global layers so CORS/tracing apply)
    if let Some(ref relay_state) = relay_state_opt {
        let relay_routes = tunnel::relay::relay_router(relay_state.clone());
        app = app.merge(relay_routes);
    }

    // sctlin web UI: reverse proxy /sctlin/* → localhost:3000 (relay mode only)
    if relay_state_opt.is_some() {
        app = app.fallback(sctlin_proxy::sctlin_proxy);
    }

    // GUARD: .layer() only applies to routes merged BEFORE the call.
    let app = app.layer(cors).layer(TraceLayer::new_for_http()).layer(
        tower::limit::ConcurrencyLimitLayer::new(state.config.server.max_connections),
    );

    let listener = TcpListener::bind(&state.config.server.listen)
        .await
        .expect("Failed to bind");

    info!("Server ready");

    // Tunnel: spawn client if configured, with panic-recovery supervisor.
    // If the tunnel task panics it will be restarted after 5s. A normal return
    // (e.g. permanent auth error) stops the supervisor loop.
    let _tunnel_client_task = if let Some(ref tc) = tunnel_config {
        if tc.url.is_some() && !tc.relay {
            info!(
                "Tunnel client mode enabled, will connect to {}",
                tc.url.as_deref().unwrap()
            );
            let tc = tc.clone();
            let tunnel_state = state.clone();
            Some(tokio::spawn(async move {
                loop {
                    let handle = tunnel::client::spawn(tunnel_state.clone(), tc.clone());
                    match handle.await {
                        Ok(()) => {
                            // Normal return — tunnel client decided to stop (e.g. permanent auth error)
                            info!("Tunnel client exited normally, not restarting");
                            break;
                        }
                        Err(e) => {
                            // JoinError means panic — restart after delay
                            tracing::error!("Tunnel client panicked: {e}, restarting in 5s");
                            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                        }
                    }
                }
            }))
        } else {
            None
        }
    } else {
        None
    };

    let is_tunnel_client = tunnel_config
        .as_ref()
        .is_some_and(|tc| tc.url.is_some() && !tc.relay);

    // GPS poller (only when [gps] config is present AND the modem is open).
    // Polls at reduced rate when LTE data path is active to avoid QMI disruption.
    let gps_task = match (
        gps_config,
        state.gps_state.as_ref(),
        gps_modem.as_ref(),
        modem_tx.as_ref(),
    ) {
        (Some(gc), Some(gs), Some(modem), Some(tx)) => {
            let modem_rx = tx.subscribe();
            let lte_iface = state.config.lte.as_ref().map(|lc| lc.interface.clone());
            Some(gps::spawn_gps_poller(
                gc,
                modem.clone(),
                gs.clone(),
                state.session_events.clone(),
                modem_rx,
                lte_iface,
            ))
        }
        (Some(_), _, None, _) => {
            warn!("GPS poller: not spawning — modem unavailable this boot");
            None
        }
        _ => None,
    };

    // LTE poller (only when [lte] config is present AND the modem is open).
    let lte_task = match (
        lte_config,
        state.lte_state.as_ref(),
        lte_modem.as_ref(),
        modem_tx.as_ref(),
    ) {
        (Some(lc), Some(ls), Some(modem), Some(tx)) => {
            let modem_rx = tx.subscribe();
            let poll_notify = Arc::new(tokio::sync::Notify::new());
            state.lte_poll_notify = Some(poll_notify.clone());
            Some(lte::spawn_lte_poller(
                lc,
                modem.clone(),
                ls.clone(),
                state.session_events.clone(),
                modem_rx,
                data_dir.clone(),
                state.tunnel_stats.clone(),
                poll_notify,
            ))
        }
        (Some(_), _, None, _) => {
            warn!("LTE poller: not spawning — modem unavailable this boot");
            None
        }
        _ => None,
    };

    // LTE watchdog (only when [lte] watchdog=true AND tunnel client mode active
    // AND the modem is open). Takes ownership of the watch::Sender so it can
    // broadcast new modem handles after USB power cycle.
    let watchdog_task = match (
        state.config.lte.clone(),
        state.lte_state.as_ref(),
        lte_modem.as_ref(),
        modem_tx.take(),
    ) {
        (Some(lc), Some(ls), Some(modem), Some(tx)) if lc.watchdog && is_tunnel_client => {
            info!("LTE watchdog enabled (interface: {})", lc.interface);
            let tunnel_url = tunnel_config.as_ref().and_then(|tc| tc.url.clone());
            let wd_snapshot = state
                .watchdog_snapshot
                .clone()
                .expect("watchdog_snapshot must be pre-created");
            Some(sctl::lte_watchdog::spawn_lte_watchdog(
                modem.clone(),
                tx,
                ls.clone(),
                state.tunnel_stats.clone(),
                state.session_events.clone(),
                lc,
                data_dir.clone(),
                tunnel_url,
                wd_snapshot,
                modem_detected_path.clone(),
            ))
        }
        (Some(lc), _, None, _) if lc.watchdog && is_tunnel_client => {
            warn!("LTE watchdog: not spawning — modem unavailable this boot");
            None
        }
        _ => None,
    };

    // Start infra monitor if config was loaded from disk (skipped in safe mode)
    if !safe_mode_active {
        let mut guard = infra_state.lock().await;
        if let Some(ref cfg) = guard.config {
            info!(
                "Infra: resuming monitoring with {} targets (config v{})",
                cfg.targets.len(),
                cfg.version
            );
            let handle = infra::monitor::spawn_monitor(infra_state.clone(), cfg.clone());
            guard.monitor_handle = Some(handle);
        }
    }

    // Periodic sweep: clean up sessions whose process has exited + stale transfers
    let mgr = state.session_manager.clone();
    let sweep_tx = state.session_events.clone();
    let sweep_transfers = state.transfer_manager.clone();
    let sweep_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
        loop {
            interval.tick().await;
            let events = mgr.sweep().await;
            for event in events {
                match event {
                    sessions::SweepEvent::Destroyed(session_id, reason) => {
                        let _ = sweep_tx.send(serde_json::json!({
                            "type": "session.destroyed",
                            "session_id": session_id,
                            "reason": reason,
                        }));
                    }
                    sessions::SweepEvent::AiAutoCleared(session_id) => {
                        let _ = sweep_tx.send(serde_json::json!({
                            "type": "session.ai_status_changed",
                            "session_id": session_id,
                            "working": false,
                        }));
                    }
                }
            }
            // Sweep stale gawdxfer transfers
            sweep_transfers.sweep_stale().await;
        }
    });

    // Tunnel relay: periodic sweep to evict dead devices
    let relay_sweep_task = relay_state_opt.clone().map(|rs| {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(15));
            loop {
                interval.tick().await;
                rs.sweep_dead_devices().await;
            }
        })
    });

    // Tunnel relay: periodic snapshot persistence (60s, debounced via dirty flag)
    let relay_snapshot_task = relay_state_opt.clone().map(|rs| {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
            loop {
                interval.tick().await;
                rs.save_snapshots().await;
            }
        })
    });

    // Tunnel events: periodic persistence (60s, debounced via dirty flag)
    let tunnel_events_flush_task = {
        let flush_stats = state.tunnel_stats.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
            loop {
                interval.tick().await;
                flush_stats.save_events().await;
            }
        })
    };

    // Graceful shutdown
    let shutdown = async {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        {
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("Failed to register SIGTERM");
            tokio::select! {
                _ = ctrl_c => info!("Received SIGINT"),
                _ = sigterm.recv() => info!("Received SIGTERM"),
            }
        }
        #[cfg(not(unix))]
        {
            ctrl_c.await.ok();
            info!("Received SIGINT");
        }
    };

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
        .expect("Server error");

    // Cleanup
    info!("Shutting down...");
    sweep_task.abort();
    tunnel_events_flush_task.abort();
    if let Some(task) = relay_sweep_task {
        task.abort();
    }
    if let Some(task) = relay_snapshot_task {
        task.abort();
    }

    // Tunnel relay: notify devices, drain state, and do a final snapshot save
    if let Some(ref rs) = relay_state_opt {
        info!("Notifying tunnel devices of relay shutdown...");
        rs.broadcast_to_devices(serde_json::json!({
            "type": "tunnel.relay_shutdown",
        }))
        .await;
        rs.drain_all().await;
        if rs.save_snapshots().await {
            info!("Saved device snapshots to disk");
        }
    }

    // GPS: abort poller and disable GNSS
    if let Some(task) = gps_task {
        task.abort();
    }
    if let Some(ref modem) = gps_modem {
        gps::disable_gnss(modem).await;
    }

    // LTE: abort poller and watchdog
    if let Some(task) = lte_task {
        task.abort();
    }
    if let Some(task) = watchdog_task {
        task.abort();
    }

    // Tunnel events: final flush
    if state.tunnel_stats.save_events().await {
        info!("Saved tunnel events to disk");
    }

    state.session_manager.kill_all().await;
    info!("Goodbye");
}
