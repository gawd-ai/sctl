//! Configuration loading and defaults.
//!
//! Configuration is resolved in order of precedence (highest wins):
//!
//! 1. **Environment variables** — `SCTL_API_KEY`, `SCTL_LISTEN`,
//!    `SCTL_DEVICE_SERIAL`
//! 2. **Config file** — path via `--config <path>`, or `sctl.toml` in CWD
//! 3. **Compiled defaults** — see each field's default value below
//!
//! The TOML file mirrors the struct hierarchy:
//!
//! ```toml
//! [server]
//! listen = "0.0.0.0:1337"
//! max_sessions = 20
//! exec_timeout_ms = 30000
//! max_batch_size = 20
//! max_file_size = 2097152  # 2 MB
//!
//! [auth]
//! api_key = "your-secret-key"
//!
//! [shell]
//! default_shell = "/bin/sh"
//! default_working_dir = "/"
//!
//! [device]
//! serial = "SCTL-0001-DEV-001"
//!
//! [logging]
//! level = "info"
//!
//! # Optional — omit entirely to disable tunnel
//! [tunnel]
//! relay = false                            # true = relay mode, false = client mode
//! tunnel_key = "shared-secret"             # device<->relay auth
//! url = "wss://relay.example.com/api/tunnel/register"  # client mode only
//! reconnect_delay_secs = 5                 # client mode, initial backoff
//! reconnect_max_delay_secs = 60            # client mode, max backoff
//! heartbeat_interval_secs = 30             # client mode, ping interval
//! ```

use serde::Deserialize;
use std::path::Path;

/// Top-level configuration, deserialized from TOML.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub shell: ShellConfig,
    #[serde(default)]
    pub device: DeviceConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub supervisor: SupervisorConfig,
    /// Optional tunnel configuration for relay or client mode.
    pub tunnel: Option<TunnelConfig>,
}

/// HTTP server and resource-limit settings.
#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    /// Socket address to bind (default `0.0.0.0:1337`).
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Maximum concurrent TCP connections (default 10). **Not currently enforced.**
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,
    /// Maximum concurrent WebSocket shell sessions (default 20).
    #[serde(default = "default_max_sessions")]
    pub max_sessions: usize,
    /// Default timeout for `POST /api/exec` in milliseconds (default 30 000).
    #[serde(default = "default_exec_timeout_ms")]
    pub exec_timeout_ms: u64,
    /// Maximum commands per `POST /api/exec/batch` request (default 20).
    #[serde(default = "default_max_batch_size")]
    pub max_batch_size: usize,
    /// Maximum file size in bytes for `/api/files` read/write (default 2 MB).
    #[serde(default = "default_max_file_size")]
    pub max_file_size: usize,
    /// Maximum output entries kept per session buffer (default 1000).
    #[serde(default = "default_session_buffer_size")]
    pub session_buffer_size: usize,
    /// Directory for persistent data (journals, etc). Default `/var/lib/sctl`.
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
    /// Enable output journaling to disk (default true).
    #[serde(default = "default_journal_enabled")]
    pub journal_enabled: bool,
    /// Batch fsync interval in milliseconds (0 = every write). Default 5000.
    #[serde(default = "default_journal_fsync_interval_ms")]
    pub journal_fsync_interval_ms: u64,
    /// Auto-delete journals older than this many hours (default 72).
    #[serde(default = "default_journal_max_age_hours")]
    pub journal_max_age_hours: u64,
    /// Default terminal rows for PTY sessions (default 24).
    #[serde(default = "default_terminal_rows")]
    pub default_terminal_rows: u16,
    /// Default terminal columns for PTY sessions (default 80).
    #[serde(default = "default_terminal_cols")]
    pub default_terminal_cols: u16,
}

/// Supervisor settings for `sctl supervise`.
#[derive(Debug, Clone, Deserialize)]
pub struct SupervisorConfig {
    /// Maximum seconds between restart attempts (default 60).
    #[serde(default = "default_supervisor_max_backoff")]
    pub max_backoff: u64,
    /// Seconds of uptime before resetting backoff (default 60).
    #[serde(default = "default_supervisor_stable_threshold")]
    pub stable_threshold: u64,
}

/// Authentication settings.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthConfig {
    /// Pre-shared Bearer token. Override with `SCTL_API_KEY` env var.
    /// Defaults to `"change-me"` which triggers a startup warning.
    #[serde(default = "default_api_key")]
    pub api_key: String,
}

/// Shell defaults used when requests don't specify overrides.
#[derive(Debug, Clone, Deserialize)]
pub struct ShellConfig {
    /// Shell binary for exec and sessions (default `/bin/sh`).
    #[serde(default = "default_shell")]
    pub default_shell: String,
    /// Working directory for exec and sessions (default `/`).
    #[serde(default = "default_working_dir")]
    pub default_working_dir: String,
}

/// Device identity, embedded in `/api/info` responses.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceConfig {
    /// Unique device serial number. Override with `SCTL_DEVICE_SERIAL`.
    #[serde(default = "default_serial")]
    pub serial: String,
}

/// Logging configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct LoggingConfig {
    /// tracing filter level (default `info`). Overridden by `RUST_LOG` env var.
    #[serde(default = "default_log_level")]
    pub level: String,
}

/// Tunnel configuration — enables relay mode or client (outbound) mode.
///
/// - **Relay mode** (`relay = true`): this instance acts as a relay server.
///   Devices connect inbound, clients connect to `/d/{serial}/api/*`.
/// - **Client mode** (`url` is set): this instance connects outbound to a relay.
/// - If neither is set, tunnel is disabled (default behavior).
#[derive(Debug, Clone, Deserialize)]
pub struct TunnelConfig {
    /// Run as a tunnel relay (default false).
    #[serde(default)]
    pub relay: bool,
    /// Shared secret for device<->relay authentication.
    pub tunnel_key: String,
    /// Relay URL for client mode (e.g. `wss://relay.example.com/api/tunnel/register`).
    pub url: Option<String>,
    /// Seconds between reconnect attempts (client mode, default 5).
    #[serde(default = "default_reconnect_delay")]
    pub reconnect_delay_secs: u64,
    /// Max seconds between reconnect attempts (client mode, default 60).
    #[serde(default = "default_reconnect_max_delay")]
    pub reconnect_max_delay_secs: u64,
    /// Seconds between heartbeat pings (client mode, default 30).
    #[serde(default = "default_heartbeat_interval")]
    pub heartbeat_interval_secs: u64,
}

fn default_listen() -> String {
    "0.0.0.0:1337".to_string()
}
fn default_max_connections() -> usize {
    10
}
fn default_max_sessions() -> usize {
    20
}
fn default_exec_timeout_ms() -> u64 {
    30000
}
fn default_max_batch_size() -> usize {
    20
}
fn default_max_file_size() -> usize {
    2 * 1024 * 1024 // 2 MB
}
fn default_session_buffer_size() -> usize {
    1000
}
fn default_api_key() -> String {
    "change-me".to_string()
}
fn default_shell() -> String {
    "/bin/sh".to_string()
}
fn default_working_dir() -> String {
    "/".to_string()
}
fn default_serial() -> String {
    "SCTL-0000-DEV-001".to_string()
}
fn default_log_level() -> String {
    "info".to_string()
}
fn default_data_dir() -> String {
    "/var/lib/sctl".to_string()
}
fn default_journal_enabled() -> bool {
    true
}
fn default_journal_fsync_interval_ms() -> u64 {
    5000
}
fn default_journal_max_age_hours() -> u64 {
    72
}
fn default_terminal_rows() -> u16 {
    24
}
fn default_terminal_cols() -> u16 {
    80
}
fn default_supervisor_max_backoff() -> u64 {
    60
}
fn default_supervisor_stable_threshold() -> u64 {
    60
}
fn default_reconnect_delay() -> u64 {
    5
}
fn default_reconnect_max_delay() -> u64 {
    60
}
fn default_heartbeat_interval() -> u64 {
    30
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            max_connections: default_max_connections(),
            max_sessions: default_max_sessions(),
            exec_timeout_ms: default_exec_timeout_ms(),
            max_batch_size: default_max_batch_size(),
            max_file_size: default_max_file_size(),
            session_buffer_size: default_session_buffer_size(),
            data_dir: default_data_dir(),
            journal_enabled: default_journal_enabled(),
            journal_fsync_interval_ms: default_journal_fsync_interval_ms(),
            journal_max_age_hours: default_journal_max_age_hours(),
            default_terminal_rows: default_terminal_rows(),
            default_terminal_cols: default_terminal_cols(),
        }
    }
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            max_backoff: default_supervisor_max_backoff(),
            stable_threshold: default_supervisor_stable_threshold(),
        }
    }
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            api_key: default_api_key(),
        }
    }
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            default_shell: default_shell(),
            default_working_dir: default_working_dir(),
        }
    }
}

impl Default for DeviceConfig {
    fn default() -> Self {
        Self {
            serial: default_serial(),
        }
    }
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
        }
    }
}

impl Config {
    /// Load configuration with the precedence chain: env vars > file > defaults.
    ///
    /// If `path` is `Some`, reads that file (panics on failure). Otherwise looks
    /// for `sctl.toml` in the current directory, falling back to compiled defaults.
    pub fn load(path: Option<&str>) -> Self {
        let mut config = if let Some(p) = path {
            let content = std::fs::read_to_string(p)
                .unwrap_or_else(|e| panic!("Failed to read config file {p}: {e}"));
            toml::from_str(&content)
                .unwrap_or_else(|e| panic!("Failed to parse config file {p}: {e}"))
        } else if Path::new("sctl.toml").exists() {
            let content = std::fs::read_to_string("sctl.toml").expect("Failed to read sctl.toml");
            toml::from_str(&content).expect("Failed to parse sctl.toml")
        } else {
            Config {
                server: ServerConfig::default(),
                auth: AuthConfig::default(),
                shell: ShellConfig::default(),
                device: DeviceConfig::default(),
                logging: LoggingConfig::default(),
                supervisor: SupervisorConfig::default(),
                tunnel: None,
            }
        };

        // Env var overrides
        if let Ok(key) = std::env::var("SCTL_API_KEY") {
            config.auth.api_key = key;
        }
        if let Ok(listen) = std::env::var("SCTL_LISTEN") {
            config.server.listen = listen;
        }
        if let Ok(serial) = std::env::var("SCTL_DEVICE_SERIAL") {
            config.device.serial = serial;
        }

        config
    }
}
