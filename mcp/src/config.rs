//! Configuration loading for mcp-sctl.
//!
//! Configuration is resolved from three fallback sources (tried in order):
//!
//! 1. **JSON file** via `--config <path>` CLI flag
//! 2. **JSON file** via `SCTL_CONFIG` environment variable
//! 3. **Environment variables** — `SCTL_URL` + `SCTL_API_KEY` (creates
//!    a single "default" device)
//!
//! The JSON config format supports multiple named devices with per-device URLs
//! and API keys. See `devices.example.json` for an example.

use clap::Parser;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

/// Bump this when the config format changes (new required fields, renamed keys, etc.).
/// mcp-sctl will warn if the on-disk version is older, so users know to update.
pub const CONFIG_VERSION: u32 = 2;

/// CLI arguments parsed by `clap`.
#[derive(Parser)]
#[command(name = "mcp-sctl", about = "MCP server for sctl devices")]
pub struct Cli {
    /// Path to devices config file (JSON)
    #[arg(long)]
    pub config: Option<PathBuf>,
}

/// Raw JSON config file structure.
#[derive(Deserialize)]
pub struct DevicesConfig {
    /// Config format version — checked against `CONFIG_VERSION` to detect stale files.
    pub config_version: Option<u32>,
    pub devices: HashMap<String, DeviceEntry>,
    pub default_device: Option<String>,
}

/// A single device entry in the config file.
#[derive(Deserialize, Clone)]
pub struct DeviceEntry {
    pub url: String,
    pub api_key: String,
    /// Directory on the device where playbook `.md` files are stored.
    /// Defaults to `/etc/sctl/playbooks` if omitted.
    pub playbooks_dir: Option<String>,
}

/// Validated configuration ready for use by the device registry.
pub struct ResolvedConfig {
    pub devices: HashMap<String, DeviceEntry>,
    pub default_device: String,
}

/// Load and validate configuration from CLI args, env vars, or config file.
pub fn load_config(cli: &Cli) -> Result<ResolvedConfig, String> {
    if let Some(path) = &cli.config {
        load_from_file(&expand_tilde(path))
    } else if let Ok(path) = std::env::var("SCTL_CONFIG") {
        load_from_file(&expand_tilde(&PathBuf::from(path)))
    } else {
        load_from_env()
    }
}

/// Expand a leading `~` to `$HOME`.
fn expand_tilde(path: &PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    path.clone()
}

fn load_from_file(path: &PathBuf) -> Result<ResolvedConfig, String> {
    let contents = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read config file {}: {}", path.display(), e))?;

    let config: DevicesConfig = serde_json::from_str(&contents)
        .map_err(|e| format!("Failed to parse config file {}: {}", path.display(), e))?;

    // Warn about missing or outdated config version
    match config.config_version {
        None => eprintln!(
            "Warning: {} has no config_version field (expected {}). \
             Config may be outdated — check devices.example.json for the current format.",
            path.display(),
            CONFIG_VERSION
        ),
        Some(v) if v < CONFIG_VERSION => eprintln!(
            "Warning: {} has config_version {} but mcp-sctl expects {}. \
             Config may be outdated — check devices.example.json for the current format.",
            path.display(),
            v,
            CONFIG_VERSION
        ),
        _ => {}
    }

    if config.devices.is_empty() {
        return Err("Config file contains no devices".into());
    }

    for (name, entry) in &config.devices {
        if entry.url.is_empty() {
            return Err(format!("Device '{}' has empty url", name));
        }
        if entry.api_key.is_empty() {
            return Err(format!("Device '{}' has empty api_key", name));
        }
    }

    let default_device = if let Some(d) = &config.default_device {
        if !config.devices.contains_key(d) {
            return Err(format!("default_device '{}' not found in devices", d));
        }
        d.clone()
    } else if config.devices.len() == 1 {
        config.devices.keys().next().unwrap().clone()
    } else {
        return Err("Multiple devices configured but no default_device specified".into());
    };

    Ok(ResolvedConfig {
        devices: config.devices,
        default_device,
    })
}

fn load_from_env() -> Result<ResolvedConfig, String> {
    let url = std::env::var("SCTL_URL").map_err(|_| "No config file and SCTL_URL not set")?;
    let api_key =
        std::env::var("SCTL_API_KEY").map_err(|_| "No config file and SCTL_API_KEY not set")?;

    if url.is_empty() {
        return Err("SCTL_URL is empty".into());
    }
    if api_key.is_empty() {
        return Err("SCTL_API_KEY is empty".into());
    }

    let mut devices = HashMap::new();
    devices.insert(
        "default".to_string(),
        DeviceEntry {
            url,
            api_key,
            playbooks_dir: None,
        },
    );

    Ok(ResolvedConfig {
        devices,
        default_device: "default".to_string(),
    })
}
