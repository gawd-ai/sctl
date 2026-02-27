//! LTE signal quality monitoring via Quectel modem AT commands.
//!
//! When `[lte]` is present in the config, a background poller queries the modem
//! for signal strength, serving cell info, network info, and operator name
//! at the configured interval, storing results in [`LteState`].
//!
//! Static modem identity (IMEI, model, firmware, ICCID) is read once at startup.

use std::sync::Arc;

use serde::Serialize;
use tokio::sync::{broadcast, Mutex};
use tracing::{debug, info};

use crate::config::LteConfig;
use crate::modem::at_command;

/// Static modem identity — read once at startup.
#[derive(Debug, Clone, Serialize)]
pub struct ModemInfo {
    /// Modem model (e.g. "EC25", from AT+CGMM).
    pub model: Option<String>,
    /// Firmware version (e.g. "EC25AFFAR07A14M4G", from AT+CGMR).
    pub firmware: Option<String>,
    /// IMEI (from AT+GSN).
    pub imei: Option<String>,
    /// SIM ICCID (from AT+QCCID).
    pub iccid: Option<String>,
}

/// LTE signal quality snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct LteSignal {
    /// RSSI in dBm (from AT+CSQ).
    pub rssi_dbm: i32,
    /// Reference Signal Received Power (from AT+QENG).
    pub rsrp: Option<i32>,
    /// Reference Signal Received Quality (from AT+QENG).
    pub rsrq: Option<i32>,
    /// Signal-to-Interference-plus-Noise Ratio (from AT+QENG).
    pub sinr: Option<f64>,
    /// LTE band (e.g. "B4", from AT+QNWINFO).
    pub band: Option<String>,
    /// Network operator name (from AT+COPS?).
    pub operator: Option<String>,
    /// Access technology (e.g. "LTE", from AT+QNWINFO).
    pub technology: Option<String>,
    /// Cell ID (hex, from AT+QENG).
    pub cell_id: Option<String>,
    /// Signal quality as 1-5 bars, derived from RSRP (or RSSI fallback).
    pub signal_bars: u8,
    /// When this reading was recorded (epoch seconds).
    pub recorded_at: u64,
}

/// Shared LTE state updated by the background poller.
#[derive(Default)]
pub struct LteState {
    pub modem: Option<ModemInfo>,
    pub signal: Option<LteSignal>,
    pub errors_total: u64,
    pub last_error: Option<String>,
}

impl LteState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// Compute signal bars (1-5) from RSRP, falling back to RSSI.
fn compute_signal_bars(rsrp: Option<i32>, rssi_dbm: i32) -> u8 {
    if let Some(rsrp) = rsrp {
        return match rsrp {
            _ if rsrp >= -80 => 5,
            _ if rsrp >= -90 => 4,
            _ if rsrp >= -100 => 3,
            _ if rsrp >= -110 => 2,
            _ => 1,
        };
    }
    match rssi_dbm {
        _ if rssi_dbm >= -65 => 5,
        _ if rssi_dbm >= -75 => 4,
        _ if rssi_dbm >= -85 => 3,
        _ if rssi_dbm >= -95 => 2,
        _ => 1,
    }
}

// ── Parsers ──────────────────────────────────────────────────────────

/// Parse `AT+CSQ` response → RSSI in dBm.
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

/// Parsed serving cell data from AT+QENG.
struct QengData {
    rsrp: Option<i32>,
    rsrq: Option<i32>,
    sinr: Option<f64>,
    cell_id: Option<String>,
}

/// Parse `AT+QENG="servingcell"` → RSRP, RSRQ, SINR, cell_id.
fn parse_qeng(response: &str) -> QengData {
    let Some(line) = response
        .lines()
        .find(|l| l.contains("+QENG:") && l.contains("LTE"))
    else {
        return QengData {
            rsrp: None,
            rsrq: None,
            sinr: None,
            cell_id: None,
        };
    };

    let data = match line.split(':').nth(1) {
        Some(d) => d.trim(),
        None => {
            return QengData {
                rsrp: None,
                rsrq: None,
                sinr: None,
                cell_id: None,
            }
        }
    };

    let parts: Vec<&str> = data.split(',').map(str::trim).collect();

    // LTE FDD layout:
    // 0:"servingcell" 1:"NOCONN" 2:"LTE" 3:"FDD" 4:mcc 5:mnc 6:cellid 7:pcid
    // 8:earfcn 9:freq_band 10:ul_bw 11:dl_bw 12:tac 13:rsrp 14:rsrq 15:rssi 16:sinr 17:srxlev
    if parts.len() < 17 {
        return QengData {
            rsrp: None,
            rsrq: None,
            sinr: None,
            cell_id: None,
        };
    }

    let cell_id = parts.get(6).and_then(|s| {
        let s = s.trim_matches('"');
        if s.is_empty() || s == "-" {
            None
        } else {
            Some(s.to_string())
        }
    });

    QengData {
        rsrp: parts.get(13).and_then(|s| s.parse::<i32>().ok()),
        rsrq: parts.get(14).and_then(|s| s.parse::<i32>().ok()),
        sinr: parts.get(16).and_then(|s| s.parse::<f64>().ok()),
        cell_id,
    }
}

/// Parse `AT+QNWINFO` → (technology, band). Operator comes from AT+COPS? instead.
fn parse_qnwinfo(response: &str) -> (Option<String>, Option<String>) {
    let Some(line) = response.lines().find(|l| l.contains("+QNWINFO:")) else {
        return (None, None);
    };

    let data = match line.split(':').nth(1) {
        Some(d) => d.trim(),
        None => return (None, None),
    };

    let parts: Vec<&str> = data
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

/// Parse `AT+COPS?` → operator name.
///
/// Response: `+COPS: 0,0,"ROGERS ROGERS",7`
fn parse_cops(response: &str) -> Option<String> {
    let line = response.lines().find(|l| l.contains("+COPS:"))?;
    let data = line.split(':').nth(1)?.trim();
    // Find the quoted operator name
    let start = data.find('"')? + 1;
    let end = data[start..].find('"')? + start;
    let name = data[start..end].trim();
    if name.is_empty() {
        return None;
    }
    // Clean up "ROGERS ROGERS" → "Rogers"
    let words: Vec<&str> = name.split_whitespace().collect();
    if words.len() >= 2 && words[0].eq_ignore_ascii_case(words[1]) {
        // Deduplicate "ROGERS ROGERS" → "Rogers"
        Some(titlecase(words[0]))
    } else {
        Some(titlecase(name))
    }
}

/// Title-case a string: "ROGERS" → "Rogers".
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

/// Parse a simple one-line AT response (e.g. `AT+GSN` → `866834049460285`).
fn parse_simple_line(response: &str) -> Option<String> {
    response
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("AT") && *l != "OK" && !l.contains("ERROR"))
        .map(String::from)
}

/// Parse `AT+QCCID` → ICCID.
fn parse_qccid(response: &str) -> Option<String> {
    let line = response.lines().find(|l| l.contains("+QCCID:"))?;
    let iccid = line.split(':').nth(1)?.trim();
    if iccid.is_empty() {
        None
    } else {
        Some(iccid.to_string())
    }
}

/// Read static modem identity (IMEI, model, firmware, ICCID).
async fn read_modem_info(shell: &str, device: &str) -> ModemInfo {
    let model = match at_command(shell, device, "AT+CGMM").await {
        Ok(resp) => parse_simple_line(&resp),
        Err(_) => None,
    };
    let firmware = match at_command(shell, device, "AT+CGMR").await {
        Ok(resp) => parse_simple_line(&resp),
        Err(_) => None,
    };
    let imei = match at_command(shell, device, "AT+GSN").await {
        Ok(resp) => parse_simple_line(&resp),
        Err(_) => None,
    };
    let iccid = match at_command(shell, device, "AT+QCCID").await {
        Ok(resp) => parse_qccid(&resp),
        Err(_) => None,
    };

    ModemInfo {
        model,
        firmware,
        imei,
        iccid,
    }
}

/// Spawn the background LTE signal poller. Returns a `JoinHandle` for abort on shutdown.
pub fn spawn_lte_poller(
    config: LteConfig,
    shell: String,
    lte_state: Arc<Mutex<LteState>>,
    session_events: broadcast::Sender<serde_json::Value>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let shell = &shell;
        let device = &config.device;
        let interval = tokio::time::Duration::from_secs(config.poll_interval_secs);

        // Read static modem info once at startup
        let modem_info = read_modem_info(shell, device).await;
        info!(
            "LTE modem: {} {} IMEI={}",
            modem_info.model.as_deref().unwrap_or("?"),
            modem_info.firmware.as_deref().unwrap_or("?"),
            modem_info.imei.as_deref().unwrap_or("?"),
        );
        lte_state.lock().await.modem = Some(modem_info);

        let mut ticker = tokio::time::interval(interval);
        // First tick is immediate — get an initial signal reading
        ticker.tick().await;

        loop {
            // 1. AT+CSQ for RSSI
            let rssi_result = match at_command(shell, device, "AT+CSQ").await {
                Ok(resp) => parse_csq(&resp),
                Err(e) => Err(e),
            };

            let rssi_dbm = match rssi_result {
                Ok(v) => v,
                Err(e) => {
                    debug!("LTE: CSQ failed: {e}");
                    let mut state = lte_state.lock().await;
                    state.errors_total += 1;
                    state.last_error = Some(e);
                    ticker.tick().await;
                    continue;
                }
            };

            // 2. AT+QENG for RSRP/RSRQ/SINR/cell_id
            let qeng = match at_command(shell, device, "AT+QENG=\"servingcell\"").await {
                Ok(resp) => parse_qeng(&resp),
                Err(e) => {
                    debug!("LTE: QENG failed: {e}");
                    QengData {
                        rsrp: None,
                        rsrq: None,
                        sinr: None,
                        cell_id: None,
                    }
                }
            };

            // 3. AT+QNWINFO for band/technology
            let (technology, band) = match at_command(shell, device, "AT+QNWINFO").await {
                Ok(resp) => parse_qnwinfo(&resp),
                Err(e) => {
                    debug!("LTE: QNWINFO failed: {e}");
                    (None, None)
                }
            };

            // 4. AT+COPS? for operator name
            let operator = match at_command(shell, device, "AT+COPS?").await {
                Ok(resp) => parse_cops(&resp),
                Err(e) => {
                    debug!("LTE: COPS failed: {e}");
                    None
                }
            };

            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            let signal_bars = compute_signal_bars(qeng.rsrp, rssi_dbm);

            let signal = LteSignal {
                rssi_dbm,
                rsrp: qeng.rsrp,
                rsrq: qeng.rsrq,
                sinr: qeng.sinr,
                band,
                operator,
                technology,
                cell_id: qeng.cell_id,
                signal_bars,
                recorded_at: now,
            };

            debug!(
                "LTE: RSSI={} RSRP={:?} SINR={:?} bars={} band={:?} op={:?}",
                signal.rssi_dbm,
                signal.rsrp,
                signal.sinr,
                signal.signal_bars,
                signal.band,
                signal.operator
            );

            let _ = session_events.send(serde_json::json!({
                "type": "lte.signal",
                "rssi_dbm": signal.rssi_dbm,
                "signal_bars": signal.signal_bars,
                "band": signal.band,
                "operator": signal.operator,
            }));

            lte_state.lock().await.signal = Some(signal);

            ticker.tick().await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_csq_valid() {
        let response = "+CSQ: 15,99\r\nOK";
        assert_eq!(parse_csq(response).unwrap(), -83);
    }

    #[test]
    fn test_parse_csq_not_detectable() {
        assert!(parse_csq("+CSQ: 99,99\r\nOK").is_err());
    }

    #[test]
    fn test_parse_csq_no_response() {
        assert!(parse_csq("ERROR\r\n").is_err());
    }

    #[test]
    fn test_parse_qeng_lte() {
        let response = "+QENG: \"servingcell\",\"NOCONN\",\"LTE\",\"FDD\",302,720,101A901,266,2050,4,5,5,61E4,-102,-11,-70,10,-\r\nOK";
        let q = parse_qeng(response);
        assert_eq!(q.rsrp, Some(-102));
        assert_eq!(q.rsrq, Some(-11));
        assert_eq!(q.sinr, Some(10.0));
        assert_eq!(q.cell_id.as_deref(), Some("101A901"));
    }

    #[test]
    fn test_parse_qeng_no_lte() {
        let q = parse_qeng("OK\r\n");
        assert!(q.rsrp.is_none());
    }

    #[test]
    fn test_parse_qnwinfo() {
        let response = "+QNWINFO: \"FDD LTE\",\"302720\",\"LTE BAND 4\",2050\r\nOK";
        let (tech, band) = parse_qnwinfo(response);
        assert_eq!(tech.as_deref(), Some("LTE"));
        assert_eq!(band.as_deref(), Some("B4"));
    }

    #[test]
    fn test_parse_qnwinfo_wcdma() {
        let response = "+QNWINFO: \"WCDMA\",\"Rogers\",\"WCDMA BAND 2\",412\r\nOK";
        let (tech, band) = parse_qnwinfo(response);
        assert_eq!(tech.as_deref(), Some("WCDMA"));
        assert_eq!(band.as_deref(), Some("B2"));
    }

    #[test]
    fn test_parse_cops_rogers() {
        let response = "\r\n+COPS: 0,0,\"ROGERS ROGERS\",7\r\n\r\nOK\r\n";
        assert_eq!(parse_cops(response).as_deref(), Some("Rogers"));
    }

    #[test]
    fn test_parse_cops_normal() {
        let response = "+COPS: 0,0,\"T-Mobile\",7\r\nOK";
        assert_eq!(parse_cops(response).as_deref(), Some("T-mobile"));
    }

    #[test]
    fn test_parse_cops_no_match() {
        assert!(parse_cops("OK\r\n").is_none());
    }

    #[test]
    fn test_parse_simple_line_imei() {
        let response = "\r\n866834049460285\r\n\r\nOK\r\n";
        assert_eq!(
            parse_simple_line(response).as_deref(),
            Some("866834049460285")
        );
    }

    #[test]
    fn test_parse_simple_line_model() {
        let response = "\r\nEC25\r\n\r\nOK\r\n";
        assert_eq!(parse_simple_line(response).as_deref(), Some("EC25"));
    }

    #[test]
    fn test_parse_qccid() {
        let response = "\r\n+QCCID: 89302720554115293655\r\n\r\nOK\r\n";
        assert_eq!(
            parse_qccid(response).as_deref(),
            Some("89302720554115293655")
        );
    }

    #[test]
    fn test_signal_bars() {
        assert_eq!(compute_signal_bars(Some(-75), -50), 5);
        assert_eq!(compute_signal_bars(Some(-85), -50), 4);
        assert_eq!(compute_signal_bars(Some(-95), -50), 3);
        assert_eq!(compute_signal_bars(Some(-105), -50), 2);
        assert_eq!(compute_signal_bars(Some(-115), -50), 1);
        assert_eq!(compute_signal_bars(None, -60), 5);
        assert_eq!(compute_signal_bars(None, -90), 2);
    }
}
