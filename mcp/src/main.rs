//! # mcp-sctl
//!
//! MCP (Model Context Protocol) server that proxies tool calls to one or more
//! sctl devices. Runs as a stdio JSON-RPC server — designed to be launched
//! by an AI agent host (e.g. Claude Code).
//!
//! ## Architecture
//!
//! ```text
//! main.rs              — entry point, config loading, MCP server launch
//! config.rs            — JSON file / env-var configuration loading
//! client.rs            — HTTP client for sctl REST endpoints
//! devices.rs           — device registry with WebSocket connection pool
//! mcp.rs               — MCP JSON-RPC protocol handler (stdio)
//! tools.rs             — tool definitions and handlers
//! websocket.rs         — WebSocket client with auto-reconnect and local buffers
//! playbooks.rs         — playbook model, parsing, rendering (pure data)
//! playbook_registry.rs — per-device playbook cache with lazy fetch
//! ```
//!
//! ## Tools
//!
//! - **Device tools** (HTTP): `device_list`, `device_health`, `device_info`,
//!   `device_exec`, `device_exec_batch`, `device_file_read`, `device_file_write`
//! - **Session tools** (WebSocket): `session_start`, `session_exec`,
//!   `session_send`, `session_read`, `session_signal`, `session_kill`
//! - **Playbook management**: `playbook_list`, `playbook_get`, `playbook_put`
//! - **Dynamic playbook tools** (`pb_*`): one per playbook discovered on devices

mod client;
mod config;
mod devices;
mod mcp;
mod playbook_registry;
mod playbooks;
mod tools;
mod websocket;

use std::collections::HashMap;

use clap::Parser;
use config::Cli;
use devices::DeviceRegistry;
use playbook_registry::PlaybookRegistry;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = Cli::parse();

    let resolved = match config::load_config(&cli) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("mcp-sctl: configuration error: {}", e);
            std::process::exit(1);
        }
    };

    let device_count = resolved.devices.len();
    let default = resolved.default_device.clone();

    // Extract per-device playbook directory overrides before consuming the config
    let device_dirs: HashMap<String, String> = resolved
        .devices
        .iter()
        .filter_map(|(name, entry)| {
            entry
                .playbooks_dir
                .as_ref()
                .map(|dir| (name.clone(), dir.clone()))
        })
        .collect();

    let registry = DeviceRegistry::from_config(resolved);
    let pb_registry = PlaybookRegistry::with_defaults(device_dirs);

    eprintln!(
        "mcp-sctl: {} device(s) configured, default={}",
        device_count, default
    );

    mcp::run_stdio(registry, pb_registry).await;
}
