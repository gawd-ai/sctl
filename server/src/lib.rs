//! sctl library — exposes core modules for use by the server binary and integrations.
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

/// Full version: `<cargo-version>.<git-commit-count>`. Bumps on every commit
/// so a fresh binary is never mistaken for an older one.
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), ".", env!("SCTL_BUILD_NUMBER"));

pub mod activity;
pub mod at;
pub mod atomic;
pub mod auth;
pub mod comms;
pub mod config;
pub mod error;
pub mod gawdxfer;
pub mod infra;
pub mod lte_watchdog;
pub mod pin_store;
pub mod platform;
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
