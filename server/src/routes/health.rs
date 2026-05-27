//! Unauthenticated health-check endpoint.

use std::sync::atomic::Ordering;
use std::time::Instant;

use axum::{extract::State, Json};
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::AppState;

/// `GET /api/health` — liveness probe.
///
/// Returns status, uptime, version, session count, and tunnel status. No
/// authentication required, suitable for load-balancer health checks.
pub async fn health(State(state): State<AppState>) -> Json<Value> {
    let start = Instant::now();
    let has_lte = state.config.lte.is_some();
    info!(has_lte, "api.health: begin");

    let uptime = state.start_time.elapsed().as_secs();
    let sessions = state.session_manager.session_count().await;
    let ts = &state.tunnel_stats;

    let tunnel_connected = ts.connected.load(Ordering::Relaxed);
    let tunnel_reconnects = ts.reconnects.load(Ordering::Relaxed);

    // Build enhanced tunnel section when tunnel client mode is configured
    let tunnel = if state
        .config
        .tunnel
        .as_ref()
        .is_some_and(|tc| tc.url.is_some() && !tc.relay)
    {
        let messages_sent = ts.messages_sent.load(Ordering::Relaxed);
        let messages_received = ts.messages_received.load(Ordering::Relaxed);
        let last_pong_age_ms = ts.last_pong_age_ms.load(Ordering::Relaxed);
        let current_uptime_ms = ts.current_uptime_ms.load(Ordering::Relaxed);
        let dropped_outbound = ts.dropped_outbound.load(Ordering::Relaxed);
        let stream_backpressure_events = ts.stream_backpressure_events.load(Ordering::Relaxed);
        let stream_replay_events = ts.stream_replay_events.load(Ordering::Relaxed);

        let rtt = ts.rtt_stats().await;
        let (rtt_median, rtt_p95) = rtt.unwrap_or((0, 0));

        // Format recent events
        let events = ts.events.lock().await;
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let recent_events: Vec<Value> = events
            .iter()
            .rev()
            .take(10)
            .map(|e| {
                let ago_secs = now_unix.saturating_sub(e.timestamp);
                let ago_str = if ago_secs < 60 {
                    format!("{ago_secs}s ago")
                } else if ago_secs < 3600 {
                    format!("{}m ago", ago_secs / 60)
                } else {
                    format!("{}h ago", ago_secs / 3600)
                };
                json!({
                    "time": ago_str,
                    "event": e.event_type.as_str(),
                    "detail": e.detail,
                })
            })
            .collect();

        json!({
            "connected": tunnel_connected,
            "reconnects": tunnel_reconnects,
            "uptime_secs": current_uptime_ms / 1000,
            "messages_sent": messages_sent,
            "messages_received": messages_received,
            "last_pong_age_ms": last_pong_age_ms,
            "dropped_outbound": dropped_outbound,
            "stream_backpressure_events": stream_backpressure_events,
            "stream_replay_events": stream_replay_events,
            "rtt_median_ms": rtt_median,
            "rtt_p95_ms": rtt_p95,
            "recent_events": recent_events,
        })
    } else {
        json!({
            "connected": tunnel_connected,
            "reconnects": tunnel_reconnects,
        })
    };

    // GPS summary
    let gps = if state.config.gps.is_some() {
        state
            .comms_state
            .as_ref()
            .and_then(|cs| cs.try_lock().ok())
            .and_then(|cs| cs.gps.clone())
            .map_or_else(
                || json!({"status": "searching", "has_fix": false}),
                |gps| {
                    let last_fix = gps.get("last_fix").cloned().unwrap_or(Value::Null);
                    json!({
                        "status": gps.get("status").cloned().unwrap_or_else(|| json!("unknown")),
                        "has_fix": !last_fix.is_null(),
                        "fix_age_secs": gps.get("fix_age_secs").cloned().unwrap_or(Value::Null),
                        "satellites": last_fix.get("satellites").cloned().unwrap_or(Value::Null),
                    })
                },
            )
    } else {
        json!(null)
    };

    // LTE summary
    let mut lte_lock_wait_ms = 0u64;
    let lte = if state.config.lte.is_some() {
        if let Some(ref cs) = state.comms_state {
            let lock_started = Instant::now();
            let cs = cs.lock().await;
            #[allow(clippy::cast_possible_truncation)]
            {
                lte_lock_wait_ms = lock_started.elapsed().as_millis() as u64;
            }
            if let Some(sig) = cs.lte.as_ref().and_then(|v| v.get("signal")) {
                json!({
                    "rssi_dbm": sig.get("rssi_dbm").cloned().unwrap_or(Value::Null),
                    "rsrp": sig.get("rsrp").cloned().unwrap_or(Value::Null),
                    "sinr": sig.get("sinr").cloned().unwrap_or(Value::Null),
                    "signal_bars": sig.get("signal_bars").cloned().unwrap_or(Value::Null),
                    "band": sig.get("band").cloned().unwrap_or(Value::Null),
                    "operator": sig.get("operator").cloned().unwrap_or(Value::Null),
                })
            } else {
                json!({"status": "no_signal"})
            }
        } else {
            json!({"status": "provider_unavailable"})
        }
    } else {
        json!(null)
    };

    // Connection history (relay mode only)
    let connection_history = if let Some(ref history) = state.relay_history {
        let sessions_snap = history.snapshot().await;
        #[allow(clippy::cast_possible_truncation)]
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let entries: Vec<serde_json::Value> = sessions_snap
            .iter()
            .map(|s| {
                let duration_secs = s
                    .disconnected_at
                    .unwrap_or(now)
                    .saturating_sub(s.connected_at);
                json!({
                    "serial": s.serial,
                    "connected_at": s.connected_at,
                    "disconnected_at": s.disconnected_at,
                    "duration_secs": duration_secs,
                    "reason": s.reason,
                    "last_heartbeat_age_ms": s.last_heartbeat_age_ms,
                })
            })
            .collect();
        Some(entries)
    } else {
        None
    };

    // Device snapshots (relay mode only) — last-known telemetry for offline devices
    let device_snapshots = if let Some(ref snaps) = state.device_snapshots {
        let snaps = snaps.read().await;
        let map: serde_json::Map<String, Value> = snaps
            .iter()
            .map(|(serial, snap)| {
                (
                    serial.clone(),
                    json!({
                        "last_lte_signal": snap.last_lte_signal,
                        "last_gps_fix": snap.last_gps_fix,
                        "last_watchdog": snap.last_watchdog,
                        "last_seen": snap.last_seen,
                    }),
                )
            })
            .collect();
        Some(Value::Object(map))
    } else {
        None
    };

    let live_devices = if let Some(ref relay_state) = state.relay_state {
        Some(relay_state.live_device_statuses().await)
    } else {
        None
    };

    let mut resp = json!({
        "status": "ok",
        "uptime_secs": uptime,
        "version": crate::VERSION,
        "sessions": sessions,
        "tunnel": tunnel,
        "gps": gps,
        "lte": lte,
    });
    if let Some(ch) = connection_history {
        resp["connection_history"] = json!(ch);
    }
    if let Some(ds) = device_snapshots {
        resp["device_snapshots"] = ds;
    }
    if let Some(ld) = live_devices {
        resp["live_devices"] = json!(ld);
    }
    #[allow(clippy::cast_possible_truncation)]
    let total_ms = start.elapsed().as_millis() as u64;
    if lte_lock_wait_ms >= 250 {
        warn!(
            total_ms,
            lte_lock_wait_ms, "api.health: slow LTE state lock acquisition"
        );
    }
    info!(total_ms, lte_lock_wait_ms, "api.health: end");
    Json(resp)
}
