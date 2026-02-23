//! HTTP client for sctl REST endpoints.
//!
//! [`SctlClient`] wraps `reqwest::Client` and provides typed methods for
//! each sctl HTTP endpoint. All responses are returned as `serde_json::Value`
//! — the MCP tools layer handles formatting for the AI agent.
//!
//! ## Authentication
//!
//! All endpoints except `/api/health` use Bearer token authentication.
//!
//! ## Error handling
//!
//! Non-2xx responses are parsed for an `error` field in the JSON body. If
//! parsing fails, the raw response body is returned as the error message.

use std::collections::HashMap;
use std::time::Duration;

/// HTTP client for a single sctl device.
pub struct SctlClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl SctlClient {
    /// Create a new client for a sctl device at the given URL.
    pub fn new(base_url: String, api_key: String) -> Self {
        let mut default_headers = reqwest::header::HeaderMap::new();
        default_headers.insert(
            reqwest::header::HeaderName::from_static("x-sctl-client"),
            reqwest::header::HeaderValue::from_static("mcp"),
        );
        let http = reqwest::Client::builder()
            .default_headers(default_headers)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .expect("Failed to build HTTP client");
        // Strip trailing slash for consistent URL construction
        let base_url = base_url.trim_end_matches('/').to_string();
        Self {
            http,
            base_url,
            api_key,
        }
    }

    /// The device's base URL (without trailing slash).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The API key used for Bearer authentication.
    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    /// `GET /api/health` — liveness probe (no auth required).
    pub async fn health(&self) -> Result<serde_json::Value, ClientError> {
        let resp = self
            .http
            .get(format!("{}/api/health", self.base_url))
            .send()
            .await
            .map_err(ClientError::Request)?;
        Self::handle_response(resp).await
    }

    /// `GET /api/info` — system information (hostname, CPU, memory, etc.).
    pub async fn info(&self) -> Result<serde_json::Value, ClientError> {
        let resp = self
            .http
            .get(format!("{}/api/info", self.base_url))
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(ClientError::Request)?;
        Self::handle_response(resp).await
    }

    /// `POST /api/exec` — execute a single command with optional timeout and env.
    pub async fn exec(
        &self,
        command: &str,
        timeout_ms: Option<u64>,
        working_dir: Option<&str>,
        env: Option<&HashMap<String, String>>,
    ) -> Result<serde_json::Value, ClientError> {
        let mut body = serde_json::json!({ "command": command });
        if let Some(t) = timeout_ms {
            body["timeout_ms"] = serde_json::json!(t);
        }
        if let Some(d) = working_dir {
            body["working_dir"] = serde_json::json!(d);
        }
        if let Some(e) = env {
            body["env"] = serde_json::json!(e);
        }

        let resp = self
            .http
            .post(format!("{}/api/exec", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(ClientError::Request)?;
        Self::handle_response(resp).await
    }

    /// `POST /api/exec/batch` — execute multiple commands sequentially.
    pub async fn exec_batch(
        &self,
        commands: &[serde_json::Value],
        working_dir: Option<&str>,
        env: Option<&HashMap<String, String>>,
    ) -> Result<serde_json::Value, ClientError> {
        let mut body = serde_json::json!({ "commands": commands });
        if let Some(d) = working_dir {
            body["working_dir"] = serde_json::json!(d);
        }
        if let Some(e) = env {
            body["env"] = serde_json::json!(e);
        }

        let resp = self
            .http
            .post(format!("{}/api/exec/batch", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(ClientError::Request)?;
        Self::handle_response(resp).await
    }

    /// `GET /api/files` — read a file or list a directory.
    pub async fn file_read(
        &self,
        path: &str,
        list: bool,
    ) -> Result<serde_json::Value, ClientError> {
        let mut url = reqwest::Url::parse(&format!("{}/api/files", self.base_url))
            .map_err(|e| ClientError::Protocol(format!("Invalid base URL: {e}")))?;
        url.query_pairs_mut().append_pair("path", path);
        if list {
            url.query_pairs_mut().append_pair("list", "true");
        }

        let resp = self
            .http
            .get(url)
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(ClientError::Request)?;
        Self::handle_response(resp).await
    }

    /// `PUT /api/files` — write a file atomically.
    pub async fn file_write(
        &self,
        path: &str,
        content: &str,
        encoding: Option<&str>,
        mode: Option<&str>,
        create_dirs: Option<bool>,
    ) -> Result<serde_json::Value, ClientError> {
        let mut body = serde_json::json!({
            "path": path,
            "content": content,
        });
        if let Some(e) = encoding {
            body["encoding"] = serde_json::json!(e);
        }
        if let Some(m) = mode {
            body["mode"] = serde_json::json!(m);
        }
        if let Some(c) = create_dirs {
            body["create_dirs"] = serde_json::json!(c);
        }

        let resp = self
            .http
            .put(format!("{}/api/files", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(ClientError::Request)?;
        Self::handle_response(resp).await
    }

    /// `DELETE /api/files` — delete a file.
    pub async fn file_delete(&self, path: &str) -> Result<serde_json::Value, ClientError> {
        let body = serde_json::json!({ "path": path });
        let resp = self
            .http
            .delete(format!("{}/api/files", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(ClientError::Request)?;
        Self::handle_response(resp).await
    }

    /// `GET /api/activity` — read activity log.
    pub async fn activity(
        &self,
        since_id: u64,
        limit: u64,
    ) -> Result<serde_json::Value, ClientError> {
        let url = format!(
            "{}/api/activity?since_id={}&limit={}",
            self.base_url, since_id, limit
        );
        let resp = self
            .http
            .get(url)
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(ClientError::Request)?;
        Self::handle_response(resp).await
    }

    // --- Playbook REST endpoints ---

    /// `GET /api/playbooks` — list available playbooks.
    pub async fn list_playbooks(&self) -> Result<serde_json::Value, ClientError> {
        let resp = self
            .http
            .get(format!("{}/api/playbooks", self.base_url))
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(ClientError::Request)?;
        Self::handle_response(resp).await
    }

    /// `GET /api/playbooks/:name` — get playbook detail.
    pub async fn get_playbook(&self, name: &str) -> Result<serde_json::Value, ClientError> {
        let resp = self
            .http
            .get(format!("{}/api/playbooks/{}", self.base_url, name))
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(ClientError::Request)?;
        Self::handle_response(resp).await
    }

    /// `PUT /api/playbooks/:name` — create/update a playbook.
    pub async fn put_playbook(
        &self,
        name: &str,
        content: &str,
    ) -> Result<serde_json::Value, ClientError> {
        let resp = self
            .http
            .put(format!("{}/api/playbooks/{}", self.base_url, name))
            .bearer_auth(&self.api_key)
            .header("content-type", "text/markdown")
            .body(content.to_string())
            .send()
            .await
            .map_err(ClientError::Request)?;
        Self::handle_response(resp).await
    }

    /// `DELETE /api/playbooks/:name` — delete a playbook.
    pub async fn delete_playbook(&self, name: &str) -> Result<serde_json::Value, ClientError> {
        let resp = self
            .http
            .delete(format!("{}/api/playbooks/{}", self.base_url, name))
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(ClientError::Request)?;
        Self::handle_response(resp).await
    }

    /// Parse an HTTP response — returns the JSON body on success, or a
    /// [`ClientError`] with the error message on failure.
    async fn handle_response(resp: reqwest::Response) -> Result<serde_json::Value, ClientError> {
        let status = resp.status();
        let body = resp.text().await.map_err(ClientError::Request)?;

        if status.is_success() {
            serde_json::from_str(&body)
                .map_err(|e| ClientError::Protocol(format!("Invalid JSON from device: {}", e)))
        } else {
            // Try to extract error message from JSON body
            let message = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v["error"].as_str().map(String::from))
                .unwrap_or(body);
            Err(ClientError::Device {
                status: status.as_u16(),
                message,
            })
        }
    }
}

/// Errors returned by [`SctlClient`] methods.
#[derive(Debug)]
pub enum ClientError {
    /// HTTP transport error (connection refused, timeout, DNS failure, etc.).
    Request(reqwest::Error),
    /// The device returned a non-2xx HTTP status.
    Device { status: u16, message: String },
    /// The response body was not valid JSON.
    Protocol(String),
}

impl ClientError {
    /// Returns `true` if the error is an HTTP 404 Not Found response.
    pub fn is_not_found(&self) -> bool {
        matches!(self, ClientError::Device { status: 404, .. })
    }
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Request(e) => write!(f, "HTTP request failed: {}", e),
            ClientError::Device { status, message } => {
                write!(f, "Device error (HTTP {}): {}", status, message)
            }
            ClientError::Protocol(msg) => write!(f, "Protocol error: {}", msg),
        }
    }
}
