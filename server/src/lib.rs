#![deny(clippy::all)]
#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
// Pedantic guard against any owned-return going unused. ~50 candidates
// triggered when re-enabled; almost none represent real bug-bait
// (every internal caller uses the return), so the annotation pressure
// outweighs the diagnostic value. Re-enable when a real "ignored result"
// bug shows up.
#![allow(clippy::must_use_candidate)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::unused_async)]
// Suggests adding `S: BuildHasher` generics to functions taking HashMap
// params. The four hits live in axum extractor signatures + shell helpers
// where the public API is keyed on `HashMap<String, String>`; generalizing
// would propagate the parameter through callers without any practical
// benefit (no consumer uses an alternative hasher).
#![allow(clippy::implicit_hasher)]

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

/// Full version: `<cargo-version>.<git-commit-count>`. Bumps on every commit
/// so a fresh binary is never mistaken for an older one.
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), ".", env!("SCTL_BUILD_NUMBER"));

pub mod activity;
pub mod auth;
pub mod config;
pub mod error;
pub mod gawdxfer;
pub mod gps;
pub mod infra;
pub mod lte;
pub mod lte_watchdog;
pub mod modem;
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
