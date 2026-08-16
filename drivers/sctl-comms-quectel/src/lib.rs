#![allow(clippy::missing_safety_doc)]

use std::collections::VecDeque;
use std::ffi::{c_char, c_void};
use std::fmt::Write as _;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sctl_comms_abi::{
    capabilities, SctlCommsHostV1, SctlCommsLinkPollParams, SctlCommsMutSlice, SctlCommsOpenConfig,
    SctlCommsPluginV1, SctlCommsProbeResult, SctlCommsScanParams, SctlCommsSetBandsParams,
    SctlCommsSlice, SctlCommsStatusParams, SCTL_COMMS_ABI_VERSION, SCTL_COMMS_BAND_MODE_AUTO,
    SCTL_COMMS_CAP_CELLULAR_BAND_CONTROL, SCTL_COMMS_CAP_LINK_CELLULAR,
    SCTL_COMMS_CAP_LOCATION_GNSS, SCTL_COMMS_CAP_RECOVERY_USB_CYCLE, SCTL_COMMS_ERR_AT_FAILED,
    SCTL_COMMS_ERR_BUFFER_TOO_SMALL, SCTL_COMMS_ERR_INVALID, SCTL_COMMS_ERR_MODEM_UNAVAILABLE,
    SCTL_COMMS_ERR_TUNNEL_CONNECTED, SCTL_COMMS_ERR_UNSUPPORTED, SCTL_COMMS_LOG_INFO,
    SCTL_COMMS_LOG_WARN, SCTL_COMMS_MAX_STR, SCTL_COMMS_OK,
};

const PROVIDER: &str = "quectel-at";
const AT_BUF_INITIAL: usize = 4096;
const AT_BUF_MAX: usize = 64 * 1024;
const GPS_HISTORY_DEFAULT: usize = 100;

struct Plugin {
    host: SctlCommsHostV1,
    config: RuntimeConfig,
    gps: GpsState,
    lte: LteState,
    opened: bool,
    detected_path: Option<String>,
}

#[derive(Default)]
struct RuntimeConfig {
    gps_enabled: bool,
    gps_auto_enable: bool,
    gps_history_size: usize,
    lte_enabled: bool,
    lte_interface: String,
}

#[derive(Clone)]
struct GpsFix {
    latitude: f64,
    longitude: f64,
    altitude: f64,
    speed_kmh: f64,
    course: f64,
    hdop: f64,
    satellites: u32,
    utc: String,
    date: String,
    fix_type: u32,
    recorded_at: u64,
}

struct GpsState {
    status: &'static str,
    last_fix: Option<GpsFix>,
    history: VecDeque<GpsFix>,
    history_max: usize,
    fixes_total: u64,
    errors_total: u64,
    last_error: Option<String>,
    last_fix_ms: Option<u64>,
}

impl GpsState {
    fn new(history_max: usize) -> Self {
        Self {
            status: "disabled",
            last_fix: None,
            history: VecDeque::with_capacity(history_max),
            history_max,
            fixes_total: 0,
            errors_total: 0,
            last_error: None,
            last_fix_ms: None,
        }
    }

    fn push_fix(&mut self, fix: GpsFix) {
        if self.history.len() >= self.history_max {
            self.history.pop_front();
        }
        self.last_fix_ms = Some(now_ms());
        self.history.push_back(fix.clone());
        self.last_fix = Some(fix);
        self.fixes_total = self.fixes_total.saturating_add(1);
        self.status = "active";
        self.last_error = None;
    }

    fn record_error(&mut self, msg: String, escalate: bool) {
        self.errors_total = self.errors_total.saturating_add(1);
        self.last_error = Some(msg);
        if escalate {
            self.status = "error";
        }
    }
}

#[derive(Default, Clone)]
struct ModemInfo {
    model: Option<String>,
    firmware: Option<String>,
    imei: Option<String>,
    iccid: Option<String>,
    imsi: Option<String>,
}

#[derive(Clone)]
struct NeighborCell {
    earfcn: u32,
    pci: u16,
    rsrp: Option<i32>,
    rsrq: Option<i32>,
    rssi: Option<i32>,
    sinr: Option<f64>,
    cell_type: String,
}

#[derive(Clone)]
struct BandConfig {
    enabled_bands: Vec<u16>,
    priority_band: Option<u16>,
}

#[derive(Clone)]
struct LteSignal {
    rssi_dbm: i32,
    rsrp: Option<i32>,
    rsrq: Option<i32>,
    sinr: Option<f64>,
    band: Option<String>,
    operator: Option<String>,
    technology: Option<String>,
    cell_id: Option<String>,
    pci: Option<u16>,
    earfcn: Option<u32>,
    freq_band: Option<u16>,
    tac: Option<String>,
    plmn: Option<String>,
    enodeb_id: Option<u32>,
    sector: Option<u8>,
    ul_bw_mhz: Option<String>,
    dl_bw_mhz: Option<String>,
    connection_state: Option<String>,
    duplex: Option<String>,
    neighbors: Vec<NeighborCell>,
    band_config: Option<BandConfig>,
    signal_bars: u8,
    recorded_at: u64,
}

#[derive(Default)]
struct LteState {
    modem: Option<ModemInfo>,
    signal: Option<LteSignal>,
    errors_total: u64,
    last_error: Option<String>,
    registration_pending: bool,
    scan_status: Option<String>,
}

impl Plugin {
    fn new(host: SctlCommsHostV1) -> Self {
        Self {
            host,
            config: RuntimeConfig::default(),
            gps: GpsState::new(GPS_HISTORY_DEFAULT),
            lte: LteState::default(),
            opened: false,
            detected_path: None,
        }
    }

    fn capabilities() -> u64 {
        SCTL_COMMS_CAP_LOCATION_GNSS
            | SCTL_COMMS_CAP_LINK_CELLULAR
            | SCTL_COMMS_CAP_CELLULAR_BAND_CONTROL
            | SCTL_COMMS_CAP_RECOVERY_USB_CYCLE
    }

    fn at(&self, command: &str, timeout_ms: u64) -> Result<String, i32> {
        let Some(at_command) = self.host.at_command else {
            return Err(SCTL_COMMS_ERR_MODEM_UNAVAILABLE);
        };
        let mut cap = AT_BUF_INITIAL;
        loop {
            let mut buf = vec![0u8; cap];
            let mut out_len = 0usize;
            let rc = at_command(
                self.host.ctx,
                slice(command),
                timeout_ms,
                mut_slice(&mut buf, &mut out_len),
            );
            if rc == SCTL_COMMS_ERR_BUFFER_TOO_SMALL && cap < AT_BUF_MAX {
                cap = (cap * 2).min(AT_BUF_MAX);
                continue;
            }
            if rc != SCTL_COMMS_OK {
                return Err(rc);
            }
            return Ok(String::from_utf8_lossy(&buf[..out_len]).into_owned());
        }
    }

    fn log(&self, level: i32, msg: &str) {
        if let Some(log) = self.host.log {
            log(self.host.ctx, level, slice(msg));
        }
    }

    fn open(&mut self, config: &SctlCommsOpenConfig) -> String {
        let device = slice_to_string(config.device);
        self.config = RuntimeConfig {
            gps_enabled: config.gps_enabled,
            gps_auto_enable: config.gps_auto_enable,
            gps_history_size: config.gps_history_size.max(1),
            lte_enabled: config.lte_enabled,
            lte_interface: slice_to_string(config.lte_interface),
        };
        self.gps = GpsState::new(self.config.gps_history_size);
        self.detected_path = Some(device);
        self.opened = true;

        if self.config.gps_enabled && self.config.gps_auto_enable {
            self.enable_gps();
        }
        if self.config.lte_enabled {
            self.lte.modem = Some(self.read_modem_info());
        }
        self.status_json()
    }

    fn enable_gps(&mut self) {
        for attempt in 1..=3 {
            match self.at("AT+QGPS=1", 5_000) {
                // 504 = GNSS already on. Match the full `+CME ERROR: 504`
                // token, not a bare "504" substring, which could appear inside
                // an ICCID/IMSI/coordinate echoed in the reply.
                Ok(resp) if resp.contains("OK") || resp.contains("CME ERROR: 504") => {
                    self.gps.status = "searching";
                    self.log(SCTL_COMMS_LOG_INFO, "GNSS enabled");
                    return;
                }
                Ok(resp) => {
                    self.log(
                        SCTL_COMMS_LOG_WARN,
                        &format!("GNSS enable attempt {attempt}: {}", resp.trim()),
                    );
                }
                Err(_) if attempt == 3 => {
                    self.gps
                        .record_error("failed to enable GNSS".to_string(), true);
                }
                Err(_) => {}
            }
            std::thread::sleep(Duration::from_secs(3));
        }
    }

    fn status_json(&self) -> String {
        let mut out = String::new();
        out.push('{');
        field_str(&mut out, "provider", PROVIDER, false);
        field_str(
            &mut out,
            "status",
            if self.opened { "ok" } else { "not_open" },
            true,
        );
        field_opt_str(
            &mut out,
            "detected_path",
            self.detected_path.as_deref(),
            true,
        );
        out.push_str(",\"capabilities\":");
        push_capabilities(&mut out, Self::capabilities());
        out.push('}');
        out
    }

    fn poll_location(&mut self) -> Result<String, i32> {
        if !self.config.gps_enabled {
            return Err(SCTL_COMMS_ERR_UNSUPPORTED);
        }
        match self.at("AT+QGPSLOC=2", 5_000) {
            Ok(resp) => match parse_qgpsloc(&resp) {
                Ok(fix) => self.gps.push_fix(fix),
                Err(err) if err == "searching" => {
                    self.gps.status = "searching";
                    self.gps.last_error = None;
                }
                Err(err) => self.gps.record_error(err, false),
            },
            Err(_) => self.gps.record_error("AT+QGPSLOC failed".to_string(), true),
        }
        Ok(self.gps_json())
    }

    fn gps_json(&self) -> String {
        let mut out = String::new();
        out.push('{');
        field_str(&mut out, "status", self.gps.status, false);
        out.push_str(",\"last_fix\":");
        if let Some(ref fix) = self.gps.last_fix {
            push_gps_fix(&mut out, fix, true);
        } else {
            out.push_str("null");
        }
        out.push_str(",\"fix_age_secs\":");
        match self.gps.last_fix_ms {
            Some(ms) => out.push_str(&(now_ms().saturating_sub(ms) / 1000).to_string()),
            None => out.push_str("null"),
        }
        out.push_str(",\"history\":[");
        for (idx, fix) in self.gps.history.iter().rev().take(50).enumerate() {
            if idx > 0 {
                out.push(',');
            }
            push_gps_fix(&mut out, fix, false);
        }
        out.push(']');
        out.push_str(",\"fixes_total\":");
        out.push_str(&self.gps.fixes_total.to_string());
        out.push_str(",\"errors_total\":");
        out.push_str(&self.gps.errors_total.to_string());
        field_opt_str(&mut out, "last_error", self.gps.last_error.as_deref(), true);
        out.push('}');
        out
    }

    fn disable_location(&mut self) -> String {
        let _ = self.at("AT+QGPSEND", 5_000);
        self.gps.status = "disabled";
        "{\"status\":\"ok\"}".to_string()
    }

    fn poll_link(&mut self, params: SctlCommsLinkPollParams) -> Result<String, i32> {
        if !self.config.lte_enabled {
            return Err(SCTL_COMMS_ERR_UNSUPPORTED);
        }
        match self.read_lte_signal(params.tunnel_connected, params.refresh) {
            Ok(signal) => {
                self.lte.signal = Some(signal);
                self.lte.last_error = None;
            }
            Err(err) => {
                self.lte.errors_total = self.lte.errors_total.saturating_add(1);
                self.lte.last_error = Some(err.clone());
                return Err(SCTL_COMMS_ERR_AT_FAILED);
            }
        }
        Ok(self.lte_json())
    }

    fn read_lte_signal(&self, tunnel_connected: bool, refresh: bool) -> Result<LteSignal, String> {
        let csq = self
            .at("AT+CSQ", 5_000)
            .map_err(|_| "AT+CSQ failed".to_string())?;
        let rssi_dbm = parse_csq(&csq)?;
        let qeng_resp = self
            .at("AT+QENG=\"servingcell\"", 5_000)
            .unwrap_or_default();
        let qeng = parse_qeng(&qeng_resp);
        let data_active =
            tunnel_connected || interface_has_ipv4(&self.host, &self.config.lte_interface);
        // Gentle poll: while the data bearer is up, an unforced poll issues
        // only CSQ + QENG servingcell (above). Firing the full AT set every
        // cycle disrupts the QMI raw-IP bearer on the EC25 (see the
        // no-modem-polling-on-bpi constraint), so the remaining commands run
        // only on a user-forced refresh or when the bearer is down. Band and
        // operator still come from QENG, so the gentle path stays populated.
        let full = refresh || !data_active;
        let (technology, qnw_band) = if full {
            self.at("AT+QNWINFO", 5_000)
                .ok()
                .map_or((None, None), |resp| parse_qnwinfo(&resp))
        } else {
            (None, None)
        };
        let operator = if full {
            self.at("AT+COPS?", 5_000)
                .ok()
                .and_then(|resp| parse_cops(&resp))
                .or_else(|| qeng.plmn.as_deref().map(operator_name_from_plmn))
        } else {
            qeng.plmn.as_deref().map(operator_name_from_plmn)
        };
        let neighbors = if full {
            self.at("AT+QENG=\"neighbourcell\"", 5_000)
                .ok()
                .map(|resp| parse_neighbour(&resp))
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let enabled_bands = if full {
            self.at("AT+QCFG=\"band\"", 5_000)
                .ok()
                .map(|resp| parse_band_config(&resp))
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let priority_band = if full {
            self.at("AT+QCFG=\"bandpri\"", 5_000)
                .ok()
                .and_then(|resp| parse_bandpri(&resp))
        } else {
            None
        };
        let connection_state = effective_connection_state(qeng.state.clone(), data_active);
        let band = qnw_band.or_else(|| qeng.freq_band.map(|b| format!("B{b}")));
        let signal_bars = compute_signal_bars(qeng.rsrp, rssi_dbm);
        Ok(LteSignal {
            rssi_dbm,
            rsrp: qeng.rsrp,
            rsrq: qeng.rsrq,
            sinr: qeng.sinr,
            band,
            operator,
            technology,
            cell_id: qeng.cell_id,
            pci: qeng.pci,
            earfcn: qeng.earfcn,
            freq_band: qeng.freq_band,
            tac: qeng.tac,
            plmn: qeng.plmn,
            enodeb_id: qeng.enodeb_id,
            sector: qeng.sector,
            ul_bw_mhz: qeng.ul_bw,
            dl_bw_mhz: qeng.dl_bw,
            connection_state,
            duplex: qeng.duplex,
            neighbors,
            band_config: Some(BandConfig {
                enabled_bands,
                priority_band,
            }),
            signal_bars,
            recorded_at: now_secs(),
        })
    }

    fn read_modem_info(&self) -> ModemInfo {
        ModemInfo {
            model: self
                .at("AT+CGMM", 5_000)
                .ok()
                .and_then(|r| parse_simple_line(&r)),
            firmware: self
                .at("AT+CGMR", 5_000)
                .ok()
                .and_then(|r| parse_simple_line(&r)),
            imei: self
                .at("AT+GSN", 5_000)
                .ok()
                .and_then(|r| parse_simple_line(&r)),
            iccid: self
                .at("AT+QCCID", 5_000)
                .ok()
                .and_then(|r| parse_qccid(&r)),
            imsi: self.at("AT+CIMI", 5_000).ok().and_then(|r| parse_cimi(&r)),
        }
    }

    fn lte_json(&self) -> String {
        let mut out = String::new();
        out.push('{');
        out.push_str("\"signal\":");
        if let Some(ref sig) = self.lte.signal {
            push_lte_signal(&mut out, sig);
        } else {
            out.push_str("null");
        }
        out.push_str(",\"modem\":");
        if let Some(ref modem) = self.lte.modem {
            push_modem_info(&mut out, modem);
        } else {
            out.push_str("null");
        }
        out.push_str(",\"errors_total\":");
        out.push_str(&self.lte.errors_total.to_string());
        field_opt_str(&mut out, "last_error", self.lte.last_error.as_deref(), true);
        out.push_str(",\"band_history\":[]");
        out.push_str(",\"scan_status\":");
        match self.lte.scan_status.as_deref() {
            Some(status) => {
                out.push('{');
                field_str(&mut out, "state", status, false);
                out.push('}');
            }
            None => out.push_str("null"),
        }
        out.push_str(",\"registration_pending\":");
        out.push_str(if self.lte.registration_pending {
            "true"
        } else {
            "false"
        });
        // Watchdog is temporarily disabled in the 0.5.0 plugin (the autonomous
        // recovery state machine is pending a dedicated review). The field is
        // kept in the response shape so consumers don't have to special-case
        // its absence; it returns to a real object when the watchdog lands.
        out.push_str(",\"watchdog\":null");
        out.push('}');
        out
    }

    fn set_bands(&mut self, params: &SctlCommsSetBandsParams) -> Result<String, i32> {
        if params.tunnel_connected && !params.force {
            return Err(SCTL_COMMS_ERR_TUNNEL_CONNECTED);
        }
        if !self.config.lte_enabled {
            return Err(SCTL_COMMS_ERR_UNSUPPORTED);
        }
        let bands = if params.mode == SCTL_COMMS_BAND_MODE_AUTO && params.bands_len == 0 {
            (1u16..=128).collect::<Vec<_>>()
        } else {
            // Clamp defensively: `bands_len` crosses the FFI boundary from the
            // host, and an out-of-range value would panic the slice index —
            // which, under panic=abort, takes down the whole server process.
            let n = params.bands_len.min(params.bands.len());
            params.bands[..n].to_vec()
        };
        if bands.is_empty() {
            return Err(SCTL_COMMS_ERR_INVALID);
        }
        let hex = bands_to_hex(&bands);
        self.at(&format!("AT+QCFG=\"band\",260,{hex},0"), 10_000)
            .map_err(|_| SCTL_COMMS_ERR_AT_FAILED)?;
        if params.has_priority_band {
            let _ = self.at(
                &format!("AT+QCFG=\"bandpri\",{}", params.priority_band),
                5_000,
            );
        }
        self.lte.registration_pending = true;
        let band_config = BandConfig {
            enabled_bands: bands,
            priority_band: params.has_priority_band.then_some(params.priority_band),
        };
        if let Some(ref mut signal) = self.lte.signal {
            signal.band_config = Some(band_config.clone());
        }
        let snapshot = self.lte_json();
        let mut out = String::new();
        out.push('{');
        field_str(&mut out, "status", "ok", false);
        field_str(
            &mut out,
            "mode",
            if params.mode == SCTL_COMMS_BAND_MODE_AUTO {
                "auto"
            } else {
                "locked"
            },
            true,
        );
        out.push_str(",\"band_config\":");
        push_band_config(&mut out, &band_config);
        field_str(&mut out, "registration", "pending", true);
        out.push_str(",\"snapshot\":");
        out.push_str(&snapshot);
        out.push('}');
        Ok(out)
    }

    fn usb_cycle(&mut self) -> Result<String, i32> {
        let Some(usb_cycle) = self.host.usb_cycle else {
            return Err(SCTL_COMMS_ERR_UNSUPPORTED);
        };
        let mut buf = [0u8; SCTL_COMMS_MAX_STR];
        let mut out_len = 0usize;
        let rc = usb_cycle(self.host.ctx, mut_slice(&mut buf, &mut out_len));
        if rc != SCTL_COMMS_OK {
            return Err(rc);
        }
        self.opened = false;
        let mut out = String::new();
        out.push('{');
        field_str(&mut out, "action", "usb_cycle", false);
        out.push_str(",\"detected_path\":null");
        out.push('}');
        Ok(out)
    }
}

#[no_mangle]
pub unsafe extern "C" fn sctl_comms_plugin_init_v1(
    host: *const SctlCommsHostV1,
    out: *mut SctlCommsPluginV1,
) -> i32 {
    if host.is_null() || out.is_null() {
        return SCTL_COMMS_ERR_INVALID;
    }
    // SAFETY: pointers are checked above and valid for the duration of init.
    let host = unsafe { *host };
    if host.abi_version != SCTL_COMMS_ABI_VERSION {
        return SCTL_COMMS_ERR_INVALID;
    }
    let plugin = Box::new(Plugin::new(host));
    // SAFETY: out is checked non-null and points to caller-owned output.
    unsafe {
        *out = SctlCommsPluginV1 {
            abi_version: SCTL_COMMS_ABI_VERSION,
            plugin_ctx: Box::into_raw(plugin).cast(),
            destroy: Some(plugin_destroy),
            probe: Some(plugin_probe),
            open: Some(plugin_open),
            close: Some(plugin_close),
            status: Some(plugin_status),
            poll_location: Some(plugin_poll_location),
            disable_location: Some(plugin_disable_location),
            poll_link: Some(plugin_poll_link),
            set_bands: Some(plugin_set_bands),
            scan: Some(plugin_scan),
            usb_cycle: Some(plugin_usb_cycle),
        };
    }
    SCTL_COMMS_OK
}

extern "C" fn plugin_destroy(ctx: *mut c_void) {
    if !ctx.is_null() {
        // SAFETY: ctx came from Box::into_raw in init and is destroyed once.
        unsafe {
            drop(Box::from_raw(ctx.cast::<Plugin>()));
        }
    }
}

extern "C" fn plugin_probe(ctx: *mut c_void, out: *mut SctlCommsProbeResult) -> i32 {
    let Some(plugin) = plugin_mut(ctx) else {
        return SCTL_COMMS_ERR_INVALID;
    };
    if out.is_null() {
        return SCTL_COMMS_ERR_INVALID;
    }
    let detected = detect_quectel_at_port();
    plugin.detected_path.clone_from(&detected);
    // SAFETY: out is checked non-null.
    unsafe {
        (*out).capabilities = Plugin::capabilities();
        if let Some(path) = detected {
            write_c_array(&mut (*out).detected_path, &path);
        }
    }
    SCTL_COMMS_OK
}

extern "C" fn plugin_open(
    ctx: *mut c_void,
    config: *const SctlCommsOpenConfig,
    out: SctlCommsMutSlice,
) -> i32 {
    let Some(plugin) = plugin_mut(ctx) else {
        return SCTL_COMMS_ERR_INVALID;
    };
    if config.is_null() {
        return SCTL_COMMS_ERR_INVALID;
    }
    // SAFETY: config is checked non-null and valid for this call.
    let json = plugin.open(unsafe { &*config });
    write_out(out, json.as_bytes())
}

extern "C" fn plugin_close(ctx: *mut c_void) -> i32 {
    let Some(plugin) = plugin_mut(ctx) else {
        return SCTL_COMMS_ERR_INVALID;
    };
    plugin.opened = false;
    SCTL_COMMS_OK
}

extern "C" fn plugin_status(
    ctx: *mut c_void,
    _params: *const SctlCommsStatusParams,
    out: SctlCommsMutSlice,
) -> i32 {
    let Some(plugin) = plugin_mut(ctx) else {
        return SCTL_COMMS_ERR_INVALID;
    };
    write_out(out, plugin.status_json().as_bytes())
}

extern "C" fn plugin_poll_location(ctx: *mut c_void, out: SctlCommsMutSlice) -> i32 {
    let Some(plugin) = plugin_mut(ctx) else {
        return SCTL_COMMS_ERR_INVALID;
    };
    match plugin.poll_location() {
        Ok(json) => write_out(out, json.as_bytes()),
        Err(rc) => rc,
    }
}

extern "C" fn plugin_disable_location(ctx: *mut c_void, out: SctlCommsMutSlice) -> i32 {
    let Some(plugin) = plugin_mut(ctx) else {
        return SCTL_COMMS_ERR_INVALID;
    };
    let json = plugin.disable_location();
    write_out(out, json.as_bytes())
}

extern "C" fn plugin_poll_link(
    ctx: *mut c_void,
    params: *const SctlCommsLinkPollParams,
    out: SctlCommsMutSlice,
) -> i32 {
    let Some(plugin) = plugin_mut(ctx) else {
        return SCTL_COMMS_ERR_INVALID;
    };
    if params.is_null() {
        return SCTL_COMMS_ERR_INVALID;
    }
    // SAFETY: params is checked non-null and valid for the call.
    match plugin.poll_link(unsafe { *params }) {
        Ok(json) => write_out(out, json.as_bytes()),
        Err(rc) => rc,
    }
}

extern "C" fn plugin_set_bands(
    ctx: *mut c_void,
    params: *const SctlCommsSetBandsParams,
    out: SctlCommsMutSlice,
) -> i32 {
    let Some(plugin) = plugin_mut(ctx) else {
        return SCTL_COMMS_ERR_INVALID;
    };
    if params.is_null() {
        return SCTL_COMMS_ERR_INVALID;
    }
    // SAFETY: params is checked non-null and valid for the call.
    match plugin.set_bands(unsafe { &*params }) {
        Ok(json) => write_out(out, json.as_bytes()),
        Err(rc) => rc,
    }
}

extern "C" fn plugin_scan(
    _ctx: *mut c_void,
    _params: *const SctlCommsScanParams,
    _out: SctlCommsMutSlice,
) -> i32 {
    SCTL_COMMS_ERR_UNSUPPORTED
}

extern "C" fn plugin_usb_cycle(ctx: *mut c_void, out: SctlCommsMutSlice) -> i32 {
    let Some(plugin) = plugin_mut(ctx) else {
        return SCTL_COMMS_ERR_INVALID;
    };
    match plugin.usb_cycle() {
        Ok(json) => write_out(out, json.as_bytes()),
        Err(rc) => rc,
    }
}

fn plugin_mut(ctx: *mut c_void) -> Option<&'static mut Plugin> {
    if ctx.is_null() {
        None
    } else {
        // SAFETY: ctx came from Box::into_raw in init and calls are serialized by sctl.
        Some(unsafe { &mut *ctx.cast::<Plugin>() })
    }
}

fn detect_quectel_at_port() -> Option<String> {
    let entries = std::fs::read_dir("/sys/bus/usb/devices").ok()?;
    for entry in entries.flatten() {
        let device = entry.file_name().to_string_lossy().into_owned();
        let vendor_path = format!("/sys/bus/usb/devices/{device}/idVendor");
        let Ok(vendor) = std::fs::read_to_string(vendor_path) else {
            continue;
        };
        if vendor.trim() != "2c7c" {
            continue;
        }
        let iface = format!("/sys/bus/usb/devices/{device}/{device}:1.2");
        let Ok(iface_entries) = std::fs::read_dir(&iface) else {
            continue;
        };
        for iface_entry in iface_entries.flatten() {
            let name = iface_entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with("ttyUSB") {
                continue;
            }
            let tty_path = format!("{iface}/{name}/tty");
            if let Ok(tty_entries) = std::fs::read_dir(tty_path) {
                for tty in tty_entries.flatten() {
                    let tty_name = tty.file_name().to_string_lossy().into_owned();
                    if tty_name.starts_with("ttyUSB") {
                        return Some(format!("/dev/{tty_name}"));
                    }
                }
            }
            return Some(format!("/dev/{name}"));
        }
    }
    None
}

fn parse_qgpsloc(response: &str) -> Result<GpsFix, String> {
    let line = response.lines().find(|l| l.contains("+QGPSLOC:"));
    if line.is_none() {
        if response.contains("516") && response.contains("ERROR") {
            return Err("searching".into());
        }
        if response.contains("ERROR") {
            return Err(format!("modem error: {}", response.trim()));
        }
        return Err(format!("no +QGPSLOC in response: {}", response.trim()));
    }
    let line = line.unwrap_or_default();
    let data = line
        .split(':')
        .nth(1)
        .ok_or("malformed +QGPSLOC line")?
        .trim();
    let parts: Vec<&str> = data.split(',').collect();
    if parts.len() < 11 {
        return Err(format!(
            "expected 11 fields in QGPSLOC, got {}",
            parts.len()
        ));
    }
    Ok(GpsFix {
        utc: parts[0].to_string(),
        latitude: parts[1].parse().map_err(|e| format!("bad lat: {e}"))?,
        longitude: parts[2].parse().map_err(|e| format!("bad lon: {e}"))?,
        hdop: parts[3].parse().map_err(|e| format!("bad hdop: {e}"))?,
        altitude: parts[4].parse().map_err(|e| format!("bad alt: {e}"))?,
        fix_type: parts[5].parse().map_err(|e| format!("bad fix: {e}"))?,
        course: parts[6].parse().map_err(|e| format!("bad cog: {e}"))?,
        speed_kmh: parts[7].parse().map_err(|e| format!("bad spkm: {e}"))?,
        date: parts[9].to_string(),
        satellites: parts[10]
            .trim()
            .parse()
            .map_err(|e| format!("bad nsat: {e}"))?,
        recorded_at: now_secs(),
    })
}

fn parse_csq(response: &str) -> Result<i32, String> {
    let line = response
        .lines()
        .find(|l| l.contains("+CSQ:"))
        .ok_or_else(|| format!("no +CSQ in response: {}", response.trim()))?;
    let data = line.split(':').nth(1).ok_or("malformed +CSQ line")?.trim();
    let rssi_raw: i32 = data
        .split(',')
        .next()
        .ok_or("no RSSI value")?
        .trim()
        .parse()
        .map_err(|e| format!("bad RSSI: {e}"))?;
    if rssi_raw == 99 {
        return Err("RSSI not detectable (99)".into());
    }
    Ok(-113 + 2 * rssi_raw)
}

#[derive(Default)]
struct QengData {
    rsrp: Option<i32>,
    rsrq: Option<i32>,
    sinr: Option<f64>,
    cell_id: Option<String>,
    state: Option<String>,
    duplex: Option<String>,
    plmn: Option<String>,
    pci: Option<u16>,
    earfcn: Option<u32>,
    freq_band: Option<u16>,
    ul_bw: Option<String>,
    dl_bw: Option<String>,
    tac: Option<String>,
    enodeb_id: Option<u32>,
    sector: Option<u8>,
}

fn parse_qeng(response: &str) -> QengData {
    fn parse_str(parts: &[&str], idx: usize) -> Option<String> {
        parts.get(idx).and_then(|s| {
            let s = s.trim_matches('"');
            if s.is_empty() || s == "-" {
                None
            } else {
                Some(s.to_string())
            }
        })
    }
    let Some(line) = response
        .lines()
        .find(|l| l.contains("+QENG:") && l.contains("LTE"))
    else {
        return QengData::default();
    };
    let Some(data) = line.split(':').nth(1) else {
        return QengData::default();
    };
    let parts: Vec<&str> = data.trim().split(',').map(str::trim).collect();
    if parts.len() < 17 {
        return QengData::default();
    }
    let cell_id = parse_str(&parts, 6);
    let (enodeb_id, sector) = cell_id.as_deref().map_or((None, None), decompose_cell_id);
    let plmn = match (parse_str(&parts, 4), parse_str(&parts, 5)) {
        (Some(m), Some(n)) => Some(format!("{m}{n}")),
        _ => None,
    };
    QengData {
        rsrp: parts.get(13).and_then(|s| s.parse::<i32>().ok()),
        rsrq: parts.get(14).and_then(|s| s.parse::<i32>().ok()),
        sinr: parts.get(16).and_then(|s| s.parse::<f64>().ok()),
        cell_id,
        state: parse_str(&parts, 1),
        duplex: parse_str(&parts, 3),
        plmn,
        pci: parts.get(7).and_then(|s| s.parse::<u16>().ok()),
        earfcn: parts.get(8).and_then(|s| s.parse::<u32>().ok()),
        freq_band: parts.get(9).and_then(|s| s.parse::<u16>().ok()),
        ul_bw: parts
            .get(10)
            .and_then(|s| decode_bandwidth(s))
            .map(String::from),
        dl_bw: parts
            .get(11)
            .and_then(|s| decode_bandwidth(s))
            .map(String::from),
        tac: parse_str(&parts, 12),
        enodeb_id,
        sector,
    }
}

fn parse_neighbour(response: &str) -> Vec<NeighborCell> {
    let mut neighbors = Vec::new();
    for line in response.lines() {
        if !line.contains("+QENG:") || !line.contains("neighbourcell") {
            continue;
        }
        let Some(data) = line.split(':').nth(1) else {
            continue;
        };
        let data = data.trim();
        let parts: Vec<&str> = data
            .split(',')
            .map(|s| s.trim().trim_matches('"'))
            .collect();
        let cell_type = if data.contains("neighbourcell intra") {
            "intra"
        } else if data.contains("neighbourcell inter") {
            "inter"
        } else {
            continue;
        };
        if parts.len() < 6 {
            continue;
        }
        let Some(earfcn) = parts.get(2).and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Some(pci) = parts.get(3).and_then(|s| s.parse::<u16>().ok()) else {
            continue;
        };
        neighbors.push(NeighborCell {
            earfcn,
            pci,
            rsrq: parts.get(4).and_then(|s| s.parse::<i32>().ok()),
            rsrp: parts.get(5).and_then(|s| s.parse::<i32>().ok()),
            rssi: parts.get(6).and_then(|s| s.parse::<i32>().ok()),
            sinr: parts.get(7).and_then(|s| s.parse::<f64>().ok()),
            cell_type: cell_type.to_string(),
        });
    }
    neighbors
}

fn parse_qnwinfo(response: &str) -> (Option<String>, Option<String>) {
    let Some(line) = response.lines().find(|l| l.contains("+QNWINFO:")) else {
        return (None, None);
    };
    let Some(data) = line.split(':').nth(1) else {
        return (None, None);
    };
    let parts: Vec<&str> = data
        .trim()
        .split(',')
        .map(|s| s.trim().trim_matches('"'))
        .collect();
    let technology = parts.first().map(|s| {
        if s.contains("LTE") {
            "LTE".to_string()
        } else {
            (*s).to_string()
        }
    });
    let band = parts.get(2).map(|s| {
        if let Some(rest) = s.strip_prefix("LTE BAND ") {
            format!("B{rest}")
        } else if let Some(rest) = s.strip_prefix("WCDMA BAND ") {
            format!("B{rest}")
        } else {
            (*s).to_string()
        }
    });
    (technology, band)
}

fn parse_cops(response: &str) -> Option<String> {
    let line = response.lines().find(|l| l.contains("+COPS:"))?;
    let data = line.split(':').nth(1)?.trim();
    let start = data.find('"')? + 1;
    let end = data[start..].find('"')? + start;
    let name = data[start..end].trim();
    if name.is_empty() {
        return None;
    }
    let words: Vec<&str> = name.split_whitespace().collect();
    if words.len() >= 2 && words[0].eq_ignore_ascii_case(words[1]) {
        Some(titlecase(words[0]))
    } else {
        Some(titlecase(name))
    }
}

fn parse_band_config(response: &str) -> Vec<u16> {
    let Some(line) = response
        .lines()
        .find(|l| l.contains("+QCFG:") && l.contains("band"))
    else {
        return Vec::new();
    };
    let Some(data) = line.split(':').nth(1) else {
        return Vec::new();
    };
    let parts: Vec<&str> = data.split(',').map(str::trim).collect();
    let Some(lte_hex) = parts.get(2) else {
        return Vec::new();
    };
    let lte_hex = lte_hex
        .trim_matches('"')
        .trim_start_matches("0x")
        .trim_start_matches("0X");
    let Ok(val) = u128::from_str_radix(lte_hex, 16) else {
        return Vec::new();
    };
    let mut bands = Vec::new();
    for bit in 0u16..128 {
        if val & (1u128 << bit) != 0 {
            bands.push(bit + 1);
        }
    }
    bands
}

fn parse_bandpri(response: &str) -> Option<u16> {
    let line = response
        .lines()
        .find(|l| l.contains("+QCFG:") && l.contains("bandpri"))?;
    let data = line.split(':').nth(1)?.trim();
    let parts: Vec<&str> = data.split(',').map(str::trim).collect();
    parts.get(1)?.trim_matches('"').parse::<u16>().ok()
}

fn bands_to_hex(bands: &[u16]) -> String {
    let mut mask: u128 = 0;
    for &band in bands {
        if (1..=128).contains(&band) {
            mask |= 1u128 << (band - 1);
        }
    }
    format!("0x{mask:X}")
}

fn compute_signal_bars(rsrp: Option<i32>, rssi_dbm: i32) -> u8 {
    if let Some(rsrp) = rsrp {
        match rsrp {
            -80..=i32::MAX => 5,
            -90..=-81 => 4,
            -100..=-91 => 3,
            -110..=-101 => 2,
            _ => 1,
        }
    } else {
        match rssi_dbm {
            -70..=i32::MAX => 5,
            -85..=-71 => 4,
            -100..=-86 => 3,
            -110..=-101 => 2,
            _ => 1,
        }
    }
}

fn decode_bandwidth(code: &str) -> Option<&'static str> {
    match code {
        "0" => Some("1.4"),
        "1" => Some("3"),
        "2" => Some("5"),
        "3" => Some("10"),
        "4" => Some("15"),
        "5" => Some("20"),
        _ => None,
    }
}

fn decompose_cell_id(hex_str: &str) -> (Option<u32>, Option<u8>) {
    let Ok(val) = u32::from_str_radix(hex_str, 16) else {
        return (None, None);
    };
    (Some(val >> 8), Some((val & 0xFF) as u8))
}

fn parse_simple_line(response: &str) -> Option<String> {
    response
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("AT") && *l != "OK" && !l.contains("ERROR"))
        .map(String::from)
}

fn parse_qccid(response: &str) -> Option<String> {
    let line = response.lines().find(|l| l.contains("+QCCID:"))?;
    let iccid = line.split(':').nth(1)?.trim();
    (!iccid.is_empty()).then(|| iccid.to_string())
}

fn parse_cimi(response: &str) -> Option<String> {
    response
        .lines()
        .map(str::trim)
        .find(|l| l.len() >= 6 && l.len() <= 15 && l.chars().all(|c| c.is_ascii_digit()))
        .map(String::from)
}

fn operator_name_from_plmn(plmn: &str) -> String {
    match plmn {
        "302720" => "Rogers",
        "302370" => "Fido",
        "302220" | "302221" => "Telus",
        "302610" => "Bell",
        "302490" => "Freedom",
        "302500" => "Videotron",
        "302780" => "SaskTel",
        other => other,
    }
    .to_string()
}

fn titlecase(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => {
            let upper: String = c.to_uppercase().collect();
            upper + &chars.as_str().to_lowercase()
        }
    }
}

fn effective_connection_state(raw: Option<String>, data_active: bool) -> Option<String> {
    if data_active {
        match raw.as_deref() {
            None | Some("NOCONN" | "LIMSRV" | "SEARCH") => Some("CONNECT".to_string()),
            _ => raw,
        }
    } else {
        raw
    }
}

fn interface_has_ipv4(host: &SctlCommsHostV1, iface: &str) -> bool {
    host.interface_has_ipv4
        .is_some_and(|cb| cb(host.ctx, slice(iface)))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
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

fn slice_to_string(slice: SctlCommsSlice) -> String {
    if slice.ptr.is_null() || slice.len == 0 {
        return String::new();
    }
    // SAFETY: ABI caller guarantees pointer+len valid for the call.
    let bytes = unsafe { std::slice::from_raw_parts(slice.ptr, slice.len) };
    String::from_utf8_lossy(bytes).into_owned()
}

fn write_out(out: SctlCommsMutSlice, bytes: &[u8]) -> i32 {
    if out.ptr.is_null() || out.len.is_null() {
        return SCTL_COMMS_ERR_INVALID;
    }
    if bytes.len() > out.cap {
        return SCTL_COMMS_ERR_BUFFER_TOO_SMALL;
    }
    // SAFETY: out is caller-provided and points to at least cap bytes.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), out.ptr, bytes.len());
        *out.len = bytes.len();
    }
    SCTL_COMMS_OK
}

// `as c_char` wraps on purpose: c_char is i8 on x86 but u8 on ARM targets, so
// the portable byte-reinterpreting cast must stay `as` (cast_signed() would
// only compile where c_char = i8).
#[allow(clippy::cast_possible_wrap)]
fn write_c_array(out: &mut [c_char; SCTL_COMMS_MAX_STR], value: &str) {
    for b in out.iter_mut() {
        *b = 0;
    }
    for (idx, byte) in value.bytes().take(SCTL_COMMS_MAX_STR - 1).enumerate() {
        out[idx] = byte as c_char;
    }
}

fn push_capabilities(out: &mut String, caps: u64) {
    let mut first = true;
    out.push('[');
    for (bit, name) in [
        (SCTL_COMMS_CAP_LOCATION_GNSS, capabilities::LOCATION_GNSS),
        (SCTL_COMMS_CAP_LINK_CELLULAR, capabilities::LINK_CELLULAR),
        (
            SCTL_COMMS_CAP_CELLULAR_BAND_CONTROL,
            capabilities::CELLULAR_BAND_CONTROL,
        ),
        (
            SCTL_COMMS_CAP_RECOVERY_USB_CYCLE,
            capabilities::RECOVERY_USB_CYCLE,
        ),
    ] {
        if caps & bit == 0 {
            continue;
        }
        if !first {
            out.push(',');
        }
        push_json_str(out, name);
        first = false;
    }
    out.push(']');
}

fn field_str(out: &mut String, name: &str, value: &str, comma: bool) {
    if comma {
        out.push(',');
    }
    push_json_str(out, name);
    out.push(':');
    push_json_str(out, value);
}

fn field_opt_str(out: &mut String, name: &str, value: Option<&str>, comma: bool) {
    if comma {
        out.push(',');
    }
    push_json_str(out, name);
    out.push(':');
    if let Some(value) = value {
        push_json_str(out, value);
    } else {
        out.push_str("null");
    }
}

fn field_opt_i32(out: &mut String, name: &str, value: Option<i32>, comma: bool) {
    if comma {
        out.push(',');
    }
    push_json_str(out, name);
    out.push(':');
    match value {
        Some(value) => out.push_str(&value.to_string()),
        None => out.push_str("null"),
    }
}

fn field_opt_u16(out: &mut String, name: &str, value: Option<u16>, comma: bool) {
    if comma {
        out.push(',');
    }
    push_json_str(out, name);
    out.push(':');
    match value {
        Some(value) => out.push_str(&value.to_string()),
        None => out.push_str("null"),
    }
}

fn field_opt_u32(out: &mut String, name: &str, value: Option<u32>, comma: bool) {
    if comma {
        out.push(',');
    }
    push_json_str(out, name);
    out.push(':');
    match value {
        Some(value) => out.push_str(&value.to_string()),
        None => out.push_str("null"),
    }
}

fn field_opt_u8(out: &mut String, name: &str, value: Option<u8>, comma: bool) {
    if comma {
        out.push(',');
    }
    push_json_str(out, name);
    out.push(':');
    match value {
        Some(value) => out.push_str(&value.to_string()),
        None => out.push_str("null"),
    }
}

fn field_opt_f64(out: &mut String, name: &str, value: Option<f64>, comma: bool) {
    if comma {
        out.push(',');
    }
    push_json_str(out, name);
    out.push(':');
    match value {
        Some(value) if value.is_finite() => {
            let _ = write!(out, "{value:.1}");
        }
        _ => out.push_str("null"),
    }
}

fn push_json_str(out: &mut String, value: &str) {
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn push_gps_fix(out: &mut String, fix: &GpsFix, full: bool) {
    out.push('{');
    out.push_str("\"latitude\":");
    out.push_str(&fix.latitude.to_string());
    out.push_str(",\"longitude\":");
    out.push_str(&fix.longitude.to_string());
    out.push_str(",\"altitude\":");
    out.push_str(&fix.altitude.to_string());
    out.push_str(",\"speed_kmh\":");
    out.push_str(&fix.speed_kmh.to_string());
    if full {
        out.push_str(",\"course\":");
        out.push_str(&fix.course.to_string());
        out.push_str(",\"hdop\":");
        out.push_str(&fix.hdop.to_string());
    }
    out.push_str(",\"satellites\":");
    out.push_str(&fix.satellites.to_string());
    if full {
        field_str(out, "utc", &fix.utc, true);
        field_str(out, "date", &fix.date, true);
        out.push_str(",\"fix_type\":");
        out.push_str(&fix.fix_type.to_string());
    }
    out.push_str(",\"recorded_at\":");
    out.push_str(&fix.recorded_at.to_string());
    out.push('}');
}

fn push_modem_info(out: &mut String, modem: &ModemInfo) {
    out.push('{');
    field_opt_str(out, "model", modem.model.as_deref(), false);
    field_opt_str(out, "firmware", modem.firmware.as_deref(), true);
    field_opt_str(out, "imei", modem.imei.as_deref(), true);
    field_opt_str(out, "iccid", modem.iccid.as_deref(), true);
    field_opt_str(out, "imsi", modem.imsi.as_deref(), true);
    out.push('}');
}

fn push_band_config(out: &mut String, cfg: &BandConfig) {
    out.push('{');
    out.push_str("\"enabled_bands\":[");
    for (idx, band) in cfg.enabled_bands.iter().enumerate() {
        if idx > 0 {
            out.push(',');
        }
        out.push_str(&band.to_string());
    }
    out.push(']');
    field_opt_u16(out, "priority_band", cfg.priority_band, true);
    out.push('}');
}

fn push_lte_signal(out: &mut String, sig: &LteSignal) {
    out.push('{');
    out.push_str("\"rssi_dbm\":");
    out.push_str(&sig.rssi_dbm.to_string());
    field_opt_i32(out, "rsrp", sig.rsrp, true);
    field_opt_i32(out, "rsrq", sig.rsrq, true);
    field_opt_f64(out, "sinr", sig.sinr, true);
    field_opt_str(out, "band", sig.band.as_deref(), true);
    field_opt_str(out, "operator", sig.operator.as_deref(), true);
    field_opt_str(out, "technology", sig.technology.as_deref(), true);
    field_opt_str(out, "cell_id", sig.cell_id.as_deref(), true);
    field_opt_u16(out, "pci", sig.pci, true);
    field_opt_u32(out, "earfcn", sig.earfcn, true);
    field_opt_u16(out, "freq_band", sig.freq_band, true);
    field_opt_str(out, "tac", sig.tac.as_deref(), true);
    field_opt_str(out, "plmn", sig.plmn.as_deref(), true);
    field_opt_u32(out, "enodeb_id", sig.enodeb_id, true);
    field_opt_u8(out, "sector", sig.sector, true);
    field_opt_str(out, "ul_bw_mhz", sig.ul_bw_mhz.as_deref(), true);
    field_opt_str(out, "dl_bw_mhz", sig.dl_bw_mhz.as_deref(), true);
    field_opt_str(
        out,
        "connection_state",
        sig.connection_state.as_deref(),
        true,
    );
    field_opt_str(out, "duplex", sig.duplex.as_deref(), true);
    out.push_str(",\"neighbors\":[");
    for (idx, n) in sig.neighbors.iter().enumerate() {
        if idx > 0 {
            out.push(',');
        }
        out.push('{');
        out.push_str("\"earfcn\":");
        out.push_str(&n.earfcn.to_string());
        out.push_str(",\"pci\":");
        out.push_str(&n.pci.to_string());
        field_opt_i32(out, "rsrp", n.rsrp, true);
        field_opt_i32(out, "rsrq", n.rsrq, true);
        field_opt_i32(out, "rssi", n.rssi, true);
        field_opt_f64(out, "sinr", n.sinr, true);
        field_str(out, "cell_type", &n.cell_type, true);
        out.push('}');
    }
    out.push(']');
    out.push_str(",\"band_config\":");
    if let Some(ref cfg) = sig.band_config {
        push_band_config(out, cfg);
    } else {
        out.push_str("null");
    }
    out.push_str(",\"signal_bars\":");
    out.push_str(&sig.signal_bars.to_string());
    out.push_str(",\"recorded_at\":");
    out.push_str(&sig.recorded_at.to_string());
    out.push('}');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_band_config() {
        assert_eq!(
            parse_band_config("+QCFG: \"band\",0x260,0x808,0x0\r\nOK\r\n"),
            vec![4, 12]
        );
    }

    #[test]
    fn bands_to_hex_sets_band_bits() {
        assert_eq!(bands_to_hex(&[4, 12]), "0x808");
    }

    #[test]
    fn parses_gps_fix() {
        let fix = parse_qgpsloc(
            "+QGPSLOC: 153233.0,45.5,-73.5,1.2,47.0,3,270.5,0.0,0.0,260226,08\r\nOK\r\n",
        )
        .unwrap();
        assert_eq!(fix.satellites, 8);
        assert_eq!(fix.fix_type, 3);
    }

    #[test]
    fn c_abi_open_and_poll_lte_through_fake_host() {
        extern "C" fn fake_now_ms(_ctx: *mut c_void) -> u64 {
            1_700_000_000_000
        }

        extern "C" fn fake_iface(_ctx: *mut c_void, _iface: SctlCommsSlice) -> bool {
            true
        }

        extern "C" fn fake_at(
            _ctx: *mut c_void,
            command: SctlCommsSlice,
            _timeout_ms: u64,
            out: SctlCommsMutSlice,
        ) -> i32 {
            let command = slice_to_string(command);
            let response = match command.as_str() {
                "AT+CGMM" => "EC25-AF\r\nOK\r\n",
                "AT+CGMR" => "EC25AFFDR07A10M4G\r\nOK\r\n",
                "AT+GSN" => "868748070457085\r\nOK\r\n",
                "AT+QCCID" => "+QCCID: 89014103211118510720\r\nOK\r\n",
                "AT+CIMI" => "302720123456789\r\nOK\r\n",
                "AT+CSQ" => "+CSQ: 15,99\r\nOK\r\n",
                "AT+QENG=\"servingcell\"" => {
                    "+QENG: \"servingcell\",\"NOCONN\",\"LTE\",\"FDD\",302,720,1A2B3C,42,2100,4,3,3,1A2B,-95,-10,-65,12,0\r\nOK\r\n"
                }
                "AT+QNWINFO" => "+QNWINFO: \"FDD LTE\",\"302720\",\"LTE BAND 4\",2100\r\nOK\r\n",
                "AT+COPS?" => "+COPS: 0,0,\"ROGERS ROGERS\",7\r\nOK\r\n",
                // "AT+QGPS=1" and "AT+QENG=\"neighbourcell\"" fall through to
                // the wildcard's plain OK.
                "AT+QCFG=\"band\"" => "+QCFG: \"band\",0x260,0x808,0x0\r\nOK\r\n",
                "AT+QCFG=\"bandpri\"" => "+QCFG: \"bandpri\",4\r\nOK\r\n",
                _ => "OK\r\n",
            };
            write_out(out, response.as_bytes())
        }

        let host = SctlCommsHostV1 {
            abi_version: SCTL_COMMS_ABI_VERSION,
            ctx: std::ptr::null_mut(),
            log: None,
            now_ms: Some(fake_now_ms),
            at_command: Some(fake_at),
            interface_has_ipv4: Some(fake_iface),
            speed_test: None,
            usb_cycle: None,
        };
        let mut api = SctlCommsPluginV1::default();
        let rc = unsafe { sctl_comms_plugin_init_v1(&raw const host, &raw mut api) };
        assert_eq!(rc, SCTL_COMMS_OK);

        let provider = "quectel-at";
        let device = "/dev/ttyUSB2";
        let data_dir = "/tmp";
        let iface = "wwan0";
        let open_config = SctlCommsOpenConfig {
            provider: slice(provider),
            device: slice(device),
            data_dir: slice(data_dir),
            gps_enabled: true,
            gps_auto_enable: true,
            gps_history_size: 8,
            lte_enabled: true,
            lte_watchdog: false,
            lte_interface: slice(iface),
            speed_test_url: SctlCommsSlice::empty(),
            speed_test_upload_url: SctlCommsSlice::empty(),
            tunnel_url: SctlCommsSlice::empty(),
        };

        let mut buf = vec![0u8; 16 * 1024];
        let mut len = 0usize;
        let open = api.open.unwrap();
        assert_eq!(
            open(
                api.plugin_ctx,
                &raw const open_config,
                mut_slice(&mut buf, &mut len)
            ),
            SCTL_COMMS_OK
        );
        let opened = String::from_utf8_lossy(&buf[..len]);
        assert!(opened.contains("\"status\":\"ok\""));

        let mut len = 0usize;
        let poll = api.poll_link.unwrap();
        let params = SctlCommsLinkPollParams {
            refresh: false,
            tunnel_connected: true,
        };
        assert_eq!(
            poll(
                api.plugin_ctx,
                &raw const params,
                mut_slice(&mut buf, &mut len)
            ),
            SCTL_COMMS_OK
        );
        let lte = String::from_utf8_lossy(&buf[..len]);
        assert!(lte.contains("\"rssi_dbm\":-83"));
        assert!(lte.contains("\"band\":\"B4\""));
        assert!(lte.contains("\"operator\":\"Rogers\""));
        assert!(lte.contains("\"model\":\"EC25-AF\""));

        api.destroy.unwrap()(api.plugin_ctx);
    }
}
