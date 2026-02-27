#![deny(clippy::all)]
#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::unused_async)]
#![allow(clippy::implicit_hasher)]
#![allow(clippy::redundant_closure_for_method_calls)]

//! sctl library — exposes core modules for use by downstream crates (e.g. netage-server).
//!
//! This library re-exports the key building blocks:
//! - `tunnel` — relay and client for CGNAT device connectivity
//! - `auth` — API key authentication middleware
//! - `config` — configuration loading
//! - `sessions` — interactive shell session management
//! - `activity` — in-memory activity journal
//! - `routes` — REST API route handlers
//! - `ws` — WebSocket protocol handling
//! - `shell` — process spawning and PTY management
//! - `gawdxfer` — chunked file transfer

pub mod activity;
pub mod auth;
pub mod config;
pub mod gawdxfer;
pub mod gps;
pub mod lte;
pub mod modem;
pub mod routes;
pub mod sessions;
pub mod shell;
pub mod state;
pub mod tunnel;
pub mod util;
pub mod ws;

// Re-export key types at crate root for convenience.
pub use activity::{ActivityLog, ExecResultsCache};
pub use auth::ApiKey;
pub use config::Config;
pub use sessions::SessionManager;
pub use state::AppState;
pub use tunnel::relay::RelayState;
