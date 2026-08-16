//! Dynamically loaded comms plugin supervision and cached projections.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use libloading::Library;
use sctl_comms_abi::{
    capabilities, methods, SctlCommsHostV1, SctlCommsLinkPollParams, SctlCommsMutSlice,
    SctlCommsOpenConfig, SctlCommsPluginInitV1, SctlCommsPluginV1, SctlCommsProbeResult,
    SctlCommsScanParams, SctlCommsSetBandsParams, SctlCommsSlice, SctlCommsStatusParams,
    SCTL_COMMS_ABI_VERSION, SCTL_COMMS_BAND_MODE_AUTO, SCTL_COMMS_BAND_MODE_LOCKED,
    SCTL_COMMS_CAP_CELLULAR_BAND_CONTROL, SCTL_COMMS_CAP_CELLULAR_SCAN,
    SCTL_COMMS_CAP_LINK_CELLULAR, SCTL_COMMS_CAP_LOCATION_GNSS,
    SCTL_COMMS_CAP_RECOVERY_TUNNEL_WATCHDOG, SCTL_COMMS_CAP_RECOVERY_USB_CYCLE, SCTL_COMMS_ERR,
    SCTL_COMMS_ERR_AT_FAILED, SCTL_COMMS_ERR_BUFFER_TOO_SMALL, SCTL_COMMS_ERR_INVALID,
    SCTL_COMMS_ERR_MODEM_UNAVAILABLE, SCTL_COMMS_ERR_SCAN_RUNNING, SCTL_COMMS_ERR_TUNNEL_CONNECTED,
    SCTL_COMMS_ERR_UNSUPPORTED, SCTL_COMMS_LOG_DEBUG, SCTL_COMMS_LOG_ERROR, SCTL_COMMS_LOG_INFO,
    SCTL_COMMS_LOG_WARN, SCTL_COMMS_MAX_BANDS, SCTL_COMMS_OK, SCTL_COMMS_SPEED_DOWNLOAD,
    SCTL_COMMS_SPEED_UPLOAD,
};
use serde_json::{json, Value};
use tokio::sync::{Mutex, Notify};
use tracing::{debug, error, info, warn};

use crate::at::AtPort;
use crate::atomic::AtomicU64;
use crate::config::{CommsConfig, Config};
use crate::state::TunnelStats;

const PLUGIN_JSON_BUF_INITIAL: usize = 64 * 1024;
const PLUGIN_JSON_BUF_MAX: usize = 1024 * 1024;

/// A cloneable, serialized client for one loaded comms plugin.
#[derive(Clone)]
pub struct CommsClient {
    inner: Arc<Mutex<LoadedPlugin>>,
    host: Arc<HostContext>,
    request_timeout: Duration,
    next_id: Arc<AtomicU64>,
}

struct LoadedPlugin {
    _library: Library,
    plugin: SctlCommsPluginV1,
    provider: String,
}

// The plugin pointer table is guarded by `tokio::sync::Mutex`; calls are
// serialized and run via `spawn_blocking`, so moving the handle between tasks is
// controlled by `CommsClient`.
unsafe impl Send for LoadedPlugin {}
unsafe impl Sync for LoadedPlugin {}

struct HostContext {
    at: StdMutex<Option<AtPort>>,
}

impl HostContext {
    fn new() -> Self {
        Self {
            at: StdMutex::new(None),
        }
    }

    fn open_at(&self, path: &str) -> Result<(), CommsCallError> {
        let port = AtPort::open(path).map_err(|e| CommsCallError::new("MODEM_UNAVAILABLE", e))?;
        *self
            .at
            .lock()
            .map_err(|_| CommsCallError::new("COMMS_HOST_LOCK", "AT lock poisoned"))? = Some(port);
        Ok(())
    }

    fn device(&self) -> Option<String> {
        self.at
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(|p| p.device().to_string()))
    }
}

/// Error returned by provider calls or the local plugin transport.
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

impl Drop for LoadedPlugin {
    fn drop(&mut self) {
        if let Some(destroy) = self.plugin.destroy {
            destroy(self.plugin.plugin_ctx);
        }
    }
}

impl CommsClient {
    /// Load a provider shared object.
    pub fn load(config: &CommsConfig) -> Result<Self, CommsCallError> {
        let library_path = config.effective_library();
        let host = Arc::new(HostContext::new());
        let host_table = host_table(host.as_ref());

        // SAFETY: the loaded library is kept alive inside `LoadedPlugin` for at
        // least as long as any function pointer from it can be called.
        let library = unsafe { Library::new(&library_path) }.map_err(|e| {
            CommsCallError::new(
                "COMMS_LOAD_FAILED",
                format!("load comms plugin {library_path}: {e}"),
            )
        })?;
        // SAFETY: symbol type matches the ABI header. Failure is reported.
        let init = unsafe { library.get::<SctlCommsPluginInitV1>(b"sctl_comms_plugin_init_v1") }
            .map_err(|e| {
                CommsCallError::new(
                    "COMMS_LOAD_FAILED",
                    format!("missing sctl_comms_plugin_init_v1 in {library_path}: {e}"),
                )
            })?;

        let mut plugin = SctlCommsPluginV1::default();
        // SAFETY: host table and output pointer are valid for the call.
        let rc = unsafe {
            init(
                std::ptr::from_ref(&host_table),
                std::ptr::from_mut(&mut plugin),
            )
        };
        if rc != SCTL_COMMS_OK {
            return Err(code_to_error(rc, "plugin init failed"));
        }
        if plugin.abi_version != SCTL_COMMS_ABI_VERSION {
            return Err(CommsCallError::new(
                "COMMS_ABI_MISMATCH",
                format!(
                    "plugin ABI {}, sctl ABI {}",
                    plugin.abi_version, SCTL_COMMS_ABI_VERSION
                ),
            ));
        }

        Ok(Self {
            inner: Arc::new(Mutex::new(LoadedPlugin {
                _library: library,
                plugin,
                provider: config.provider.clone(),
            })),
            host,
            request_timeout: Duration::from_secs(config.request_timeout_secs.max(1)),
            next_id: Arc::new(AtomicU64::new(1)),
        })
    }

    pub async fn probe(&self) -> Result<Value, CommsCallError> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            let guard = this.inner.blocking_lock();
            let Some(probe) = guard.plugin.probe else {
                return Err(CommsCallError::new(
                    crate::error::codes::COMMS_CAPABILITY_UNSUPPORTED,
                    "plugin does not implement probe",
                ));
            };
            let mut result = SctlCommsProbeResult::default();
            let rc = probe(guard.plugin.plugin_ctx, std::ptr::from_mut(&mut result));
            if rc != SCTL_COMMS_OK {
                return Err(code_to_error(rc, "probe failed"));
            }
            Ok(json!({
                "provider": guard.provider,
                "status": "ok",
                "detected_path": c_array_to_string(&result.detected_path),
                "capabilities": caps_to_strings(result.capabilities),
            }))
        })
        .await
        .map_err(|e| CommsCallError::new("COMMS_JOIN_FAILED", e.to_string()))?
    }

    pub async fn open(
        &self,
        config: &Config,
        comms_config: &CommsConfig,
        device: &str,
    ) -> Result<Value, CommsCallError> {
        self.host.open_at(device)?;
        let gps = config.gps.as_ref();
        let lte = config.lte.as_ref();
        let tunnel_url = config
            .tunnel
            .as_ref()
            .and_then(|tc| tc.url.as_deref())
            .unwrap_or("");
        let provider = comms_config.provider.clone();
        let data_dir = config.server.data_dir.clone();
        let device = device.to_string();
        let lte_interface = lte.map_or_else(|| "wwan0".to_string(), |c| c.interface.clone());
        let speed_test_url = lte
            .and_then(|c| c.speed_test_url.clone())
            .unwrap_or_default();
        let speed_test_upload_url = lte
            .and_then(|c| c.speed_test_upload_url.clone())
            .unwrap_or_default();
        let gps_enabled = gps.is_some();
        let gps_auto_enable = gps.is_none_or(|g| g.auto_enable);
        let gps_history_size = gps.map_or(100, |g| g.history_size);
        let lte_enabled = lte.is_some();
        let lte_watchdog = lte.is_some_and(|l| l.watchdog);
        let tunnel_url = tunnel_url.to_string();

        self.call_with_timeout_inner(
            Duration::from_secs(comms_config.startup_timeout_secs.max(1)),
            move |plugin| {
                let Some(open) = plugin.plugin.open else {
                    return Err(CommsCallError::new(
                        crate::error::codes::COMMS_CAPABILITY_UNSUPPORTED,
                        "plugin does not implement open",
                    ));
                };
                let open_config = SctlCommsOpenConfig {
                    provider: slice(&provider),
                    device: slice(&device),
                    data_dir: slice(&data_dir),
                    gps_enabled,
                    gps_auto_enable,
                    gps_history_size,
                    lte_enabled,
                    lte_watchdog,
                    lte_interface: slice(&lte_interface),
                    speed_test_url: slice(&speed_test_url),
                    speed_test_upload_url: slice(&speed_test_upload_url),
                    tunnel_url: slice(&tunnel_url),
                };
                call_json(|out| {
                    open(
                        plugin.plugin.plugin_ctx,
                        std::ptr::from_ref(&open_config),
                        out,
                    )
                })
            },
        )
        .await
    }

    /// Run a single AT command on the shared serial port. Used by the watchdog
    /// for diagnosis and recovery; serialized against plugin polls by the same
    /// `host.at` lock (it does not take the plugin lock, so it cannot deadlock
    /// against an in-flight `poll_link`).
    pub async fn at_command(
        &self,
        command: &str,
        timeout: Duration,
    ) -> Result<String, CommsCallError> {
        let host = self.host.clone();
        let command = command.to_string();
        tokio::task::spawn_blocking(move || {
            let guard = host
                .at
                .lock()
                .map_err(|_| CommsCallError::new("COMMS_HOST_LOCK", "AT lock poisoned"))?;
            let port = guard
                .as_ref()
                .ok_or_else(|| CommsCallError::new("MODEM_UNAVAILABLE", "AT port not open"))?;
            port.command_blocking(&command, timeout.max(Duration::from_millis(500)))
                .map_err(|e| CommsCallError::new("MODEM_AT_FAILED", e))
        })
        .await
        .map_err(|e| CommsCallError::new("COMMS_JOIN_FAILED", e.to_string()))?
    }

    /// The serial device path currently open, if any.
    #[must_use]
    pub fn device(&self) -> Option<String> {
        self.host.device()
    }

    /// Send one method call. Calls are serialized per plugin instance.
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
        let _id = self.next_id.fetch_add(1, Ordering::Relaxed);
        match method {
            methods::HELLO | methods::CAPABILITIES | methods::DETECT => self.probe().await,
            methods::LINK_SPEED_TEST => self.speed_test(params, timeout).await,
            methods::RECOVERY_USB_CYCLE => {
                let value = self.call_plugin_json(method, params, timeout).await?;
                match self.reopen_after_probe().await {
                    Ok(()) => {}
                    Err(err) => warn!("comms reopen after usb cycle failed: {err}"),
                }
                Ok(value)
            }
            _ => self.call_plugin_json(method, params, timeout).await,
        }
    }

    async fn call_plugin_json(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, CommsCallError> {
        let method = method.to_string();
        self.call_with_timeout_inner(timeout, move |plugin| match method.as_str() {
            methods::STATUS => {
                let Some(status) = plugin.plugin.status else {
                    return Err(unsupported(&method));
                };
                let args = SctlCommsStatusParams {
                    tunnel_connected: params
                        .get("tunnel_connected")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                };
                call_json(|out| status(plugin.plugin.plugin_ctx, std::ptr::from_ref(&args), out))
            }
            methods::LOCATION_POLL => {
                let Some(poll) = plugin.plugin.poll_location else {
                    return Err(unsupported(&method));
                };
                call_json(|out| poll(plugin.plugin.plugin_ctx, out))
            }
            methods::LOCATION_DISABLE => {
                let Some(disable) = plugin.plugin.disable_location else {
                    return Err(unsupported(&method));
                };
                call_json(|out| disable(plugin.plugin.plugin_ctx, out))
            }
            methods::LINK_POLL => {
                let Some(poll) = plugin.plugin.poll_link else {
                    return Err(unsupported(&method));
                };
                let args = SctlCommsLinkPollParams {
                    refresh: params
                        .get("refresh")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    tunnel_connected: params
                        .get("tunnel_connected")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                };
                call_json(|out| poll(plugin.plugin.plugin_ctx, std::ptr::from_ref(&args), out))
            }
            methods::CELLULAR_SET_BANDS => {
                let Some(set_bands) = plugin.plugin.set_bands else {
                    return Err(unsupported(&method));
                };
                let args = set_bands_params(&params)?;
                call_json(|out| set_bands(plugin.plugin.plugin_ctx, std::ptr::from_ref(&args), out))
            }
            methods::CELLULAR_SCAN => {
                let Some(scan) = plugin.plugin.scan else {
                    return Err(unsupported(&method));
                };
                let args = scan_params(&params)?;
                call_json(|out| scan(plugin.plugin.plugin_ctx, std::ptr::from_ref(&args), out))
            }
            methods::RECOVERY_USB_CYCLE => {
                let Some(usb_cycle) = plugin.plugin.usb_cycle else {
                    return Err(unsupported(&method));
                };
                call_json(|out| usb_cycle(plugin.plugin.plugin_ctx, out))
            }
            _ => Err(unsupported(&method)),
        })
        .await
    }

    async fn call_with_timeout_inner<F>(
        &self,
        timeout: Duration,
        f: F,
    ) -> Result<Value, CommsCallError>
    where
        F: FnOnce(&LoadedPlugin) -> Result<Value, CommsCallError> + Send + 'static,
    {
        let inner = self.inner.clone();
        match tokio::time::timeout(
            timeout.max(Duration::from_secs(1)),
            tokio::task::spawn_blocking(move || {
                let guard = inner.blocking_lock();
                f(&guard)
            }),
        )
        .await
        {
            Ok(Ok(result)) => result,
            Ok(Err(e)) => Err(CommsCallError::new("COMMS_JOIN_FAILED", e.to_string())),
            Err(_) => Err(CommsCallError::new(
                "COMMS_TIMEOUT",
                "provider call timed out",
            )),
        }
    }

    async fn reopen_after_probe(&self) -> Result<(), CommsCallError> {
        let detected = self
            .probe()
            .await?
            .get("detected_path")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
            .or_else(|| self.host.device())
            .ok_or_else(|| {
                CommsCallError::new("MODEM_UNAVAILABLE", "no device path after USB cycle")
            })?;
        self.host.open_at(&detected)
    }

    async fn speed_test(&self, params: Value, timeout: Duration) -> Result<Value, CommsCallError> {
        let interface = params
            .get("interface")
            .and_then(Value::as_str)
            .unwrap_or("wwan0")
            .to_string();
        let download_url = params
            .get("download_url")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        let upload_url = params
            .get("upload_url")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        match tokio::time::timeout(
            timeout.max(Duration::from_secs(1)),
            tokio::task::spawn_blocking(move || {
                let download_bps = download_url.as_deref().and_then(|url| {
                    run_speed_test_blocking(SCTL_COMMS_SPEED_DOWNLOAD, url, &interface)
                });
                let upload_bps = upload_url.as_deref().and_then(|url| {
                    run_speed_test_blocking(SCTL_COMMS_SPEED_UPLOAD, url, &interface)
                });
                json!({
                    "download_bps": download_bps,
                    "upload_bps": upload_bps,
                })
            }),
        )
        .await
        {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(e)) => Err(CommsCallError::new("COMMS_JOIN_FAILED", e.to_string())),
            Err(_) => Err(CommsCallError::new("COMMS_TIMEOUT", "speed test timed out")),
        }
    }
}

fn call_json<F>(mut f: F) -> Result<Value, CommsCallError>
where
    F: FnMut(SctlCommsMutSlice) -> i32,
{
    let mut cap = PLUGIN_JSON_BUF_INITIAL;
    loop {
        let mut buf = vec![0u8; cap];
        let mut out_len = 0usize;
        let rc = f(mut_slice(&mut buf, &mut out_len));
        if rc == SCTL_COMMS_ERR_BUFFER_TOO_SMALL && cap < PLUGIN_JSON_BUF_MAX {
            cap = (cap * 2).min(PLUGIN_JSON_BUF_MAX);
            continue;
        }
        if rc != SCTL_COMMS_OK {
            return Err(code_to_error(rc, "plugin call failed"));
        }
        let raw = std::str::from_utf8(&buf[..out_len]).map_err(|e| {
            CommsCallError::new(
                "COMMS_DECODE_FAILED",
                format!("plugin returned non-UTF8 JSON: {e}"),
            )
        })?;
        if raw.trim().is_empty() {
            return Ok(Value::Null);
        }
        return serde_json::from_str(raw).map_err(|e| {
            CommsCallError::new(
                "COMMS_DECODE_FAILED",
                format!("plugin returned invalid JSON: {e}; raw={}", raw.trim()),
            )
        });
    }
}

fn set_bands_params(params: &Value) -> Result<SctlCommsSetBandsParams, CommsCallError> {
    let mode = match params.get("mode").and_then(Value::as_str).unwrap_or("") {
        "auto" => SCTL_COMMS_BAND_MODE_AUTO,
        "locked" => SCTL_COMMS_BAND_MODE_LOCKED,
        other => {
            return Err(CommsCallError::new(
                "INVALID_REQUEST",
                format!("mode must be 'locked' or 'auto', got {other:?}"),
            ))
        }
    };
    let mut bands = [0u16; SCTL_COMMS_MAX_BANDS];
    let raw_bands = params
        .get("bands")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if raw_bands.len() > SCTL_COMMS_MAX_BANDS {
        return Err(CommsCallError::new("INVALID_REQUEST", "too many bands"));
    }
    for (idx, value) in raw_bands.iter().enumerate() {
        bands[idx] = value
            .as_u64()
            .and_then(|v| u16::try_from(v).ok())
            .ok_or_else(|| CommsCallError::new("INVALID_REQUEST", "band must be u16"))?;
    }
    let priority_band = params
        .get("priority_band")
        .and_then(Value::as_u64)
        .and_then(|v| u16::try_from(v).ok())
        .unwrap_or(0);
    Ok(SctlCommsSetBandsParams {
        mode,
        bands,
        bands_len: raw_bands.len(),
        priority_band,
        has_priority_band: params.get("priority_band").is_some_and(|v| !v.is_null()),
        force: params
            .get("force")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        tunnel_connected: params
            .get("tunnel_connected")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn scan_params(params: &Value) -> Result<SctlCommsScanParams, CommsCallError> {
    let mut bands = [0u16; SCTL_COMMS_MAX_BANDS];
    let raw_bands = params
        .get("bands")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if raw_bands.len() > SCTL_COMMS_MAX_BANDS {
        return Err(CommsCallError::new("INVALID_REQUEST", "too many bands"));
    }
    for (idx, value) in raw_bands.iter().enumerate() {
        bands[idx] = value
            .as_u64()
            .and_then(|v| u16::try_from(v).ok())
            .ok_or_else(|| CommsCallError::new("INVALID_REQUEST", "band must be u16"))?;
    }
    Ok(SctlCommsScanParams {
        bands,
        bands_len: raw_bands.len(),
        include_speed_test: params
            .get("include_speed_test")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        force: params
            .get("force")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        tunnel_connected: params
            .get("tunnel_connected")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
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
    /// Watchdog snapshot, owned by the watchdog task and merged into the
    /// `watchdog` field of the `/api/lte` response (the plugin emits `null`).
    pub watchdog: Option<Value>,
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
            watchdog: None,
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
            self.detected_path = if path.is_empty() {
                None
            } else {
                Some(path.to_string())
            };
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
    let client = CommsClient::load(comms_config)?;
    let probe = client.probe().await?;
    debug!(?probe, "comms plugin probe");

    let device = comms_config
        .effective_device(config)
        .or_else(|| {
            probe
                .get("detected_path")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string)
        })
        .ok_or_else(|| {
            CommsCallError::new(
                "MODEM_UNAVAILABLE",
                "no comms device detected and no [comms].device configured",
            )
        })?;

    let opened = client.open(config, comms_config, &device).await?;
    let mut state = CommsState::new(&comms_config.provider);
    state.apply_status(&probe);
    state.apply_status(&opened);
    if state.detected_path.is_none() {
        state.detected_path = Some(device);
    }
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

fn host_table(host: &HostContext) -> SctlCommsHostV1 {
    SctlCommsHostV1 {
        abi_version: SCTL_COMMS_ABI_VERSION,
        ctx: std::ptr::from_ref(host).cast_mut().cast(),
        log: Some(host_log),
        now_ms: Some(host_now_ms),
        at_command: Some(host_at_command),
        interface_has_ipv4: Some(host_interface_has_ipv4),
        speed_test: Some(host_speed_test),
        usb_cycle: Some(host_usb_cycle),
    }
}

extern "C" fn host_log(_ctx: *mut std::ffi::c_void, level: i32, msg: SctlCommsSlice) {
    let msg = slice_to_string(msg).unwrap_or_else(|| "<invalid utf8>".to_string());
    match level {
        SCTL_COMMS_LOG_ERROR => error!(target: "sctl_comms_plugin", "{}", msg),
        SCTL_COMMS_LOG_WARN => warn!(target: "sctl_comms_plugin", "{}", msg),
        SCTL_COMMS_LOG_DEBUG => debug!(target: "sctl_comms_plugin", "{}", msg),
        SCTL_COMMS_LOG_INFO => info!(target: "sctl_comms_plugin", "{}", msg),
        _ => info!(target: "sctl_comms_plugin", "{}", msg),
    }
}

extern "C" fn host_now_ms(_ctx: *mut std::ffi::c_void) -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

extern "C" fn host_at_command(
    ctx: *mut std::ffi::c_void,
    command: SctlCommsSlice,
    timeout_ms: u64,
    out: SctlCommsMutSlice,
) -> i32 {
    let Some(host) = host_from_ctx(ctx) else {
        return SCTL_COMMS_ERR;
    };
    let Some(command) = slice_to_string(command) else {
        return SCTL_COMMS_ERR_INVALID;
    };
    let Ok(guard) = host.at.lock() else {
        return SCTL_COMMS_ERR;
    };
    let Some(port) = guard.as_ref() else {
        return SCTL_COMMS_ERR_MODEM_UNAVAILABLE;
    };
    match port.command_blocking(&command, Duration::from_millis(timeout_ms.max(1))) {
        Ok(resp) => write_out(out, resp.as_bytes()),
        Err(err) => {
            warn!("AT command failed: {err}");
            SCTL_COMMS_ERR_AT_FAILED
        }
    }
}

extern "C" fn host_interface_has_ipv4(_ctx: *mut std::ffi::c_void, iface: SctlCommsSlice) -> bool {
    let Some(iface) = slice_to_string(iface) else {
        return false;
    };
    interface_has_ipv4_blocking(&iface)
}

extern "C" fn host_speed_test(
    _ctx: *mut std::ffi::c_void,
    kind: i32,
    url: SctlCommsSlice,
    iface: SctlCommsSlice,
    out_bps: *mut u64,
) -> i32 {
    let (Some(url), Some(iface)) = (slice_to_string(url), slice_to_string(iface)) else {
        return SCTL_COMMS_ERR_INVALID;
    };
    let Some(value) = run_speed_test_blocking(kind, &url, &iface) else {
        return SCTL_COMMS_ERR;
    };
    if !out_bps.is_null() {
        // SAFETY: plugin provided a valid out pointer by ABI contract.
        unsafe {
            *out_bps = value;
        }
    }
    SCTL_COMMS_OK
}

extern "C" fn host_usb_cycle(ctx: *mut std::ffi::c_void, out: SctlCommsMutSlice) -> i32 {
    let Some(host) = host_from_ctx(ctx) else {
        return SCTL_COMMS_ERR;
    };
    let Some(device) = host.device() else {
        return SCTL_COMMS_ERR_MODEM_UNAVAILABLE;
    };
    match usb_cycle_blocking(&device) {
        Ok(()) => write_out(out, b""),
        Err(err) => {
            warn!("USB cycle failed for {device}: {err}");
            SCTL_COMMS_ERR
        }
    }
}

fn host_from_ctx(ctx: *mut std::ffi::c_void) -> Option<&'static HostContext> {
    if ctx.is_null() {
        return None;
    }
    // SAFETY: ctx is created from an `Arc<HostContext>` stored by `CommsClient`;
    // the plugin cannot outlive that client because the library handle is held
    // inside the same client and all calls are serialized.
    Some(unsafe { &*ctx.cast::<HostContext>() })
}

fn slice(value: &str) -> SctlCommsSlice {
    SctlCommsSlice {
        ptr: value.as_ptr(),
        len: value.len(),
    }
}

fn mut_slice(buf: &mut [u8], out_len: &mut usize) -> SctlCommsMutSlice {
    SctlCommsMutSlice {
        ptr: buf.as_mut_ptr(),
        cap: buf.len(),
        len: out_len,
    }
}

fn slice_to_string(slice: SctlCommsSlice) -> Option<String> {
    if slice.ptr.is_null() || slice.len == 0 {
        return Some(String::new());
    }
    // SAFETY: plugin/host provides pointer+length valid for the callback.
    let bytes = unsafe { std::slice::from_raw_parts(slice.ptr, slice.len) };
    String::from_utf8(bytes.to_vec()).ok()
}

fn write_out(out: SctlCommsMutSlice, bytes: &[u8]) -> i32 {
    if out.ptr.is_null() || out.len.is_null() {
        return SCTL_COMMS_ERR_INVALID;
    }
    if bytes.len() > out.cap {
        return SCTL_COMMS_ERR_BUFFER_TOO_SMALL;
    }
    // SAFETY: out points to a caller-provided buffer of at least cap bytes.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), out.ptr, bytes.len());
        *out.len = bytes.len();
    }
    SCTL_COMMS_OK
}

fn c_array_to_string(buf: &[std::ffi::c_char]) -> Option<String> {
    let bytes: Vec<u8> = buf
        .iter()
        .take_while(|&&c| c != 0)
        // `c_char` is i8 on some targets, u8 on others; reinterpret the raw
        // byte without a sign-losing cast.
        .map(|&c| c.to_ne_bytes()[0])
        .collect();
    if bytes.is_empty() {
        None
    } else {
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }
}

fn caps_to_strings(caps: u64) -> Vec<&'static str> {
    let mut out = Vec::new();
    if caps & SCTL_COMMS_CAP_LOCATION_GNSS != 0 {
        out.push(capabilities::LOCATION_GNSS);
    }
    if caps & SCTL_COMMS_CAP_LINK_CELLULAR != 0 {
        out.push(capabilities::LINK_CELLULAR);
    }
    if caps & SCTL_COMMS_CAP_CELLULAR_BAND_CONTROL != 0 {
        out.push(capabilities::CELLULAR_BAND_CONTROL);
    }
    if caps & SCTL_COMMS_CAP_CELLULAR_SCAN != 0 {
        out.push(capabilities::CELLULAR_SCAN);
    }
    if caps & SCTL_COMMS_CAP_RECOVERY_USB_CYCLE != 0 {
        out.push(capabilities::RECOVERY_USB_CYCLE);
    }
    if caps & SCTL_COMMS_CAP_RECOVERY_TUNNEL_WATCHDOG != 0 {
        out.push(capabilities::RECOVERY_TUNNEL_WATCHDOG);
    }
    out
}

fn unsupported(method: &str) -> CommsCallError {
    CommsCallError::new(
        crate::error::codes::COMMS_CAPABILITY_UNSUPPORTED,
        format!("active comms plugin does not support {method}"),
    )
}

fn code_to_error(code: i32, fallback: &str) -> CommsCallError {
    match code {
        SCTL_COMMS_ERR_UNSUPPORTED => {
            CommsCallError::new(crate::error::codes::COMMS_CAPABILITY_UNSUPPORTED, fallback)
        }
        SCTL_COMMS_ERR_INVALID => CommsCallError::new("INVALID_REQUEST", fallback),
        SCTL_COMMS_ERR_BUFFER_TOO_SMALL => CommsCallError::new("COMMS_BUFFER_TOO_SMALL", fallback),
        SCTL_COMMS_ERR_MODEM_UNAVAILABLE => CommsCallError::new("MODEM_UNAVAILABLE", fallback),
        SCTL_COMMS_ERR_AT_FAILED => CommsCallError::new("MODEM_AT_FAILED", fallback),
        SCTL_COMMS_ERR_TUNNEL_CONNECTED => CommsCallError::new("TUNNEL_CONNECTED", fallback),
        SCTL_COMMS_ERR_SCAN_RUNNING => CommsCallError::new("SCAN_RUNNING", fallback),
        SCTL_COMMS_ERR => CommsCallError::new("COMMS_PROVIDER_ERROR", fallback),
        _ => CommsCallError::new("COMMS_PROVIDER_ERROR", format!("{fallback} ({code})")),
    }
}

fn interface_has_ipv4_blocking(iface: &str) -> bool {
    let output = std::process::Command::new("ip")
        .args(["-4", "addr", "show", "dev", iface])
        .output();
    output.is_ok_and(|out| out.status.success() && !out.stdout.is_empty())
}

fn run_speed_test_blocking(kind: i32, url: &str, interface: &str) -> Option<u64> {
    match kind {
        SCTL_COMMS_SPEED_DOWNLOAD => {
            let output = std::process::Command::new("curl")
                .args([
                    "-o",
                    "/dev/null",
                    "-w",
                    "%{speed_download}",
                    "-s",
                    "--connect-timeout",
                    "5",
                    "--max-time",
                    "10",
                    "--interface",
                    interface,
                    url,
                ])
                .output()
                .ok()?;
            parse_curl_speed(&output.stdout)
        }
        SCTL_COMMS_SPEED_UPLOAD => {
            let tmp_path = "/tmp/sctl-upload-test.bin";
            let dd = std::process::Command::new("dd")
                .args([
                    "if=/dev/urandom",
                    &format!("of={tmp_path}"),
                    "bs=256k",
                    "count=8",
                ])
                .stderr(std::process::Stdio::null())
                .output()
                .ok()?;
            if !dd.status.success() {
                return None;
            }
            let output = std::process::Command::new("curl")
                .args([
                    "-X",
                    "POST",
                    "--data-binary",
                    &format!("@{tmp_path}"),
                    "-o",
                    "/dev/null",
                    "-w",
                    "%{speed_upload}",
                    "-s",
                    "--connect-timeout",
                    "3",
                    "--max-time",
                    "10",
                    "--interface",
                    interface,
                    url,
                ])
                .output()
                .ok();
            let _ = std::fs::remove_file(tmp_path);
            output
                .as_ref()
                .and_then(|out| parse_curl_speed(&out.stdout))
        }
        _ => None,
    }
}

fn parse_curl_speed(stdout: &[u8]) -> Option<u64> {
    let stdout = String::from_utf8_lossy(stdout);
    // `v.max(0.0)` guarantees a non-negative value, so the sign can't be lost.
    #[allow(clippy::cast_sign_loss)]
    let speed = stdout.trim().parse::<f64>().ok().map(|v| v.max(0.0) as u64);
    match speed {
        Some(0) | None => None,
        s => s,
    }
}

fn usb_cycle_blocking(device_path: &str) -> Result<(), String> {
    let tty = Path::new(device_path)
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| format!("invalid tty path {device_path}"))?;
    let sys_tty = PathBuf::from("/sys/class/tty").join(tty).join("device");
    let mut path = std::fs::canonicalize(&sys_tty)
        .map_err(|e| format!("resolve {}: {e}", sys_tty.display()))?;
    let auth = loop {
        let candidate = path.join("authorized");
        if candidate.exists() {
            break candidate;
        }
        if !path.pop() {
            return Err(format!("no authorized sysfs node for {device_path}"));
        }
    };
    std::fs::write(&auth, b"0").map_err(|e| format!("deauthorize {}: {e}", auth.display()))?;
    std::thread::sleep(Duration::from_secs(5));
    std::fs::write(&auth, b"1").map_err(|e| format!("reauthorize {}: {e}", auth.display()))?;
    std::thread::sleep(Duration::from_secs(8));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_capability_bits_to_public_names() {
        let caps = SCTL_COMMS_CAP_LOCATION_GNSS | SCTL_COMMS_CAP_LINK_CELLULAR;
        assert_eq!(
            caps_to_strings(caps),
            vec![capabilities::LOCATION_GNSS, capabilities::LINK_CELLULAR]
        );
    }

    #[test]
    fn parses_band_params() {
        let params = json!({
            "mode": "locked",
            "bands": [4, 12],
            "priority_band": 4,
            "force": true,
            "tunnel_connected": true,
        });
        let parsed = set_bands_params(&params).unwrap();
        assert_eq!(parsed.mode, SCTL_COMMS_BAND_MODE_LOCKED);
        assert_eq!(parsed.bands_len, 2);
        assert_eq!(parsed.bands[0], 4);
        assert!(parsed.has_priority_band);
        assert!(parsed.force);
        assert!(parsed.tunnel_connected);
    }
}
