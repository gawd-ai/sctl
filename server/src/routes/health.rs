//! Unauthenticated health-check endpoint.

use std::sync::atomic::Ordering;

use axum::{extract::State, Json};
use serde_json::{json, Value};

use crate::AppState;

/// `GET /api/health` — liveness probe.
///
/// Returns status, uptime, version, session count, and tunnel status. No
/// authentication required, suitable for load-balancer health checks.
pub async fn health(State(state): State<AppState>) -> Json<Value> {
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

        let rtt = ts.rtt_stats().await;
        let (rtt_median, rtt_p95) = rtt.unwrap_or((0, 0));

        // Format recent events
        let events = ts.events.lock().await;
        let now = std::time::Instant::now();
        let recent_events: Vec<Value> = events
            .iter()
            .rev()
            .take(10)
            .map(|e| {
                let ago = now.duration_since(e.timestamp);
                let ago_str = if ago.as_secs() < 60 {
                    format!("{}s ago", ago.as_secs())
                } else if ago.as_secs() < 3600 {
                    format!("{}m ago", ago.as_secs() / 60)
                } else {
                    format!("{}h ago", ago.as_secs() / 3600)
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
    let gps = if let Some(ref gs) = state.gps_state {
        let gs = gs.lock().await;
        let has_fix = gs.last_fix.is_some();
        let fix_age_secs = gs.last_fix_at.map(|t| t.elapsed().as_secs());
        let satellites = gs.last_fix.as_ref().map(|f| f.satellites);
        json!({
            "status": gs.status,
            "has_fix": has_fix,
            "fix_age_secs": fix_age_secs,
            "satellites": satellites,
        })
    } else {
        json!(null)
    };

    // LTE summary
    let lte = if let Some(ref ls) = state.lte_state {
        let ls = ls.lock().await;
        if let Some(ref sig) = ls.signal {
            json!({
                "rssi_dbm": sig.rssi_dbm,
                "rsrp": sig.rsrp,
                "sinr": sig.sinr,
                "signal_bars": sig.signal_bars,
                "band": sig.band,
                "operator": sig.operator,
            })
        } else {
            json!({"status": "no_signal"})
        }
    } else {
        json!(null)
    };

    Json(json!({
        "status": "ok",
        "uptime_secs": uptime,
        "version": env!("CARGO_PKG_VERSION"),
        "sessions": sessions,
        "tunnel": tunnel,
        "gps": gps,
        "lte": lte,
    }))
}
