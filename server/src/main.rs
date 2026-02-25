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
//!
//! ## API surface
//!
//! | Method | Path                        | Auth | Description                          |
//! |--------|-----------------------------|------|--------------------------------------|
//! | GET    | `/api/health`               | No   | Liveness probe                       |
//! | GET    | `/api/info`                 | Yes  | System info (IPs, CPU, mem, disk)    |
//! | POST   | `/api/exec`                 | Yes  | One-shot command execution           |
//! | POST   | `/api/exec/batch`           | Yes  | Batch command execution              |
//! | GET    | `/api/files`                | Yes  | Read file or list directory          |
//! | PUT    | `/api/files`                | Yes  | Write file (atomic)                  |
//! | DELETE | `/api/files`                | Yes  | Delete a file                        |
//! | GET    | `/api/activity`             | Yes  | Activity journal (with filters)      |
//! | GET    | `/api/sessions`             | Yes  | List all sessions                    |
//! | POST   | `/api/sessions/{id}/signal` | Yes  | Send POSIX signal to session         |
//! | DELETE | `/api/sessions/{id}`        | Yes  | Kill a session                       |
//! | PATCH  | `/api/sessions/{id}`        | Yes  | Rename / set AI permission & status  |
//! | GET    | `/api/shells`               | Yes  | List available shells                |
//! | GET    | `/api/events`               | Yes  | SSE event stream (real-time)         |
//! | GET    | `/api/playbooks`            | Yes  | List playbooks                       |
//! | GET    | `/api/playbooks/{name}`     | Yes  | Get playbook detail                  |
//! | PUT    | `/api/playbooks/{name}`     | Yes  | Create/update playbook               |
//! | DELETE | `/api/playbooks/{name}`     | Yes  | Delete playbook                      |
//! | GET    | `/api/ws`                   | Yes* | WebSocket for interactive sessions   |
//!
//! *WebSocket auth is via `?token=<key>` query param (no `Authorization` header
//! available during the upgrade handshake).
//!
//! ### Tunnel endpoints (when `tunnel.relay = true`)
//!
//! | Method | Path                                     | Auth         | Description                 |
//! |--------|------------------------------------------|--------------|-----------------------------|
//! | GET    | `/api/tunnel/register`                   | `tunnel_key` | Device WS registration      |
//! | GET    | `/api/tunnel/devices`                    | `tunnel_key` | List connected devices      |
//! | GET    | `/d/{serial}/api/health`                 | No           | Proxied device health       |
//! | GET    | `/d/{serial}/api/info`                   | `api_key`    | Proxied device info         |
//! | POST   | `/d/{serial}/api/exec`                   | `api_key`    | Proxied command execution   |
//! | POST   | `/d/{serial}/api/exec/batch`             | `api_key`    | Proxied batch execution     |
//! | GET    | `/d/{serial}/api/files`                  | `api_key`    | Proxied file read/list      |
//! | PUT    | `/d/{serial}/api/files`                  | `api_key`    | Proxied file write          |
//! | DELETE | `/d/{serial}/api/files`                  | `api_key`    | Proxied file delete         |
//! | GET    | `/d/{serial}/api/files/raw`              | `api_key`    | Proxied file download       |
//! | POST   | `/d/{serial}/api/files/upload`           | `api_key`    | Proxied file upload         |
//! | GET    | `/d/{serial}/api/activity`               | `api_key`    | Proxied activity journal    |
//! | GET    | `/d/{serial}/api/sessions`               | `api_key`    | Proxied session list        |
//! | POST   | `/d/{serial}/api/sessions/{id}/signal`   | `api_key`    | Proxied session signal      |
//! | DELETE | `/d/{serial}/api/sessions/{id}`          | `api_key`    | Proxied session kill        |
//! | PATCH  | `/d/{serial}/api/sessions/{id}`          | `api_key`    | Proxied session patch       |
//! | GET    | `/d/{serial}/api/shells`                 | `api_key`    | Proxied shell list          |
//! | GET    | `/d/{serial}/api/playbooks`              | `api_key`    | Proxied playbook list       |
//! | *      | `/d/{serial}/api/playbooks/{name}`       | `api_key`    | Proxied playbook CRUD       |
//! | GET    | `/d/{serial}/api/ws`                     | `api_key`    | Proxied WS sessions         |
//!
//! Note: SSE (`/api/events`) is not proxied through the tunnel (incompatible
//! with REST-over-WS relay pattern). Tunneled devices use WS proxy for events.
//!
//! ## Architecture
//!
//! ```text
//! main.rs          — entry point, clap subcommands, router setup, graceful shutdown
//! supervisor.rs    — built-in supervisor (fork/restart loop)
//! activity.rs      — in-memory activity journal (ring buffer + broadcast)
//! auth.rs          — Bearer token middleware, constant-time comparison
//! config.rs        — TOML + env-var configuration
//! routes/
//!   health.rs      — GET /api/health
//!   info.rs        — GET /api/info (system introspection)
//!   exec.rs        — POST /api/exec, POST /api/exec/batch
//!   files.rs       — GET/PUT/DELETE /api/files (read, write, list, delete)
//!   activity.rs    — GET /api/activity (operation log, filtered)
//!   sessions.rs    — GET/POST/DELETE/PATCH /api/sessions (list, signal, kill, patch)
//!   shells.rs      — GET /api/shells (shell discovery)
//!   events.rs      — GET /api/events (SSE event stream)
//!   playbooks.rs   — CRUD /api/playbooks (list, get, put, delete)
//! shell/
//!   process.rs     — spawn_shell(), spawn_shell_pgroup(), exec_command()
//!   pty.rs         — PTY allocation, spawn, resize
//! sessions/
//!   buffer.rs      — OutputBuffer ring buffer with Notify wakeup
//!   session.rs     — ManagedSession (buffer-backed, process groups, PTY, signals)
//!   journal.rs     — Disk-backed output journal for crash recovery
//!   mod.rs         — SessionManager (lifecycle, attach/detach, sweep, recovery)
//! ws/
//!   mod.rs         — WebSocket upgrade, message dispatch, subscriber pattern
//! tunnel/
//!   mod.rs         — Reverse tunnel module root
//!   client.rs      — Outbound WS to relay, reconnect, handles proxied requests
//!   relay.rs       — Device registration, client routing, REST/WS proxy
//! ```

mod activity;
mod auth;
mod config;
mod gawdxfer;
mod routes;
mod sessions;
mod shell;
mod supervisor;
mod tunnel;
mod util;
mod ws;

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64};
use std::sync::Arc;
use std::time::Instant;

use gawdxfer::manager::TransferManager;
use gawdxfer::types::TransferConfig;

use axum::{
    middleware,
    routing::{delete, get, post},
    Extension, Router,
};
use clap::{Parser, Subcommand};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing::{info, warn};

use activity::{ActivityLog, ExecResultsCache};
use auth::ApiKey;
use config::Config;
use sessions::SessionManager;

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

/// Shared application state passed to every handler via Axum's `State` extractor.
#[derive(Clone)]
pub struct AppState {
    /// Immutable configuration loaded at startup.
    pub config: Arc<Config>,
    /// Monotonic instant when the server started (for uptime calculation).
    pub start_time: Instant,
    /// Manages the pool of interactive WebSocket shell sessions.
    pub session_manager: SessionManager,
    /// Broadcast channel for session lifecycle events (created/destroyed/renamed).
    /// All connected WebSocket clients subscribe to receive real-time updates.
    pub session_events: broadcast::Sender<Value>,
    /// In-memory activity journal for REST/WS operation tracking.
    pub activity_log: Arc<ActivityLog>,
    /// In-memory cache of full exec results, keyed by activity ID.
    pub exec_results_cache: Arc<ExecResultsCache>,
    /// Whether the tunnel client is currently connected to the relay.
    pub tunnel_connected: Arc<AtomicBool>,
    /// Number of tunnel reconnects since startup.
    pub tunnel_reconnects: Arc<AtomicU64>,
    /// Chunked file transfer manager (gawdxfer).
    pub transfer_manager: Arc<TransferManager>,
    /// Current number of SSE connections (for connection limiting).
    pub sse_connections: Arc<AtomicU32>,
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

    let state = AppState {
        session_manager,
        config: Arc::new(config),
        start_time: Instant::now(),
        session_events,
        activity_log,
        exec_results_cache,
        tunnel_connected: Arc::new(AtomicBool::new(false)),
        tunnel_reconnects: Arc::new(AtomicU64::new(0)),
        transfer_manager,
        sse_connections: Arc::new(AtomicU32::new(0)),
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
        .layer(middleware::from_fn(auth::require_api_key));

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
    // All routes (including relay) MUST be merged above this point, otherwise
    // CORS/tracing headers won't be added and browsers will block requests.
    let app = app.layer(cors).layer(TraceLayer::new_for_http()).layer(
        tower::limit::ConcurrencyLimitLayer::new(state.config.server.max_connections),
    );

    let listener = TcpListener::bind(&state.config.server.listen)
        .await
        .expect("Failed to bind");

    info!("Server ready");

    // Tunnel: spawn client if configured
    let _tunnel_client_task = if let Some(ref tc) = tunnel_config {
        if tc.url.is_some() && !tc.relay {
            info!(
                "Tunnel client mode enabled, will connect to {}",
                tc.url.as_deref().unwrap()
            );
            Some(tunnel::client::spawn(state.clone(), tc.clone()))
        } else {
            None
        }
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

    state.session_manager.kill_all().await;
    info!("Goodbye");
}
