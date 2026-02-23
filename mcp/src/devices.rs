//! Device registry and WebSocket connection pool.
//!
//! [`DeviceRegistry`] maps device names to HTTP clients and provides a
//! [`WsPool`] for lazy WebSocket connections used by the session tools.
//!
//! ## Device resolution
//!
//! All tool handlers call [`DeviceRegistry::resolve`] (or [`DeviceRegistry::resolve_with_name`])
//! with an optional device name. If omitted, the configured default device is
//! used. This allows single-device setups to work without specifying a device
//! on every call.
//!
//! ## Config hot-reload
//!
//! When created with a config file path, the registry checks the file's mtime
//! before each resolve. If the file has changed, the device list is reloaded
//! automatically. WebSocket connections for removed/changed devices are dropped.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use tokio::sync::{Mutex, RwLock};

use crate::client::SctlClient;
use crate::config::{self, DeviceEntry, ResolvedConfig};
use crate::websocket::DeviceWsConnection;

/// Summary info for a configured device.
pub struct DeviceInfo {
    pub name: String,
    pub url: String,
}

/// Pool of lazy WebSocket connections to devices.
///
/// Connections are created on first use and reused for subsequent calls.
/// Each device gets at most one WebSocket connection.
pub struct WsPool {
    connections: Mutex<HashMap<String, Arc<DeviceWsConnection>>>,
}

impl WsPool {
    fn new() -> Self {
        Self {
            connections: Mutex::new(HashMap::new()),
        }
    }

    /// Get an existing WS connection or create a new one for the device.
    ///
    /// If the cached connection is disconnected (e.g. after device reboot),
    /// it is dropped and a fresh connection is established.
    pub async fn get_or_connect(
        &self,
        name: &str,
        client: &SctlClient,
    ) -> Result<Arc<DeviceWsConnection>, String> {
        let mut conns = self.connections.lock().await;
        if let Some(conn) = conns.get(name) {
            if conn.is_connected() {
                return Ok(Arc::clone(conn));
            }
            // Connection is dead — drop it and reconnect below
            eprintln!("mcp-sctl: WS connection to '{name}' is dead, reconnecting");
            conns.remove(name);
        }

        let conn = DeviceWsConnection::connect(client.base_url(), client.api_key()).await?;
        let conn = Arc::new(conn);
        conns.insert(name.to_string(), Arc::clone(&conn));
        Ok(conn)
    }

    /// Remove a device's cached connection (e.g. after config reload changed its URL).
    async fn remove(&self, name: &str) {
        self.connections.lock().await.remove(name);
    }
}

/// Mutable inner state protected by RwLock.
struct RegistryInner {
    clients: HashMap<String, SctlClient>,
    default_device: String,
    /// Per-device playbook directory overrides, re-extracted on reload.
    playbook_dirs: HashMap<String, String>,
}

/// Registry of configured sctl devices.
///
/// Holds both HTTP clients (for REST endpoints) and a [`WsPool`] (for
/// streaming session connections). Supports config hot-reload when created
/// from a file.
pub struct DeviceRegistry {
    inner: RwLock<RegistryInner>,
    /// Lazy WebSocket connection pool for session tools.
    pub ws_pool: WsPool,
    /// Maps session IDs to their owning device name.
    session_device_map: Mutex<HashMap<String, String>>,
    /// Config file path (if loaded from file) for hot-reload.
    config_path: Option<PathBuf>,
    /// Last observed mtime of the config file.
    last_mtime: Mutex<Option<SystemTime>>,
}

impl DeviceRegistry {
    /// Build a registry from resolved configuration (no hot-reload).
    pub fn from_config(config: ResolvedConfig) -> Self {
        let playbook_dirs = extract_playbook_dirs(&config);
        let clients = build_clients(config.devices);

        Self {
            inner: RwLock::new(RegistryInner {
                clients,
                default_device: config.default_device,
                playbook_dirs,
            }),
            ws_pool: WsPool::new(),
            session_device_map: Mutex::new(HashMap::new()),
            config_path: None,
            last_mtime: Mutex::new(None),
        }
    }

    /// Build a registry from a config file with hot-reload support.
    pub fn from_config_file(config: ResolvedConfig, path: PathBuf) -> Self {
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let playbook_dirs = extract_playbook_dirs(&config);
        let clients = build_clients(config.devices);

        Self {
            inner: RwLock::new(RegistryInner {
                clients,
                default_device: config.default_device,
                playbook_dirs,
            }),
            ws_pool: WsPool::new(),
            session_device_map: Mutex::new(HashMap::new()),
            config_path: Some(path),
            last_mtime: Mutex::new(mtime),
        }
    }

    /// Check if the config file has changed and reload if so.
    /// Called automatically before device resolution.
    pub async fn maybe_reload(&self) {
        let path = match &self.config_path {
            Some(p) => p,
            None => return,
        };

        let current_mtime = match std::fs::metadata(path).and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(_) => return,
        };

        let mut last = self.last_mtime.lock().await;
        if *last == Some(current_mtime) {
            return;
        }

        // File changed — reload
        let new_config = match config::load_config_from_file(path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("mcp-sctl: config reload failed: {}", e);
                return;
            }
        };

        eprintln!("mcp-sctl: config file changed, reloading devices");

        let new_playbook_dirs = extract_playbook_dirs(&new_config);
        let new_clients = build_clients(new_config.devices);

        // Drop WS connections for devices whose URL changed or were removed
        {
            let inner = self.inner.read().await;
            for (name, old_client) in &inner.clients {
                match new_clients.get(name) {
                    Some(new_client) if new_client.base_url() == old_client.base_url() => {}
                    _ => {
                        self.ws_pool.remove(name).await;
                    }
                }
            }
        }

        // Swap in new state
        {
            let mut inner = self.inner.write().await;
            inner.clients = new_clients;
            inner.default_device = new_config.default_device;
            inner.playbook_dirs = new_playbook_dirs;
        }

        *last = Some(current_mtime);
    }

    /// Look up a device's HTTP client by name (defaults to the configured default).
    /// Checks for config changes before resolving.
    pub async fn resolve(&self, device: Option<&str>) -> Result<SctlClient, String> {
        self.maybe_reload().await;
        let inner = self.inner.read().await;
        let name = device.unwrap_or(&inner.default_device);
        inner
            .clients
            .get(name)
            .cloned()
            .ok_or_else(|| format!("Unknown device: '{}'", name))
    }

    /// Resolve and return both the device name and a cloned client.
    pub async fn resolve_with_name(
        &self,
        device: Option<&str>,
    ) -> Result<(String, SctlClient), String> {
        self.maybe_reload().await;
        let inner = self.inner.read().await;
        let name = device.unwrap_or(&inner.default_device);
        inner
            .clients
            .get(name)
            .map(|v| (name.to_string(), v.clone()))
            .ok_or_else(|| format!("Unknown device: '{name}'"))
    }

    /// List all configured devices, sorted by name.
    pub async fn list(&self) -> Vec<DeviceInfo> {
        self.maybe_reload().await;
        let inner = self.inner.read().await;
        let mut devices: Vec<DeviceInfo> = inner
            .clients
            .iter()
            .map(|(name, client)| DeviceInfo {
                name: name.clone(),
                url: client.base_url().to_string(),
            })
            .collect();
        devices.sort_by(|a, b| a.name.cmp(&b.name));
        devices
    }

    /// The name of the default device.
    pub async fn default_device(&self) -> String {
        let inner = self.inner.read().await;
        inner.default_device.clone()
    }

    /// Access all HTTP clients (keyed by device name).
    pub async fn clients(&self) -> HashMap<String, SctlClient> {
        let inner = self.inner.read().await;
        inner.clients.clone()
    }

    /// Record which device owns a session.
    pub async fn register_session(&self, session_id: &str, device: &str) {
        self.session_device_map
            .lock()
            .await
            .insert(session_id.to_string(), device.to_string());
    }

    /// Look up which device owns a session.
    pub async fn resolve_session_device(&self, session_id: &str) -> Option<String> {
        self.session_device_map
            .lock()
            .await
            .get(session_id)
            .cloned()
    }
}

fn extract_playbook_dirs(config: &ResolvedConfig) -> HashMap<String, String> {
    config
        .devices
        .iter()
        .filter_map(|(name, entry)| {
            entry
                .playbooks_dir
                .as_ref()
                .map(|dir| (name.clone(), dir.clone()))
        })
        .collect()
}

fn build_clients(devices: HashMap<String, DeviceEntry>) -> HashMap<String, SctlClient> {
    devices
        .into_iter()
        .map(|(name, entry)| {
            let client = SctlClient::new(entry.url, entry.api_key);
            (name, client)
        })
        .collect()
}
