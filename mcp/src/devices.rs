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

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::client::SctlClient;
use crate::config::ResolvedConfig;
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
    pub async fn get_or_connect(
        &self,
        name: &str,
        client: &SctlClient,
    ) -> Result<Arc<DeviceWsConnection>, String> {
        let mut conns = self.connections.lock().await;
        if let Some(conn) = conns.get(name) {
            return Ok(Arc::clone(conn));
        }

        let conn = DeviceWsConnection::connect(client.base_url(), client.api_key()).await?;
        let conn = Arc::new(conn);
        conns.insert(name.to_string(), Arc::clone(&conn));
        Ok(conn)
    }
}

/// Registry of configured sctl devices.
///
/// Holds both HTTP clients (for REST endpoints) and a [`WsPool`] (for
/// streaming session connections). Created once at startup from the resolved
/// configuration.
pub struct DeviceRegistry {
    clients: HashMap<String, SctlClient>,
    default_device: String,
    /// Lazy WebSocket connection pool for session tools.
    pub ws_pool: WsPool,
    /// Maps session IDs to their owning device name.
    /// Populated by `session_list` and `session_start` so that subsequent
    /// session tools can auto-route without an explicit `device` parameter.
    session_device_map: Mutex<HashMap<String, String>>,
}

impl DeviceRegistry {
    /// Build a registry from resolved configuration.
    pub fn from_config(config: ResolvedConfig) -> Self {
        let clients = config
            .devices
            .into_iter()
            .map(|(name, entry)| {
                let client = SctlClient::new(entry.url, entry.api_key);
                (name, client)
            })
            .collect();

        Self {
            clients,
            default_device: config.default_device,
            ws_pool: WsPool::new(),
            session_device_map: Mutex::new(HashMap::new()),
        }
    }

    /// Look up a device's HTTP client by name (defaults to the configured default).
    pub fn resolve(&self, device: Option<&str>) -> Result<&SctlClient, String> {
        let name = device.unwrap_or(&self.default_device);
        self.clients
            .get(name)
            .ok_or_else(|| format!("Unknown device: '{}'", name))
    }

    /// Resolve and return both the device name and the client.
    pub fn resolve_with_name(&self, device: Option<&str>) -> Result<(&str, &SctlClient), String> {
        let name = device.unwrap_or(&self.default_device);
        self.clients
            .get_key_value(name)
            .map(|(k, v)| (k.as_str(), v))
            .ok_or_else(|| format!("Unknown device: '{name}'"))
    }

    /// List all configured devices, sorted by name.
    pub fn list(&self) -> Vec<DeviceInfo> {
        let mut devices: Vec<DeviceInfo> = self
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
    pub fn default_device(&self) -> &str {
        &self.default_device
    }

    /// Access all HTTP clients (keyed by device name).
    pub fn clients(&self) -> &HashMap<String, SctlClient> {
        &self.clients
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
        self.session_device_map.lock().await.get(session_id).cloned()
    }
}
