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
    config::Config,
    gps, lte, routes, sessions,
    sessions::SessionManager,
    state::{AppState, TunnelStats},
    tunnel, ws, ExecResultsCache,
};

/// Remote shell control service for Linux devices.
#[derive(Parser)]
#[command(name = "sctl", version)]
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
        Some(Commands::Serve { config }) => {
            run_server(config.as_deref()).await;
        }
        None => {
            // Backward compat: no subcommand but --config may be passed
            let args: Vec<String> = std::env::args().collect();
            let config_path = args
                .windows(2)
                .find(|w| w[0] == "--config")
                .map(|w| w[1].clone());
            run_server(config_path.as_deref()).await;
        }
    }
}

async fn run_supervisor_mode(config_path: Option<&str>) -> ! {
    let config = Config::load(config_path);

    // Initialize tracing for supervisor
    let log_filter = std::env::var("RUST_LOG").unwrap_or_else(|_| config.logging.level.clone());
    tracing_subscriber::fmt().with_env_filter(log_filter).init();

    info!("sctl supervisor starting");
    supervisor::run_supervisor(config_path, &config.supervisor).await
}

#[allow(clippy::too_many_lines)]
async fn run_server(config_path: Option<&str>) {
    let config = Config::load(config_path);

    // Initialize tracing
    let log_filter = std::env::var("RUST_LOG").unwrap_or_else(|_| config.logging.level.clone());
    tracing_subscriber::fmt().with_env_filter(log_filter).init();

    // Validate config before proceeding
    let validation_errors = config.validate();
    if !validation_errors.is_empty() {
        for err in &validation_errors {
            tracing::error!("Config error: {err}");
        }
        std::process::exit(1);
    }

    info!("sctl v{} starting", env!("CARGO_PKG_VERSION"));
    info!("Device serial: {}", config.device.serial);
    info!("Listening on {}", config.server.listen);

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
            "GPS tracking enabled (device: {}, poll: {}s)",
            gc.device, gc.poll_interval_secs
        );
        Arc::new(tokio::sync::Mutex::new(gps::GpsState::new(gc.history_size)))
    });

    // LTE state (only when [lte] config is present)
    let lte_state = config.lte.as_ref().map(|lc| {
        info!(
            "LTE monitoring enabled (device: {}, poll: {}s)",
            lc.device, lc.poll_interval_secs
        );
        Arc::new(tokio::sync::Mutex::new(lte::LteState::new()))
    });

    let state = AppState {
        session_manager,
        config: Arc::new(config),
        start_time: Instant::now(),
        session_events,
        activity_log,
        exec_results_cache,
        tunnel_stats: Arc::new(TunnelStats::new()),
        transfer_manager,
        sse_connections: Arc::new(AtomicU32::new(0)),
        gps_state,
        lte_state,
    };

    // Build router
    let public_routes = Router::new().route("/api/health", get(routes::health::health));

    let authed_routes = Router::new()
        .route("/api/info", get(routes::info::info))
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

    let mut app = Router::new()
        .merge(public_routes)
        .merge(authed_routes)
        .merge(ws_route)
        .layer(Extension(ApiKey(state.config.auth.api_key.clone())))
        .with_state(state.clone());

    // Tunnel: add relay routes if configured (before global layers so CORS/tracing apply)
    let tunnel_config = state.config.tunnel.clone();
    let mut relay_state_opt: Option<tunnel::relay::RelayState> = None;
    if let Some(ref tc) = tunnel_config {
        if tc.relay {
            info!("Tunnel relay mode enabled");
            let relay_state = tunnel::relay::RelayState::new(
                tc.tunnel_key.clone(),
                tc.heartbeat_timeout_secs,
                tc.tunnel_proxy_timeout_secs,
            );
            let relay_routes = tunnel::relay::relay_router(relay_state.clone());
            app = app.merge(relay_routes);
            relay_state_opt = Some(relay_state);
        }
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

    // Open modem instances (deduplicated by device path) for GPS/LTE pollers
    let mut modems: std::collections::HashMap<String, sctl::modem::Modem> =
        std::collections::HashMap::new();

    // Helper: get or create modem for a device path
    let get_modem = |modems: &mut std::collections::HashMap<String, sctl::modem::Modem>,
                     device: &str| {
        if let Some(m) = modems.get(device) {
            return Some(m.clone());
        }
        match sctl::modem::Modem::open(device) {
            Ok(m) => {
                modems.insert(device.to_string(), m.clone());
                Some(m)
            }
            Err(e) => {
                warn!("Failed to open modem {device}: {e}");
                None
            }
        }
    };

    // GPS poller (only when [gps] config is present)
    let gps_config = state.config.gps.clone();
    let gps_modem = gps_config
        .as_ref()
        .and_then(|gc| get_modem(&mut modems, &gc.device));
    let gps_task =
        if let (Some(gc), Some(ref gs), Some(modem)) = (gps_config, &state.gps_state, &gps_modem) {
            Some(gps::spawn_gps_poller(
                gc,
                modem.clone(),
                gs.clone(),
                state.session_events.clone(),
            ))
        } else {
            None
        };

    // LTE poller (only when [lte] config is present)
    let lte_config = state.config.lte.clone();
    let lte_modem = lte_config
        .as_ref()
        .and_then(|lc| get_modem(&mut modems, &lc.device));
    let lte_task =
        if let (Some(lc), Some(ref ls), Some(modem)) = (lte_config, &state.lte_state, &lte_modem) {
            Some(lte::spawn_lte_poller(
                lc,
                modem.clone(),
                ls.clone(),
                state.session_events.clone(),
            ))
        } else {
            None
        };

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
    if let Some(task) = relay_sweep_task {
        task.abort();
    }

    // Tunnel relay: notify devices and drain state before stopping
    if let Some(ref rs) = relay_state_opt {
        info!("Notifying tunnel devices of relay shutdown...");
        rs.broadcast_to_devices(serde_json::json!({
            "type": "tunnel.relay_shutdown",
        }))
        .await;
        rs.drain_all().await;
    }

    // GPS: abort poller and disable GNSS
    if let Some(task) = gps_task {
        task.abort();
    }
    if let Some(ref modem) = gps_modem {
        gps::disable_gnss(modem).await;
    }

    // LTE: abort poller
    if let Some(task) = lte_task {
        task.abort();
    }

    state.session_manager.kill_all().await;
    info!("Goodbye");
}
