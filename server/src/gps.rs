//! GPS location tracking via Quectel modem AT commands.
//!
//! When `[gps]` is present in the config, a background poller sends
//! `AT+QGPSLOC=2` at the configured interval, parses the fix, and stores
//! it in [`GpsState`]. The GNSS engine is auto-enabled on startup
//! (`AT+QGPS=1`) and disabled on shutdown (`AT+QGPSEND`).

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;
use tokio::sync::{broadcast, Mutex};
use tracing::{debug, info, warn};

use crate::config::GpsConfig;

/// A single GPS fix from `AT+QGPSLOC=2`.
#[derive(Debug, Clone, Serialize)]
pub struct GpsFix {
    pub latitude: f64,
    pub longitude: f64,
    pub altitude: f64,
    /// Speed in km/h.
    pub speed_kmh: f64,
    /// Course over ground in degrees.
    pub course: f64,
    /// Horizontal dilution of precision.
    pub hdop: f64,
    /// Number of satellites used.
    pub satellites: u32,
    /// UTC time string from modem (HHmmss.s).
    pub utc: String,
    /// UTC date string from modem (ddMMyy).
    pub date: String,
    /// Fix type: 2 = 2D, 3 = 3D.
    pub fix_type: u32,
    /// When this fix was recorded (epoch seconds).
    pub recorded_at: u64,
}

/// Current GPS status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GpsStatus {
    /// GNSS engine active, have a fix.
    Active,
    /// GNSS engine active, searching for satellites.
    Searching,
    /// AT command error or modem issue.
    Error,
    /// GNSS not enabled / module disabled.
    Disabled,
}

/// Shared GPS state updated by the background poller.
pub struct GpsState {
    pub status: GpsStatus,
    pub last_fix: Option<GpsFix>,
    pub history: VecDeque<GpsFix>,
    pub history_max: usize,
    pub fixes_total: u64,
    pub errors_total: u64,
    pub last_error: Option<String>,
    pub last_fix_at: Option<Instant>,
}

impl GpsState {
    #[must_use]
    pub fn new(history_size: usize) -> Self {
        Self {
            status: GpsStatus::Disabled,
            last_fix: None,
            history: VecDeque::with_capacity(history_size),
            history_max: history_size,
            fixes_total: 0,
            errors_total: 0,
            last_error: None,
            last_fix_at: None,
        }
    }

    fn push_fix(&mut self, fix: GpsFix) {
        if self.history.len() >= self.history_max {
            self.history.pop_front();
        }
        self.last_fix_at = Some(Instant::now());
        self.history.push_back(fix.clone());
        self.last_fix = Some(fix);
        self.fixes_total += 1;
        self.status = GpsStatus::Active;
    }

    fn set_searching(&mut self) {
        self.status = GpsStatus::Searching;
    }

    fn set_error(&mut self, msg: String) {
        self.status = GpsStatus::Error;
        self.errors_total += 1;
        self.last_error = Some(msg);
    }
}

use crate::modem::at_command;

/// Parse `AT+QGPSLOC=2` response (decimal degrees format).
///
/// Response format:
/// ```text
/// +QGPSLOC: <UTC>,<lat>,<lon>,<hdop>,<alt>,<fix>,<cog>,<spkm>,<spkn>,<date>,<nsat>
/// ```
///
/// Returns `Ok(GpsFix)` on success, `Err("searching")` for CME ERROR 516 (no fix),
/// or `Err(description)` for other failures.
fn parse_qgpsloc(response: &str) -> Result<GpsFix, String> {
    // Look for valid GPS data first — stale buffer data may contain ERROR
    // alongside a valid response, so prioritize +QGPSLOC: over ERROR.
    let line = response.lines().find(|l| l.contains("+QGPSLOC:"));

    if line.is_none() {
        // No GPS data — check error codes
        if response.contains("516") && response.contains("ERROR") {
            return Err("searching".into());
        }
        if response.contains("ERROR") {
            return Err(format!("modem error: {}", response.trim()));
        }
        return Err(format!("no +QGPSLOC in response: {}", response.trim()));
    }

    let line = line.unwrap();

    let data = line
        .split(':')
        .nth(1)
        .ok_or("malformed +QGPSLOC line")?
        .trim();

    let parts: Vec<&str> = data.split(',').collect();
    if parts.len() < 11 {
        return Err(format!(
            "expected 11 fields in QGPSLOC, got {}: {data}",
            parts.len()
        ));
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    Ok(GpsFix {
        utc: parts[0].to_string(),
        latitude: parts[1].parse().map_err(|e| format!("bad lat: {e}"))?,
        longitude: parts[2].parse().map_err(|e| format!("bad lon: {e}"))?,
        hdop: parts[3].parse().map_err(|e| format!("bad hdop: {e}"))?,
        altitude: parts[4].parse().map_err(|e| format!("bad alt: {e}"))?,
        fix_type: parts[5].parse().map_err(|e| format!("bad fix: {e}"))?,
        course: parts[6].parse().map_err(|e| format!("bad cog: {e}"))?,
        speed_kmh: parts[7].parse().map_err(|e| format!("bad spkm: {e}"))?,
        // parts[8] = speed in knots, skip
        date: parts[9].to_string(),
        satellites: parts[10]
            .trim()
            .parse()
            .map_err(|e| format!("bad nsat: {e}"))?,
        recorded_at: now,
    })
}

/// Spawn the background GPS poller. Returns a `JoinHandle` for abort on shutdown.
pub fn spawn_gps_poller(
    config: GpsConfig,
    shell: String,
    gps_state: Arc<Mutex<GpsState>>,
    session_events: broadcast::Sender<serde_json::Value>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let shell = &shell;
        let device = &config.device;
        let interval = tokio::time::Duration::from_secs(config.poll_interval_secs);

        // Auto-enable GNSS engine (retry up to 3 times on failure)
        if config.auto_enable {
            info!("GPS: enabling GNSS engine on {device}");
            let mut enabled = false;
            for attempt in 1..=3 {
                match at_command(shell, device, "AT+QGPS=1").await {
                    Ok(resp) => {
                        if resp.contains("OK") || resp.contains("Session is ongoing") {
                            info!("GPS: GNSS engine enabled (attempt {attempt})");
                            gps_state.lock().await.status = GpsStatus::Searching;
                            enabled = true;
                            break;
                        } else if resp.contains("ERROR") {
                            warn!(
                                "GPS: failed to enable GNSS (attempt {attempt}): {}",
                                resp.trim()
                            );
                        } else {
                            debug!(
                                "GPS: AT+QGPS=1 response (attempt {attempt}): {}",
                                resp.trim()
                            );
                            gps_state.lock().await.status = GpsStatus::Searching;
                            enabled = true;
                            break;
                        }
                    }
                    Err(e) => {
                        warn!("GPS: failed to send AT+QGPS=1 (attempt {attempt}): {e}");
                        if attempt == 3 {
                            gps_state.lock().await.set_error(e);
                        }
                    }
                }
                if attempt < 3 {
                    tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
                }
            }
            if !enabled {
                warn!("GPS: failed to enable GNSS after 3 attempts");
            }
        }

        let mut ticker = tokio::time::interval(interval);
        // Skip the first immediate tick (we just enabled GNSS, give it a moment)
        ticker.tick().await;

        let mut consecutive_errors: u32 = 0;

        loop {
            ticker.tick().await;

            match at_command(shell, device, "AT+QGPSLOC=2").await {
                Ok(resp) => match parse_qgpsloc(&resp) {
                    Ok(fix) => {
                        debug!(
                            "GPS: fix {:.6},{:.6} alt={:.0}m sats={} hdop={:.1}",
                            fix.latitude, fix.longitude, fix.altitude, fix.satellites, fix.hdop
                        );
                        consecutive_errors = 0;
                        // Broadcast GPS fix event
                        let _ = session_events.send(serde_json::json!({
                            "type": "gps.fix",
                            "latitude": fix.latitude,
                            "longitude": fix.longitude,
                            "altitude": fix.altitude,
                            "satellites": fix.satellites,
                        }));
                        gps_state.lock().await.push_fix(fix);
                    }
                    Err(ref e) if e == "searching" => {
                        debug!("GPS: searching for satellites...");
                        consecutive_errors = 0;
                        gps_state.lock().await.set_searching();
                    }
                    Err(e) => {
                        consecutive_errors += 1;
                        warn!("GPS: parse error ({consecutive_errors}/3): {e}");
                        gps_state.lock().await.set_error(e);
                    }
                },
                Err(e) => {
                    consecutive_errors += 1;
                    warn!("GPS: AT command failed ({consecutive_errors}/3): {e}");
                    gps_state.lock().await.set_error(e);
                }
            }

            // Re-enable GNSS after 3 consecutive non-searching errors
            if consecutive_errors >= 3 {
                warn!("GPS: {consecutive_errors} consecutive errors, attempting AT+QGPS=1");
                consecutive_errors = 0;
                match at_command(shell, device, "AT+QGPS=1").await {
                    Ok(resp) => {
                        if resp.contains("OK") || resp.contains("Session is ongoing") {
                            info!("GPS: GNSS re-enabled successfully");
                            gps_state.lock().await.status = GpsStatus::Searching;
                        } else {
                            warn!("GPS: GNSS re-enable response: {}", resp.trim());
                        }
                    }
                    Err(e) => warn!("GPS: failed to re-enable GNSS: {e}"),
                }
            }
        }
    })
}

/// Disable the GNSS engine (called on shutdown).
pub async fn disable_gnss(config: &GpsConfig, shell: &str) {
    info!("GPS: disabling GNSS engine on {}", config.device);
    match at_command(shell, &config.device, "AT+QGPSEND").await {
        Ok(resp) => {
            if resp.contains("OK") {
                info!("GPS: GNSS engine disabled");
            } else {
                debug!("GPS: AT+QGPSEND response: {}", resp.trim());
            }
        }
        Err(e) => {
            warn!("GPS: failed to disable GNSS: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_qgpsloc_valid() {
        let response =
            "+QGPSLOC: 153233.0,45.50200,-73.56700,1.2,47.0,3,270.5,0.0,0.0,260226,08\r\nOK";
        let fix = parse_qgpsloc(response).unwrap();
        assert!((fix.latitude - 45.502).abs() < 0.001);
        assert!((fix.longitude - (-73.567)).abs() < 0.001);
        assert!((fix.altitude - 47.0).abs() < 0.1);
        assert_eq!(fix.satellites, 8);
        assert_eq!(fix.fix_type, 3);
        assert_eq!(fix.date, "260226");
    }

    #[test]
    fn test_parse_qgpsloc_no_fix() {
        let response = "+CME ERROR: 516\r\n";
        let err = parse_qgpsloc(response).unwrap_err();
        assert_eq!(err, "searching");
    }

    #[test]
    fn test_parse_qgpsloc_other_error() {
        let response = "+CME ERROR: 505\r\n";
        let err = parse_qgpsloc(response).unwrap_err();
        assert!(err.contains("modem error"));
    }
}
