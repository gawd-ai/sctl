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
//! max_file_size = 52428800  # 50 MB
//! max_concurrent_transfers = 4
//! transfer_chunk_size = 262144  # 256 KiB
//! transfer_max_file_size = 1073741824  # 1 GiB
//! transfer_stale_timeout_secs = 3600
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
//! reconnect_delay_secs = 2                 # client mode, initial backoff
//! reconnect_max_delay_secs = 30            # client mode, max backoff
//! heartbeat_interval_secs = 15             # client mode, ping interval
//! bind_address = "wwan0"                   # client mode, interface name or IP
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
    /// Optional GPS configuration for Quectel modem GNSS tracking.
    pub gps: Option<GpsConfig>,
    /// Optional LTE signal monitoring for Quectel modem.
    pub lte: Option<LteConfig>,
}

/// HTTP server and resource-limit settings.
#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    /// Socket address to bind (default `0.0.0.0:1337`).
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Maximum concurrent TCP connections (default 10). Enforced via tower `ConcurrencyLimitLayer`.
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
    /// Directory containing playbook markdown files (default `/etc/sctl/playbooks`).
    #[serde(default = "default_playbooks_dir")]
    pub playbooks_dir: String,
    /// Maximum entries in the in-memory activity log ring buffer (default 200).
    #[serde(default = "default_activity_log_max_entries")]
    pub activity_log_max_entries: usize,
    /// Maximum cached exec results kept in memory (default 100).
    #[serde(default = "default_exec_result_cache_size")]
    pub exec_result_cache_size: usize,
    /// Default terminal rows for PTY sessions (default 24).
    #[serde(default = "default_terminal_rows")]
    pub default_terminal_rows: u16,
    /// Default terminal columns for PTY sessions (default 80).
    #[serde(default = "default_terminal_cols")]
    pub default_terminal_cols: u16,
    /// Max concurrent gawdxfer transfers (default 4).
    #[serde(default = "default_max_concurrent_transfers")]
    pub max_concurrent_transfers: usize,
    /// Chunk size in bytes for gawdxfer (default 256 KiB).
    #[serde(default = "default_transfer_chunk_size")]
    pub transfer_chunk_size: u32,
    /// Max file size for gawdxfer transfers in bytes (default 1 GiB).
    #[serde(default = "default_transfer_max_file_size")]
    pub transfer_max_file_size: u64,
    /// Stale transfer timeout in seconds (default 3600).
    #[serde(default = "default_transfer_stale_timeout")]
    pub transfer_stale_timeout_secs: u64,
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
    /// Seconds between reconnect attempts (client mode, default 2).
    #[serde(default = "default_reconnect_delay")]
    pub reconnect_delay_secs: u64,
    /// Max seconds between reconnect attempts (client mode, default 30).
    #[serde(default = "default_reconnect_max_delay")]
    pub reconnect_max_delay_secs: u64,
    /// Seconds between heartbeat pings (client mode, default 15).
    #[serde(default = "default_heartbeat_interval")]
    pub heartbeat_interval_secs: u64,
    /// Seconds before a device is considered dead if no heartbeat (relay mode, default 45).
    #[serde(default = "default_heartbeat_timeout")]
    pub heartbeat_timeout_secs: u64,
    /// Default proxy request timeout in seconds (relay mode, default 60).
    #[serde(default = "default_tunnel_proxy_timeout")]
    pub tunnel_proxy_timeout_secs: u64,
    /// Local address or interface name to bind outbound tunnel connections to
    /// (client mode). Forces traffic over a specific interface.
    /// Accepts either an IP (`"10.180.41.231"`) or interface name (`"wwan0"`).
    /// Interface names are resolved to their current IPv4 on each connect
    /// attempt, surviving DHCP/carrier IP changes across reboots.
    pub bind_address: Option<String>,
}

/// GPS configuration for Quectel modem GNSS tracking.
///
/// When present, sctl periodically polls the modem for GPS fixes via AT commands
/// and exposes location data through `/api/gps` and the health endpoint.
///
/// ```toml
/// [gps]
/// device = "/dev/ttyUSB2"
/// poll_interval_secs = 30
/// history_size = 100
/// auto_enable = true
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct GpsConfig {
    /// Serial device for AT commands (default `/dev/ttyUSB2`).
    #[serde(default = "default_gps_device")]
    pub device: String,
    /// Seconds between GPS polls (default 30).
    #[serde(default = "default_gps_poll_interval")]
    pub poll_interval_secs: u64,
    /// Maximum GPS fix history entries (default 100).
    #[serde(default = "default_gps_history_size")]
    pub history_size: usize,
    /// Auto-enable GNSS engine on startup (default true).
    #[serde(default = "default_gps_auto_enable")]
    pub auto_enable: bool,
}

/// LTE signal monitoring for Quectel modem.
///
/// When present, sctl periodically polls the modem for signal quality via AT
/// commands and exposes the data through `/api/info`.
///
/// ```toml
/// [lte]
/// device = "/dev/ttyUSB2"
/// poll_interval_secs = 60
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct LteConfig {
    /// Serial device for AT commands (default `/dev/ttyUSB2`).
    #[serde(default = "default_lte_device")]
    pub device: String,
    /// Seconds between LTE signal polls (default 60).
    #[serde(default = "default_lte_poll_interval")]
    pub poll_interval_secs: u64,
}

fn default_lte_device() -> String {
    "/dev/ttyUSB2".to_string()
}
fn default_lte_poll_interval() -> u64 {
    60
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
    50 * 1024 * 1024 // 50 MB
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
fn default_activity_log_max_entries() -> usize {
    200
}
fn default_exec_result_cache_size() -> usize {
    100
}
fn default_terminal_rows() -> u16 {
    24
}
fn default_terminal_cols() -> u16 {
    80
}
fn default_playbooks_dir() -> String {
    "/etc/sctl/playbooks".to_string()
}
fn default_supervisor_max_backoff() -> u64 {
    60
}
fn default_supervisor_stable_threshold() -> u64 {
    60
}
fn default_max_concurrent_transfers() -> usize {
    4
}
fn default_transfer_chunk_size() -> u32 {
    256 * 1024 // 256 KiB
}
fn default_transfer_max_file_size() -> u64 {
    1024 * 1024 * 1024 // 1 GiB
}
fn default_transfer_stale_timeout() -> u64 {
    3600 // 1 hour
}
fn default_gps_device() -> String {
    "/dev/ttyUSB2".to_string()
}
fn default_gps_poll_interval() -> u64 {
    30
}
fn default_gps_history_size() -> usize {
    100
}
fn default_gps_auto_enable() -> bool {
    true
}
fn default_reconnect_delay() -> u64 {
    2
}
fn default_reconnect_max_delay() -> u64 {
    30
}
fn default_heartbeat_interval() -> u64 {
    15
}
fn default_heartbeat_timeout() -> u64 {
    45
}
fn default_tunnel_proxy_timeout() -> u64 {
    60
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
            activity_log_max_entries: default_activity_log_max_entries(),
            exec_result_cache_size: default_exec_result_cache_size(),
            default_terminal_rows: default_terminal_rows(),
            default_terminal_cols: default_terminal_cols(),
            playbooks_dir: default_playbooks_dir(),
            max_concurrent_transfers: default_max_concurrent_transfers(),
            transfer_chunk_size: default_transfer_chunk_size(),
            transfer_max_file_size: default_transfer_max_file_size(),
            transfer_stale_timeout_secs: default_transfer_stale_timeout(),
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
    /// Validate configuration values after loading. Returns a list of errors.
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();

        // Validate listen address parses as SocketAddr
        if self.server.listen.parse::<std::net::SocketAddr>().is_err() {
            errors.push(format!(
                "server.listen '{}' is not a valid socket address",
                self.server.listen
            ));
        }

        if !(1..=500).contains(&self.server.default_terminal_rows) {
            errors.push(format!(
                "server.default_terminal_rows {} out of range [1, 500]",
                self.server.default_terminal_rows
            ));
        }
        if !(1..=500).contains(&self.server.default_terminal_cols) {
            errors.push(format!(
                "server.default_terminal_cols {} out of range [1, 500]",
                self.server.default_terminal_cols
            ));
        }

        if self.server.max_sessions > 10_000 {
            errors.push(format!(
                "server.max_sessions {} exceeds limit of 10000",
                self.server.max_sessions
            ));
        }

        if self.server.max_file_size < 1024 {
            errors.push(format!(
                "server.max_file_size {} is too small (min 1024)",
                self.server.max_file_size
            ));
        }

        if self.server.transfer_chunk_size < 1024 {
            errors.push(format!(
                "server.transfer_chunk_size {} is too small (min 1024)",
                self.server.transfer_chunk_size
            ));
        }

        if self.server.max_concurrent_transfers < 1 {
            errors.push("server.max_concurrent_transfers must be >= 1".to_string());
        }

        if let Some(ref tc) = self.tunnel {
            if !tc.relay {
                if let Some(ref url) = tc.url {
                    if !url.starts_with("ws://") && !url.starts_with("wss://") {
                        errors.push(format!(
                            "tunnel.url '{url}' must start with ws:// or wss://"
                        ));
                    }
                }
            }
            if tc.relay && tc.tunnel_key.len() < 8 {
                errors.push(format!(
                    "tunnel.tunnel_key length {} is too short (min 8)",
                    tc.tunnel_key.len()
                ));
            }
        }

        errors
    }

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
                gps: None,
                lte: None,
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
        if let Ok(dir) = std::env::var("SCTL_DATA_DIR") {
            config.server.data_dir = dir;
        }
        if let Ok(dir) = std::env::var("SCTL_PLAYBOOKS_DIR") {
            config.server.playbooks_dir = dir;
        }

        config
    }
}
