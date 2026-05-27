//! External comms provider supervision and cached capability projections.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use sctl_comms_protocol::{methods, CommsRequest, CommsResponse};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, Notify};
use tracing::{debug, warn};

use crate::config::{CommsConfig, Config};
use crate::state::TunnelStats;

/// A cloneable, serialized client for one provider helper process.
#[derive(Clone)]
pub struct CommsClient {
    inner: Arc<Mutex<CommsProcess>>,
    next_id: Arc<AtomicU64>,
    request_timeout: Duration,
}

struct CommsProcess {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    child: Child,
    broken: Option<CommsCallError>,
}

/// Error returned by provider calls or the local child-process transport.
#[derive(Debug, Clone)]
pub struct CommsCallError {
    pub code: String,
    pub message: String,
}

impl CommsCallError {
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for CommsCallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for CommsCallError {}

impl CommsClient {
    /// Start a provider helper process.
    pub async fn spawn(config: &CommsConfig) -> Result<Self, CommsCallError> {
        let command = config.effective_command();
        let mut child = Command::new(&command)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                CommsCallError::new(
                    "COMMS_START_FAILED",
                    format!("start provider helper {command}: {e}"),
                )
            })?;

        let stdin = child.stdin.take().ok_or_else(|| {
            CommsCallError::new("COMMS_START_FAILED", "provider helper stdin unavailable")
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            CommsCallError::new("COMMS_START_FAILED", "provider helper stdout unavailable")
        })?;

        Ok(Self {
            inner: Arc::new(Mutex::new(CommsProcess {
                stdin,
                stdout: BufReader::new(stdout),
                child,
                broken: None,
            })),
            next_id: Arc::new(AtomicU64::new(1)),
            request_timeout: Duration::from_secs(config.request_timeout_secs.max(1)),
        })
    }

    /// Send one method call. Calls are serialized per helper process.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, CommsCallError> {
        self.call_with_timeout(method, params, self.request_timeout)
            .await
    }

    pub async fn call_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, CommsCallError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        let req = CommsRequest {
            id: id.clone(),
            method: method.to_string(),
            params,
        };
        let raw = serde_json::to_string(&req)
            .map_err(|e| CommsCallError::new("COMMS_ENCODE_FAILED", e.to_string()))?;

        let mut guard = self.inner.lock().await;
        if let Some(err) = guard.broken.clone() {
            return Err(err);
        }

        match tokio::time::timeout(
            timeout.max(Duration::from_secs(1)),
            exchange(&mut guard, &id, &raw),
        )
        .await
        {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(err)) => {
                if is_transport_error(&err.code) {
                    guard.mark_broken(err.clone());
                }
                Err(err)
            }
            Err(_) => {
                let err = CommsCallError::new("COMMS_TIMEOUT", format!("{method} timed out"));
                guard.mark_broken(err.clone());
                Err(err)
            }
        }
    }
}

impl CommsProcess {
    fn mark_broken(&mut self, err: CommsCallError) {
        let _ = self.child.start_kill();
        self.broken = Some(err);
    }
}

async fn exchange(
    process: &mut CommsProcess,
    id: &str,
    raw: &str,
) -> Result<Value, CommsCallError> {
    process
        .stdin
        .write_all(raw.as_bytes())
        .await
        .map_err(|e| CommsCallError::new("COMMS_WRITE_FAILED", e.to_string()))?;
    process
        .stdin
        .write_all(b"\n")
        .await
        .map_err(|e| CommsCallError::new("COMMS_WRITE_FAILED", e.to_string()))?;
    process
        .stdin
        .flush()
        .await
        .map_err(|e| CommsCallError::new("COMMS_WRITE_FAILED", e.to_string()))?;

    let mut line = String::new();
    let n = process
        .stdout
        .read_line(&mut line)
        .await
        .map_err(|e| CommsCallError::new("COMMS_READ_FAILED", e.to_string()))?;
    if n == 0 {
        return Err(CommsCallError::new(
            "COMMS_HELPER_EXITED",
            "provider helper closed stdout",
        ));
    }

    let resp: CommsResponse = serde_json::from_str(&line).map_err(|e| {
        CommsCallError::new(
            "COMMS_DECODE_FAILED",
            format!("invalid provider response: {e}; raw={}", line.trim()),
        )
    })?;
    if resp.id != id {
        return Err(CommsCallError::new(
            "COMMS_ID_MISMATCH",
            format!("expected response id {id}, got {}", resp.id),
        ));
    }
    if !resp.ok {
        let err = resp
            .error
            .unwrap_or_else(|| sctl_comms_protocol::CommsError {
                code: "COMMS_PROVIDER_ERROR".to_string(),
                message: "provider returned an error".to_string(),
                detail: None,
            });
        return Err(CommsCallError::new(err.code, err.message));
    }
    Ok(resp.result.unwrap_or(Value::Null))
}

fn is_transport_error(code: &str) -> bool {
    matches!(
        code,
        "COMMS_WRITE_FAILED"
            | "COMMS_READ_FAILED"
            | "COMMS_DECODE_FAILED"
            | "COMMS_ID_MISMATCH"
            | "COMMS_HELPER_EXITED"
    )
}

/// Cached comms provider projections used by existing HTTP endpoints.
#[derive(Debug, Clone)]
pub struct CommsState {
    pub provider: String,
    pub status: String,
    pub capabilities: Vec<String>,
    pub detected_path: Option<String>,
    pub gps: Option<Value>,
    pub lte: Option<Value>,
    pub last_error: Option<String>,
    pub errors_total: u64,
}

impl CommsState {
    #[must_use]
    pub fn new(provider: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            status: "starting".to_string(),
            capabilities: Vec::new(),
            detected_path: None,
            gps: None,
            lte: None,
            last_error: None,
            errors_total: 0,
        }
    }

    pub fn mark_error(&mut self, err: &CommsCallError) {
        self.status = "error".to_string();
        self.last_error = Some(err.to_string());
        self.errors_total = self.errors_total.saturating_add(1);
    }

    pub fn apply_status(&mut self, value: &Value) {
        if let Some(status) = value.get("status").and_then(Value::as_str) {
            self.status = status.to_string();
        }
        if let Some(provider) = value.get("provider").and_then(Value::as_str) {
            self.provider = provider.to_string();
        }
        if let Some(path) = value.get("detected_path").and_then(Value::as_str) {
            self.detected_path = Some(path.to_string());
        }
        if let Some(caps) = value.get("capabilities").and_then(Value::as_array) {
            self.capabilities = caps
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect();
        }
    }

    #[must_use]
    pub fn has_capability(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|c| c == capability)
    }
}

/// Start the provider and open the configured hardware.
pub async fn start_provider(
    config: &Config,
    comms_config: &CommsConfig,
) -> Result<(CommsClient, CommsState), CommsCallError> {
    let client = CommsClient::spawn(comms_config).await?;
    let hello = client.call(methods::HELLO, json!({})).await?;
    debug!(?hello, "comms provider hello");

    let open_params = json!({
        "provider": comms_config.provider,
        "device": comms_config.effective_device(config),
        "data_dir": config.server.data_dir,
        "gps": config.gps,
        "lte": config.lte,
        "tunnel_url": config.tunnel.as_ref().and_then(|tc| tc.url.clone()),
    });
    let opened = client
        .call_with_timeout(
            methods::OPEN,
            open_params,
            Duration::from_secs(comms_config.startup_timeout_secs.max(1)),
        )
        .await?;

    let mut state = CommsState::new(&comms_config.provider);
    state.apply_status(&hello);
    state.apply_status(&opened);
    Ok((client, state))
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_poller(
    client: CommsClient,
    comms_state: Arc<Mutex<CommsState>>,
    gps_enabled: bool,
    gps_interval_secs: u64,
    lte_enabled: bool,
    lte_interval_secs: u64,
    tunnel_stats: Arc<TunnelStats>,
    notify: Arc<Notify>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let gps_interval = Duration::from_secs(gps_interval_secs.max(1));
        let lte_interval = Duration::from_secs(lte_interval_secs.max(1));
        let start = Instant::now();
        let mut last_gps = start.checked_sub(gps_interval).unwrap_or(start);
        let mut last_lte = start.checked_sub(lte_interval).unwrap_or(start);

        loop {
            let now = Instant::now();
            let mut force_lte = false;
            tokio::select! {
                () = notify.notified() => {
                    force_lte = true;
                }
                () = tokio::time::sleep(Duration::from_secs(1)) => {}
            }

            let tunnel_connected = tunnel_stats.connected.load(Ordering::Relaxed);
            match client
                .call(
                    methods::STATUS,
                    json!({
                        "tunnel_connected": tunnel_connected,
                    }),
                )
                .await
            {
                Ok(value) => comms_state.lock().await.apply_status(&value),
                Err(err) => {
                    warn!("comms status failed: {err}");
                    comms_state.lock().await.mark_error(&err);
                }
            }

            if gps_enabled && now.duration_since(last_gps) >= gps_interval {
                poll_location(&client, &comms_state).await;
                last_gps = now;
            }

            if lte_enabled && (force_lte || now.duration_since(last_lte) >= lte_interval) {
                poll_link(&client, &comms_state, &tunnel_stats, force_lte).await;
                last_lte = now;
            }
        }
    })
}

pub async fn poll_location(client: &CommsClient, state: &Arc<Mutex<CommsState>>) {
    match client.call(methods::LOCATION_POLL, json!({})).await {
        Ok(value) => {
            let mut guard = state.lock().await;
            guard.gps = Some(value);
            guard.status = "ok".to_string();
            guard.last_error = None;
        }
        Err(err) => {
            warn!("comms location poll failed: {err}");
            state.lock().await.mark_error(&err);
        }
    }
}

pub async fn poll_link(
    client: &CommsClient,
    state: &Arc<Mutex<CommsState>>,
    tunnel_stats: &TunnelStats,
    refresh: bool,
) {
    let tunnel_connected = tunnel_stats.connected.load(Ordering::Relaxed);
    match client
        .call(
            methods::LINK_POLL,
            json!({
                "refresh": refresh,
                "tunnel_connected": tunnel_connected,
            }),
        )
        .await
    {
        Ok(value) => {
            let mut guard = state.lock().await;
            guard.lte = Some(value);
            guard.status = "ok".to_string();
            guard.last_error = None;
        }
        Err(err) => {
            warn!("comms link poll failed: {err}");
            state.lock().await.mark_error(&err);
        }
    }
}

#[must_use]
pub fn starting_gps_response() -> Value {
    json!({
        "status": "searching",
        "last_fix": null,
        "fix_age_secs": null,
        "history": [],
        "fixes_total": 0,
        "errors_total": 0,
        "last_error": null,
    })
}

#[must_use]
pub fn starting_lte_response() -> Value {
    json!({
        "signal": null,
        "modem": null,
        "errors_total": 0,
        "last_error": null,
        "band_history": [],
        "scan_status": null,
        "registration_pending": false,
        "watchdog": null,
    })
}
