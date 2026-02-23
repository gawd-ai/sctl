//! Per-device playbook cache with lazy fetch and invalidation.
//!
//! [`PlaybookRegistry`] fetches playbook Markdown files from devices via
//! [`SctlClient::file_read`] and caches them in memory. The cache is
//! invalidated after mutations (`playbook_put`) so the next `tools/list`
//! triggers a re-fetch.

use std::collections::HashMap;

use tokio::sync::RwLock;

use crate::client::SctlClient;
use crate::playbooks::{self, Playbook};

/// Cached playbooks for a single device.
struct DevicePlaybooks {
    loaded: bool,
    playbooks: Vec<Playbook>,
}

/// Registry of playbooks across all devices.
///
/// Thread-safe via [`RwLock`] — reads don't block each other.
pub struct PlaybookRegistry {
    cache: RwLock<HashMap<String, DevicePlaybooks>>,
    default_dir: String,
    device_dirs: HashMap<String, String>,
}

const DEFAULT_PLAYBOOKS_DIR: &str = "/etc/sctl/playbooks";

impl PlaybookRegistry {
    /// Create a new registry.
    ///
    /// `device_dirs` maps device names to their playbook directory override.
    /// Devices not in this map use `default_dir`.
    pub fn new(default_dir: String, device_dirs: HashMap<String, String>) -> Self {
        Self {
            cache: RwLock::new(HashMap::new()),
            default_dir,
            device_dirs,
        }
    }

    /// Create with the standard default directory.
    pub fn with_defaults(device_dirs: HashMap<String, String>) -> Self {
        Self::new(DEFAULT_PLAYBOOKS_DIR.to_string(), device_dirs)
    }

    /// Get the playbooks directory for a device.
    pub fn dir_for_device(&self, device: &str) -> &str {
        self.dir_for(device)
    }

    fn dir_for(&self, device: &str) -> &str {
        self.device_dirs
            .get(device)
            .map(String::as_str)
            .unwrap_or(&self.default_dir)
    }

    /// Lazy load: only fetches devices not yet loaded. Called from `tools/list`.
    pub async fn ensure_loaded(&self, clients: &HashMap<String, SctlClient>) {
        // Quick check under read lock
        {
            let cache = self.cache.read().await;
            let all_loaded = clients
                .keys()
                .all(|name| cache.get(name).is_some_and(|dp| dp.loaded));
            if all_loaded {
                return;
            }
        }

        // Fetch missing devices under write lock
        let mut cache = self.cache.write().await;
        for (name, client) in clients {
            let entry = cache.get(name);
            if entry.is_some_and(|dp| dp.loaded) {
                continue;
            }
            let playbooks = fetch_device_playbooks(client, self.dir_for(name), name).await;
            cache.insert(
                name.clone(),
                DevicePlaybooks {
                    loaded: true,
                    playbooks,
                },
            );
        }
    }

    /// Force refresh a single device. Returns the freshly loaded playbooks.
    pub async fn refresh_device(&self, device: &str, client: &SctlClient) -> Vec<Playbook> {
        let playbooks = fetch_device_playbooks(client, self.dir_for(device), device).await;
        let mut cache = self.cache.write().await;
        cache.insert(
            device.to_string(),
            DevicePlaybooks {
                loaded: true,
                playbooks: playbooks.clone(),
            },
        );
        playbooks
    }

    /// Force refresh all devices. Returns all freshly loaded playbooks.
    pub async fn refresh_all(&self, clients: &HashMap<String, SctlClient>) -> Vec<Playbook> {
        let mut all = Vec::new();
        let mut cache = self.cache.write().await;
        for (name, client) in clients {
            let playbooks = fetch_device_playbooks(client, self.dir_for(name), name).await;
            all.extend(playbooks.clone());
            cache.insert(
                name.clone(),
                DevicePlaybooks {
                    loaded: true,
                    playbooks,
                },
            );
        }
        all
    }

    /// Mark a device's cache as stale so the next `ensure_loaded` re-fetches.
    pub async fn invalidate_device(&self, device: &str) {
        let mut cache = self.cache.write().await;
        if let Some(entry) = cache.get_mut(device) {
            entry.loaded = false;
        }
    }

    /// Read all cached playbooks (does not fetch).
    pub async fn all_playbooks(&self) -> Vec<Playbook> {
        let cache = self.cache.read().await;
        cache
            .values()
            .flat_map(|dp| dp.playbooks.iter().cloned())
            .collect()
    }

    /// Find a playbook by its MCP tool name (e.g. `pb_restart-wifi`).
    pub async fn find_by_tool_name(&self, tool_name: &str) -> Option<Playbook> {
        let cache = self.cache.read().await;
        cache
            .values()
            .flat_map(|dp| dp.playbooks.iter())
            .find(|pb| pb.tool_name() == tool_name)
            .cloned()
    }
}

/// Fetch and parse all playbooks from a device.
///
/// First tries the REST endpoint (`GET /api/playbooks`). If that returns a 404
/// (server too old), falls back to the file-based approach (`file_read` on the
/// playbooks directory).
///
/// Graceful: returns an empty list on any error (device unreachable,
/// directory missing, etc.). Malformed playbooks are skipped.
async fn fetch_device_playbooks(
    client: &SctlClient,
    dir: &str,
    device_name: &str,
) -> Vec<Playbook> {
    // Try the REST endpoint first.
    match fetch_device_playbooks_rest(client, device_name).await {
        Ok(pbs) => return pbs,
        Err(e) if e.is_not_found() => {
            // Server doesn't have the playbook endpoint — fall back to file-based.
            eprintln!(
                "mcp-sctl: playbooks: {device_name}: REST endpoint not available, falling back to file-based"
            );
        }
        Err(e) => {
            eprintln!("mcp-sctl: playbooks: {device_name}: REST list failed: {e}");
            return Vec::new();
        }
    }

    fetch_device_playbooks_files(client, dir, device_name).await
}

/// Fetch playbooks via the REST endpoint (`GET /api/playbooks`).
///
/// The response is expected to have a `playbooks` array, where each entry has
/// at least `name` and `content` fields.
async fn fetch_device_playbooks_rest(
    client: &SctlClient,
    device_name: &str,
) -> Result<Vec<Playbook>, crate::client::ClientError> {
    let resp = client.list_playbooks().await?;

    let items = resp
        .get("playbooks")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut playbooks = Vec::new();
    for item in &items {
        let name = match item.get("name").and_then(|v| v.as_str()) {
            Some(n) => n,
            None => continue,
        };
        let content = match item.get("content").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => continue,
        };
        let path = item.get("path").and_then(|v| v.as_str()).unwrap_or(name);

        match playbooks::parse_playbook(content, device_name, path) {
            Ok(pb) => playbooks.push(pb),
            Err(e) => {
                eprintln!("mcp-sctl: playbooks: {device_name}: skip {name}: {e}");
            }
        }
    }

    Ok(playbooks)
}

/// Fetch playbooks via the file-based approach (legacy fallback).
///
/// Lists `.md` files in the playbooks directory, reads each one, and parses.
async fn fetch_device_playbooks_files(
    client: &SctlClient,
    dir: &str,
    device_name: &str,
) -> Vec<Playbook> {
    // List .md files in the playbooks directory
    let listing = match client.file_read(dir, true).await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("mcp-sctl: playbooks: {device_name}: cannot list {dir}: {e}");
            return Vec::new();
        }
    };

    // The listing response has an "entries" array of objects with "name" and "type"
    let entries = match listing.get("entries").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => return Vec::new(),
    };

    let md_files: Vec<String> = entries
        .iter()
        .filter_map(|entry| {
            let name = entry.get("name")?.as_str()?;
            if name.ends_with(".md") {
                Some(format!("{}/{}", dir, name))
            } else {
                None
            }
        })
        .collect();

    let mut playbooks = Vec::new();
    for file_path in md_files {
        match client.file_read(&file_path, false).await {
            Ok(v) => {
                let content = v
                    .get("content")
                    .and_then(|c| c.as_str())
                    .unwrap_or_default();
                match playbooks::parse_playbook(content, device_name, &file_path) {
                    Ok(pb) => playbooks.push(pb),
                    Err(e) => {
                        eprintln!("mcp-sctl: playbooks: {device_name}: skip {file_path}: {e}");
                    }
                }
            }
            Err(e) => {
                eprintln!("mcp-sctl: playbooks: {device_name}: cannot read {file_path}: {e}");
            }
        }
    }

    playbooks
}
