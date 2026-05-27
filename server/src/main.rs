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
use tokio::sync::broadcast;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing::{info, warn};

use sctl::{
    activity::ActivityLog,
    auth::ApiKey,
    comms,
    config::Config,
    infra, routes, sessions,
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

    // Best-effort: self-heal persistent log capture on OpenWrt. No-op elsewhere.
    sctl::platform::openwrt::ensure_persistent_logs().await;

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
        activity_log.clone(),
    ));

    if let Some(gc) = config.gps.as_ref() {
        info!("GPS tracking enabled (poll: {}s)", gc.poll_interval_secs);
    }
    if let Some(lc) = config.lte.as_ref() {
        info!(
            "LTE monitoring enabled (poll: {}s, interface: {})",
            lc.poll_interval_secs, lc.interface
        );
    }

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
        comms_client: None,
        comms_state: None,
        comms_poll_notify: None,
        relay_history: None,
        device_snapshots: None,
        relay_state: None,
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

    let mut comms_task = None;
    if safe_mode_active {
        info!("Comms provider skipped in safe mode");
    } else if let Some(comms_cfg) = state.config.effective_comms_config() {
        info!(
            "Starting comms provider '{}' via {}",
            comms_cfg.provider,
            comms_cfg.effective_command()
        );
        match comms::start_provider(&state.config, &comms_cfg).await {
            Ok((client, comms_snapshot)) => {
                let comms_state = Arc::new(tokio::sync::Mutex::new(comms_snapshot));
                let notify = Arc::new(tokio::sync::Notify::new());
                comms_task = Some(comms::spawn_poller(
                    client.clone(),
                    comms_state.clone(),
                    state.config.gps.is_some(),
                    state
                        .config
                        .gps
                        .as_ref()
                        .map_or(30, |gc| gc.poll_interval_secs),
                    state.config.lte.is_some(),
                    state
                        .config
                        .lte
                        .as_ref()
                        .map_or(60, |lc| lc.poll_interval_secs),
                    state.tunnel_stats.clone(),
                    notify.clone(),
                ));
                state.comms_client = Some(client);
                state.comms_state = Some(comms_state);
                state.comms_poll_notify = Some(notify);
            }
            Err(err) => {
                warn!(
                    "Comms provider '{}' unavailable: {err}. Management plane unaffected.",
                    comms_cfg.provider
                );
                state.comms_state = Some(Arc::new(tokio::sync::Mutex::new(
                    comms::CommsState::new(comms_cfg.provider),
                )));
            }
        }
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

    if let Some(ref client) = state.comms_client {
        let _ = client
            .call(
                sctl_comms_protocol::methods::LOCATION_DISABLE,
                serde_json::json!({}),
            )
            .await;
    }
    if let Some(task) = comms_task {
        task.abort();
    }

    // Tunnel events: final flush
    if state.tunnel_stats.save_events().await {
        info!("Saved tunnel events to disk");
    }

    state.session_manager.kill_all().await;
    info!("Goodbye");
}
