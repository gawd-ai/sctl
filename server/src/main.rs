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
//! | Method | Path              | Auth | Description                          |
//! |--------|-------------------|------|--------------------------------------|
//! | GET    | `/api/health`     | No   | Liveness probe                       |
//! | GET    | `/api/info`       | Yes  | System info (IPs, CPU, mem, disk)    |
//! | POST   | `/api/exec`       | Yes  | One-shot command execution           |
//! | POST   | `/api/exec/batch` | Yes  | Batch command execution              |
//! | GET    | `/api/files`      | Yes  | Read file or list directory          |
//! | PUT    | `/api/files`      | Yes  | Write file (atomic)                  |
//! | GET    | `/api/ws`         | Yes* | WebSocket for interactive sessions   |
//!
//! *WebSocket auth is via `?token=<key>` query param (no `Authorization` header
//! available during the upgrade handshake).
//!
//! ### Tunnel endpoints (when `tunnel.relay = true`)
//!
//! | Method | Path                          | Auth       | Description                 |
//! |--------|-------------------------------|------------|-----------------------------|
//! | GET    | `/api/tunnel/register`        | `tunnel_key` | Device WS registration      |
//! | GET    | `/api/tunnel/devices`         | `tunnel_key` | List connected devices      |
//! | GET    | `/d/{serial}/api/health`      | No           | Proxied device health       |
//! | GET    | `/d/{serial}/api/info`        | `api_key`    | Proxied device info         |
//! | POST   | `/d/{serial}/api/exec`        | `api_key`    | Proxied command execution   |
//! | POST   | `/d/{serial}/api/exec/batch`  | `api_key`    | Proxied batch execution     |
//! | GET    | `/d/{serial}/api/files`       | `api_key`    | Proxied file read/list      |
//! | PUT    | `/d/{serial}/api/files`       | `api_key`    | Proxied file write          |
//! | GET    | `/d/{serial}/api/ws`          | `api_key`    | Proxied WS sessions         |
//!
//! ## Architecture
//!
//! ```text
//! main.rs          — entry point, clap subcommands, router setup, graceful shutdown
//! supervisor.rs    — built-in supervisor (fork/restart loop)
//! auth.rs          — Bearer token middleware, constant-time comparison
//! config.rs        — TOML + env-var configuration
//! routes/
//!   health.rs      — GET /api/health
//!   info.rs        — GET /api/info (system introspection)
//!   exec.rs        — POST /api/exec, POST /api/exec/batch
//!   files.rs       — GET/PUT /api/files (read, write, list)
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

mod auth;
mod config;
mod routes;
mod sessions;
mod shell;
mod supervisor;
mod tunnel;
mod util;
mod ws;

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;
use std::time::Instant;

use axum::{
    middleware,
    routing::{get, post},
    Extension, Router,
};
use clap::{Parser, Subcommand};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tower_http::trace::TraceLayer;
use tracing::{info, warn};

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
    /// Whether the tunnel client is currently connected to the relay.
    pub tunnel_connected: Arc<AtomicBool>,
    /// Number of tunnel reconnects since startup.
    pub tunnel_reconnects: Arc<AtomicU64>,
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

    let state = AppState {
        session_manager,
        config: Arc::new(config),
        start_time: Instant::now(),
        session_events,
        tunnel_connected: Arc::new(AtomicBool::new(false)),
        tunnel_reconnects: Arc::new(AtomicU64::new(0)),
    };

    // Build router
    let public_routes = Router::new().route("/api/health", get(routes::health::health));

    let authed_routes = Router::new()
        .route("/api/info", get(routes::info::info))
        .route("/api/exec", post(routes::exec::exec))
        .route("/api/exec/batch", post(routes::exec::batch_exec))
        .route(
            "/api/files",
            get(routes::files::get_file).put(routes::files::put_file),
        )
        .layer(middleware::from_fn(auth::require_api_key));

    let ws_route = Router::new().route("/api/ws", get(ws::ws_upgrade));

    let mut app = Router::new()
        .merge(public_routes)
        .merge(authed_routes)
        .merge(ws_route)
        .layer(Extension(ApiKey(state.config.auth.api_key.clone())))
        .layer(TraceLayer::new_for_http())
        .with_state(state.clone());

    // Tunnel: add relay routes if configured
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

    // Periodic sweep: clean up sessions whose process has exited
    let mgr = state.session_manager.clone();
    let sweep_tx = state.session_events.clone();
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
        }
    });

    // Tunnel relay: periodic sweep to evict dead devices
    let relay_sweep_task = relay_state_opt.clone().map(|rs| {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
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
