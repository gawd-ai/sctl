//! Peplink / Pepwave router profile (MAX BR1 family, firmware 8.x).
//!
//! Login is `POST /api/login` with a JSON body; the session is a cookie.
//! This model answers an expired session as `{"stat":"fail","code":401}`
//! inside an HTTP 200, so the status line alone never reveals it.
//!
//! The reduction below is the one netage-server's AP poller applied on the
//! droplet (`fleet/ap_poller.rs::build_snapshot`), moved here so it runs on
//! the vehicle: the keys are unchanged, so the fleet consumer does not care
//! where the reduction happened, and the client list is reduced to per-VLAN
//! counts before anything leaves this function.

use std::collections::HashMap;
use std::time::Instant;

use serde_json::{json, Value};
use tracing::{debug, warn};

use super::super::checks::{CheckContext, CheckResult};
use crate::routes::fetch::{execute, FetchError, FetchRequest, FetchResponse};

const DEFAULT_TIMEOUT_MS: u64 = 10_000;
const LOGIN_PATH: &str = "/api/login";
const WAN_PATH: &str = "/api/status.wan.connection";
const CLIENT_PATH: &str = "/api/status.client";
const LOCATION_PATH: &str = "/api/info.location";
const LAN_PATH: &str = "/api/status.lan";
const TRAFFIC_PATH: &str = "/api/status.traffic";
const SYSTEM_PATH: &str = "/api/info.system";
/// A reply larger than this is not a status page.
const MAX_BODY_BYTES: usize = 400_000;

/// Run one Peplink check: log in (or reuse the session), read the endpoints,
/// reduce. The WAN read decides `ok`; the others are best-effort detail.
pub async fn check(
    ctx: &CheckContext,
    base_url: &str,
    pin_sha256: Option<&str>,
    timeout_ms: Option<u64>,
) -> CheckResult {
    let base = base_url.trim_end_matches('/');
    let timeout = timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
    let Some(cred) = ctx.credential.as_ref() else {
        return CheckResult::failed("API NO CREDENTIAL: peplink profile needs a credential");
    };

    let mut session = ctx.session.clone();
    let mut renewed: Option<String> = None;
    let start = Instant::now();

    // First the WAN status; a 401-in-200 means the session died, so log in
    // once and retry. No session cached means the same path.
    let wan = loop {
        let cookie = match &session {
            Some(c) => c.clone(),
            None => match login(base, pin_sha256, timeout, &cred.username, &cred.password).await {
                Ok(c) => {
                    renewed = Some(c.clone());
                    session = Some(c.clone());
                    c
                }
                Err(r) => return r,
            },
        };
        match get(base, WAN_PATH, &cookie, pin_sha256, timeout).await {
            Ok(resp) if is_unauthorized(&resp.body) => {
                if renewed.is_some() {
                    return CheckResult::failed(
                        "API LOGIN REJECTED: session refused right after login",
                    );
                }
                debug!("peplink {base}: session expired, logging in again");
                session = None;
            }
            Ok(resp) => break resp,
            Err(r) => return r,
        }
    };
    let latency = start.elapsed().as_millis() as u64;
    let cookie = session.clone().unwrap_or_default();

    let Some(wan_resp) = inner_response(&wan.body) else {
        return CheckResult {
            ok: false,
            latency_ms: Some(latency),
            detail: format!("API BAD REPLY: {WAN_PATH} did not answer stat ok"),
            http_status: Some(wan.status),
            session: renewed,
            ..CheckResult::default()
        };
    };

    // Best-effort reads. A missing one degrades the snapshot, not the check.
    let read = |path: &'static str| {
        let cookie = cookie.clone();
        async move {
            match get(base, path, &cookie, pin_sha256, timeout).await {
                Ok(resp) => inner_response(&resp.body),
                Err(r) => {
                    warn!("peplink {base}: {path} failed: {}", r.detail);
                    None
                }
            }
        }
    };
    let clients = read(CLIENT_PATH).await;
    let location = read(LOCATION_PATH).await;
    let lan = read(LAN_PATH).await;
    let traffic = read(TRAFFIC_PATH).await;
    let system = read(SYSTEM_PATH).await;

    let data = build_snapshot(
        &wan_resp,
        clients.as_ref(),
        location.as_ref(),
        lan.as_ref(),
        traffic.as_ref(),
        system.as_ref(),
    );
    let detail = summary(&data, latency);

    CheckResult {
        ok: true,
        latency_ms: Some(latency),
        detail,
        http_status: Some(wan.status),
        data: Some(data),
        session: renewed,
        presented_sha256: None,
    }
}

// ─── transport ───────────────────────────────────────────────────────

fn request(base: &str, path: &str, pin: Option<&str>, timeout: u64) -> FetchRequest {
    FetchRequest {
        url: format!("{base}{path}"),
        timeout_ms: Some(timeout),
        max_bytes: Some(MAX_BODY_BYTES),
        pin_sha256: pin.map(str::to_string),
        ..FetchRequest::default()
    }
}

/// Map a transport failure to a check failure, keeping a presented
/// fingerprint where there is one so the operator can pin it.
fn transport_failure(path: &str, e: &FetchError) -> CheckResult {
    let presented = match e {
        FetchError::PinMismatch { presented, .. } | FetchError::Untrusted { presented, .. } => {
            presented.clone()
        }
        _ => None,
    };
    let label = match e {
        FetchError::PinMismatch { .. } => "API PIN MISMATCH",
        FetchError::Untrusted { .. } => "API UNPINNED",
        FetchError::Timeout(_) => "API TIMEOUT",
        FetchError::Invalid(_) => "API INVALID",
        FetchError::Failed(_) => "API FAIL",
    };
    CheckResult {
        detail: format!("{label}: {path}: {e}"),
        presented_sha256: presented,
        ..CheckResult::default()
    }
}

async fn login(
    base: &str,
    pin: Option<&str>,
    timeout: u64,
    username: &str,
    password: &str,
) -> Result<String, CheckResult> {
    let mut req = request(base, LOGIN_PATH, pin, timeout);
    req.method = Some("POST".to_string());
    req.headers = Some(HashMap::from([(
        "Content-Type".to_string(),
        "application/json".to_string(),
    )]));
    req.body = Some(json!({"username": username, "password": password}).to_string());
    req.max_bytes = Some(4000);
    let resp = execute(&dir_of(), &req)
        .await
        .map_err(|e| transport_failure(LOGIN_PATH, &e))?;
    let cookie = resp
        .headers
        .get("set-cookie")
        .and_then(|c| c.split(';').next())
        .map(str::to_string);
    match cookie {
        Some(c) if inner_response(&resp.body).is_some() => Ok(c),
        _ => Err(CheckResult {
            detail: format!(
                "API LOGIN FAILED: http {} {}",
                resp.status,
                first_message(&resp.body)
            ),
            http_status: Some(resp.status),
            ..CheckResult::default()
        }),
    }
}

async fn get(
    base: &str,
    path: &str,
    cookie: &str,
    pin: Option<&str>,
    timeout: u64,
) -> Result<FetchResponse, CheckResult> {
    let mut req = request(base, path, pin, timeout);
    req.headers = Some(HashMap::from([("Cookie".to_string(), cookie.to_string())]));
    execute(&dir_of(), &req)
        .await
        .map_err(|e| transport_failure(path, &e))
}

/// The pin store location. Set once by the monitor before any check runs;
/// the profile is a leaf and has no other way to reach the config.
fn dir_of() -> String {
    DATA_DIR.get().cloned().unwrap_or_default()
}
pub static DATA_DIR: std::sync::OnceLock<String> = std::sync::OnceLock::new();

// ─── reply parsing ───────────────────────────────────────────────────

/// Did the AP answer 401 INSIDE a successful HTTP 200 envelope?
pub fn is_unauthorized(body: &str) -> bool {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|b| b.get("code").and_then(Value::as_i64))
        == Some(401)
}

/// The `response` of a `{"stat":"ok","response":...}` reply.
pub fn inner_response(body: &str) -> Option<Value> {
    let parsed: Value = serde_json::from_str(body).ok()?;
    if parsed.get("stat")?.as_str()? != "ok" {
        return None;
    }
    parsed.get("response").cloned()
}

fn first_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|b| b.get("message").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default()
}

/// Parse a dotted-quad into a u32. None for anything that is not one.
fn ipv4_to_u32(s: &str) -> Option<u32> {
    let mut out: u32 = 0;
    let mut parts = 0;
    for octet in s.split('.') {
        let v: u32 = octet.parse().ok()?;
        if v > 255 {
            return None;
        }
        out = (out << 8) | v;
        parts += 1;
    }
    if parts == 4 {
        Some(out)
    } else {
        None
    }
}

/// Is `ip` inside `net/mask`? A /0 matches everything, which is why the shift
/// is guarded.
fn same_subnet(ip: u32, net: u32, mask: u32) -> bool {
    if mask == 0 {
        return true;
    }
    let m = u32::MAX << (32 - mask.min(32));
    (ip & m) == (net & m)
}

/// One line for the results table: which WAN carries, what cellular is doing,
/// how many riders.
fn summary(data: &Value, latency: u64) -> String {
    let active = data["ap_active_wan"].as_str().unwrap_or("");
    let up = data["ap_wan_up"].as_i64().unwrap_or(0);
    let riders = data["riders_associated"]
        .as_i64()
        .map_or_else(|| "riders unknown".to_string(), |r| format!("{r} riders"));
    let cellular = data["wans"]
        .as_array()
        .and_then(|w| w.iter().find(|x| x["type"] == "cellular"))
        .and_then(|c| c["message"].as_str())
        .map(|m| format!(", cellular {}", m.to_ascii_lowercase()))
        .unwrap_or_default();
    if up == 0 {
        format!("API OK {latency}ms: no WAN up{cellular}, {riders}")
    } else {
        format!("API OK {latency}ms: {active} carrying ({up} up){cellular}, {riders}")
    }
}

// ─── reduction ───────────────────────────────────────────────────────

/// Reduce the AP's replies to the fixed columns attribution reads.
///
/// Every argument is the `response` object of the corresponding endpoint.
pub fn build_snapshot(
    wan: &Value,
    clients: Option<&Value>,
    location: Option<&Value>,
    lan: Option<&Value>,
    traffic: Option<&Value>,
    system: Option<&Value>,
) -> Value {
    let mut wans: Vec<Value> = Vec::new();
    let mut wan_up = 0i64;
    // The ACTIVE wan is the connected one with the best (lowest) priority.
    // `uptime > 0` is the real test: this model reports a standby cellular WAN
    // as enabled, with an IP, and `uptime: 0`; it is not carrying anything.
    let mut active: Option<(i64, String)> = None;
    let mut cellular_module: Option<Value> = None;

    if let Some(obj) = wan.as_object() {
        for (key, v) in obj {
            if !key.chars().all(|c| c.is_ascii_digit()) {
                continue; // "order", "timestamp", ...
            }
            let name = v
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let led = v.get("statusLed").and_then(Value::as_str).unwrap_or("");
            let message = v.get("message").and_then(Value::as_str).unwrap_or("");
            let uptime = v.get("uptime").and_then(Value::as_i64).unwrap_or(0);
            let enabled = v.get("enable").and_then(Value::as_bool).unwrap_or(false);
            let priority = v.get("priority").and_then(Value::as_i64);
            let connected = led == "green" && uptime > 0;
            if connected {
                wan_up += 1;
                let p = priority.unwrap_or(i64::MAX);
                match &active {
                    Some((best, _)) if *best <= p => {}
                    _ => active = Some((p, name.clone())),
                }
            }
            // The physical modem's identity and SIM state, once. Subscriber
            // identifiers (IMSI, ICCID, IMEI) are deliberately not carried.
            if v.get("type").and_then(Value::as_str) == Some("cellular")
                && v.get("virtual").and_then(Value::as_bool) != Some(true)
                && cellular_module.is_none()
            {
                if let Some(c) = v.get("cellular") {
                    let sim_active = |slot: &str| {
                        c.pointer(&format!("/sim/{slot}/active"))
                            .and_then(Value::as_bool)
                    };
                    let sim_detected = |slot: &str| {
                        c.pointer(&format!("/sim/{slot}/simCardDetected"))
                            .and_then(Value::as_bool)
                    };
                    cellular_module = Some(json!({
                        "manufacturer": c.get("manufacturer").cloned(),
                        "model": c.get("model").cloned(),
                        "firmware": c.get("firmware").cloned(),
                        "carrier": c.pointer("/carrier/name").cloned(),
                        "sim": {
                            "1": {"detected": sim_detected("1"), "active": sim_active("1")},
                            "2": {"detected": sim_detected("2"), "active": sim_active("2")},
                            "esim": {"active": c.pointer("/speedfusionConnect5gLte/active").cloned()},
                        },
                    }));
                }
            }
            wans.push(json!({
                "id": key,
                "name": name,
                "type": v.get("type").and_then(Value::as_str).unwrap_or(""),
                "enabled": enabled,
                "connected": connected,
                // "Standby" is a distinct and load-bearing state: a cold
                // failover WAN must dial before it carries anything, and
                // that delay is a rider-visible gap.
                "message": message,
                "priority": priority,
                "uptime_secs": uptime,
                "signal": v.pointer("/cellular/rat/0/band/0/signal").cloned(),
                "carrier": v.pointer("/cellular/carrier/name").cloned(),
            }));
        }
    }

    // Which LAN carries the passengers.
    //
    // The AP describes its own networks: on Bus 01, LAN 0 is 192.168.50.1/24
    // (ours, unnamed) and LAN 1 is 10.10.10.1/24 named "PASSENGER". Taking the
    // id from the AP rather than from config means the next vehicle does not
    // need a hardcoded VLAN number that would be silently wrong.
    // NOTE the trap: the key in this reply is the LAN INDEX, not the VLAN id.
    // On Bus 01 the passenger LAN is index "1" while its clients report
    // `vlanId: 10`. Keying on the index counts zero riders for ever, silently,
    // because no client ever reports vlan 1.
    //
    // So clients are matched to the network by SUBNET, which is the AP's own
    // self-description and does not depend on how it happens to tag. The vlan
    // id is then READ BACK from the matched clients rather than assumed.
    let rider_net: Option<(u32, u32, String)> = lan.and_then(Value::as_object).and_then(|o| {
        o.iter()
            // Index 0 is the management LAN, ours. A NAMED non-zero LAN is
            // the one the operator set up for someone else.
            .filter(|(k, _)| k.chars().all(|c| c.is_ascii_digit()) && *k != "0")
            .find_map(|(_, v)| {
                let name = v.get("name").and_then(Value::as_str)?;
                let ip = ipv4_to_u32(v.get("ip").and_then(Value::as_str)?)?;
                let mask = u32::try_from(v.get("mask").and_then(Value::as_i64)?).ok()?;
                if mask > 32 {
                    return None;
                }
                Some((ip, mask, name.to_string()))
            })
    });
    let rider_lan_name = rider_net.as_ref().map(|(_, _, n)| n.clone());

    // PII BOUNDARY. The client list carries MACs, IPs and owner-chosen device
    // names for passengers. Only COUNTS leave this function.
    //
    // Counted PER VLAN: every active client was once reported as a "rider",
    // including our own device and anything else on the management LAN,
    // which inflated the figure the rider SLA is read from.
    let active_clients: Vec<Value> = clients
        .and_then(|r| r.get("list").and_then(Value::as_array).cloned())
        .unwrap_or_default()
        .into_iter()
        .filter(|c| c.get("active").and_then(Value::as_bool).unwrap_or(false))
        .collect();

    let mut by_vlan: HashMap<String, i64> = HashMap::new();
    for c in &active_clients {
        let key = c
            .get("vlanId")
            .and_then(Value::as_i64)
            .map_or_else(|| "untagged".to_string(), |v| v.to_string());
        *by_vlan.entry(key).or_insert(0) += 1;
    }

    // Riders = active clients inside the passenger subnet. When the AP did not
    // describe that network, this stays None rather than falling back to the
    // total: an over-count that looks like a healthy passenger load is worse
    // than an admitted gap.
    let rider_clients: Vec<&Value> = match &rider_net {
        Some((net, mask, _)) => active_clients
            .iter()
            .filter(|c| {
                c.get("ip")
                    .and_then(Value::as_str)
                    .and_then(ipv4_to_u32)
                    .is_some_and(|ip| same_subnet(ip, *net, *mask))
            })
            .collect(),
        None => Vec::new(),
    };
    let riders = rider_net
        .as_ref()
        .map(|_| i64::try_from(rider_clients.len()).unwrap_or(i64::MAX));

    // The vlan id the riders actually report, read back rather than assumed.
    let rider_vlan = rider_clients
        .iter()
        .find_map(|c| c.get("vlanId").and_then(Value::as_i64));

    // Per-WAN throughput, in kbps, at the moment of the reading.
    //
    // NOT per-VLAN, and that limit is the whole reason `ap_egress_measured`
    // stays false below. It shows the AP moving bytes for SOMEONE; it cannot
    // partition riders from us. Only one direction is sound: sustained
    // multi-megabit traffic cannot be our management tunnel, so it is
    // positive evidence the service works, while zero is NOT evidence of an
    // outage, because idle passengers produce exactly zero.
    let bandwidth = traffic
        .and_then(|t| t.get("bandwidth").cloned())
        .and_then(|b| {
            b.as_object().map(|o| {
                let wans: Vec<Value> = o
                    .iter()
                    .filter(|(k, _)| k.chars().all(|c| c.is_ascii_digit()))
                    .map(|(k, v)| {
                        json!({
                            "id": k,
                            "name": v.get("name").and_then(Value::as_str).unwrap_or(""),
                            "download_kbps": v.pointer("/overall/download").and_then(Value::as_i64),
                            "upload_kbps": v.pointer("/overall/upload").and_then(Value::as_i64),
                        })
                    })
                    .collect();
                json!(wans)
            })
        });

    // GPS. `gps: false` is "no fix", never a position. The fix itself lives
    // under `location`, one level down.
    let has_gps = location
        .and_then(|l| l.get("gps"))
        .is_some_and(|g| !matches!(g, Value::Bool(false)));
    let fix = if has_gps {
        location.and_then(|l| l.get("location"))
    } else {
        None
    };
    let fix_num = |key: &str| fix.and_then(|f| f.get(key)).and_then(Value::as_f64);
    let ap_gps = fix.map(|_| {
        json!({
            // Units for `speed` are not documented by Peplink; carried raw so
            // the consumer can calibrate it before claiming a speed.
            "speed_raw": fix_num("speed"),
            "heading_deg": fix_num("heading"),
            "altitude_m": fix_num("altitude"),
            "hdop": fix_num("hdop"),
            "fix_ts": fix.and_then(|f| f.get("timestamp")).and_then(Value::as_i64),
        })
    });

    // The router's own identity, for the technical view.
    let ap_system = system.map(|sys| {
        json!({
            "router_name": sys.get("routerName").cloned(),
            "vendor": sys.get("company").cloned(),
            "gps_supported": sys.pointer("/support/gps/support").cloned(),
            "cellular_module": cellular_module,
        })
    });

    json!({
        "ts": super::super::now_iso(),
        "ap_reachable": true,
        // Rider egress is NOT measured. `ap_egress_state` below is derived
        // from the WAN count, which shows the AP reaching the internet, not
        // that it delivers to the riders' LAN (they are on vlan 10 while we
        // are on 192.168.50.0/24). Stated explicitly so the consumer cannot
        // mistake the proxy for the measurement.
        "ap_egress_measured": false,
        "ap_wan_up": wan_up,
        "ap_egress_state": if wan_up > 0 { "up" } else { "down" },
        "ap_active_wan": active.as_ref().map(|(_, n)| n.clone()).unwrap_or_default(),
        "riders_associated": riders,
        // The VLAN the AP itself says the passengers are on, and its name.
        "rider_vlan_id": rider_vlan,
        "rider_lan_name": rider_lan_name,
        "clients_by_vlan": by_vlan,
        "wan_bandwidth": bandwidth,
        "gps_available": has_gps,
        "lat": fix_num("latitude"),
        "lng": fix_num("longitude"),
        "ap_gps": ap_gps,
        "ap_system": ap_system,
        "wans": wans,
    })
}

#[cfg(test)]
#[allow(clippy::unreadable_literal)]
mod tests {
    use super::*;

    /// Verbatim shape from the Peplink MAX BR1 on Concord Tours Bus 01,
    /// 2026-08-11 19:52 UTC. WAN1 connected, cellular in cold standby.
    fn real_wan() -> Value {
        json!({
            "1": {"name":"WAN","enable":true,"ip":"100.98.218.234","statusLed":"green",
                  "message":"Connected","uptime":10228,"type":"ethernet","priority":1},
            "2": {"name":"Cellular","enable":true,"ip":"10.5.217.34","statusLed":"yellow",
                  "message":"Standby","uptime":0,"type":"cellular","priority":2,
                  "cellular":{"carrier":{"name":"ROGERS"},
                              "manufacturer":"Quectel","model":"RM520N-GL","firmware":"RM520NGLAAR01A08M4G",
                              "sim":{"1":{"active":false,"simCardDetected":true,"imsi":"302720713604621","iccid":"89302720554119789815"},
                                     "2":{"active":false,"simCardDetected":false}},
                              "speedfusionConnect5gLte":{"active":true,"imsi":"454006344183417"},
                              "imei":"868371053659609",
                              "rat":[{"band":[{"signal":{"rsrp":-111,"rsrq":-18.0}}]}]}},
            "3": {"name":"Wi-Fi WAN on 2.4 GHz","enable":false,"statusLed":"gray",
                  "message":"Disabled","uptime":0,"type":"wifi"},
            "order": [1,2,3],
            "timestamp": 1786477965
        })
    }

    /// The AP's own LAN list, verbatim from Bus 01: LAN 0 is our management
    /// network and LAN 1 is the passengers', which the AP names itself.
    fn real_lan() -> Value {
        json!({
            "0": {"ip":"192.168.50.1","mask":24},
            "1": {"ip":"10.10.10.1","mask":24,"name":"PASSENGER"},
            "order": [0, 1]
        })
    }

    /// Clients as this model reports them: every entry carries a `vlanId`.
    fn real_clients() -> Value {
        json!({"list":[
            {"mac":"AA:BB:CC:DD:EE:FF","name":"S25-Ultra-de-Someone","active":true,
             "vlanId":10,"ip":"10.10.10.51","connectionType":"wifi"},
            {"mac":"11:22:33:44:55:66","name":"Someone-s-iPhone","active":true,
             "vlanId":10,"ip":"10.10.10.52","connectionType":"wifi"},
            {"mac":"99:88:77:66:55:44","name":"iPad","active":false,
             "vlanId":10,"ip":"10.10.10.53","connectionType":"wifi"},
            {"mac":"F8:5E:3C:D0:13:0E","name":"OpenWrt","active":true,
             "ip":"192.168.50.30","connectionType":"ethernet"}
        ]})
    }

    /// The documented shape of `GET /api/info.location` with a fix.
    fn documented_fix() -> Value {
        json!({
            "gps": true,
            "location": {
                "latitude": 22.340134, "longitude": 114.152588, "altitude": 55.1,
                "speed": 0.026751, "heading": 356.887,
                "pdop": 1.3, "hdop": 1, "vdop": 0.8, "timestamp": 1311972720
            }
        })
    }

    #[test]
    fn standby_wan_does_not_count_as_up() {
        // The whole rider-SLA story depends on this. The cellular WAN is
        // enabled and HAS an ip, but uptime 0 means it is not carrying
        // anything; counting it would hide every failover gap.
        let s = build_snapshot(&real_wan(), None, None, None, None, None);
        assert_eq!(s["ap_wan_up"], 1);
        assert_eq!(s["ap_active_wan"], "WAN");
        assert_eq!(s["ap_egress_state"], "up");
        assert_eq!(s["ap_egress_measured"], false);
    }

    #[test]
    fn standby_state_is_preserved_for_diagnosis() {
        let s = build_snapshot(&real_wan(), None, None, None, None, None);
        let cell = s["wans"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["name"] == "Cellular")
            .unwrap();
        assert_eq!(cell["message"], "Standby");
        assert_eq!(cell["connected"], false);
        assert_eq!(cell["carrier"], "ROGERS");
        assert_eq!(cell["signal"]["rsrp"], -111);
    }

    #[test]
    fn all_wans_down_is_egress_down() {
        let wan = json!({
            "1": {"name":"WAN","statusLed":"red","message":"Disconnected","uptime":0,"priority":1},
            "2": {"name":"Cellular","statusLed":"yellow","message":"Standby","uptime":0,"priority":2}
        });
        let s = build_snapshot(&wan, None, None, None, None, None);
        assert_eq!(s["ap_wan_up"], 0);
        assert_eq!(s["ap_egress_state"], "down");
        assert_eq!(s["ap_active_wan"], "");
    }

    #[test]
    fn active_wan_is_the_best_priority_among_connected() {
        let wan = json!({
            "1": {"name":"WAN","statusLed":"green","message":"Connected","uptime":10,"priority":2},
            "2": {"name":"Cellular","statusLed":"green","message":"Connected","uptime":5,"priority":1}
        });
        let s = build_snapshot(&wan, None, None, None, None, None);
        assert_eq!(s["ap_wan_up"], 2);
        assert_eq!(
            s["ap_active_wan"], "Cellular",
            "priority 1 beats priority 2"
        );
    }

    #[test]
    fn client_list_is_reduced_to_counts_and_identifiers_never_survive() {
        // PII boundary. These are passengers' personal devices, and the modem
        // block carries subscriber identifiers.
        let s = build_snapshot(
            &real_wan(),
            Some(&real_clients()),
            None,
            Some(&real_lan()),
            None,
            None,
        );
        let serialized = s.to_string();
        assert!(
            !serialized.contains("S25-Ultra"),
            "device names must not survive"
        );
        assert!(!serialized.contains("AA:BB:CC"), "MACs must not survive");
        assert!(
            !serialized.contains("10.10.10.51"),
            "client IPs must not survive"
        );
        assert!(
            !serialized.contains("302720713604621"),
            "IMSI must not survive"
        );
        assert!(
            !serialized.contains("89302720554119789815"),
            "ICCID must not survive"
        );
        assert!(
            !serialized.contains("868371053659609"),
            "IMEI must not survive"
        );
        assert_eq!(
            s["riders_associated"], 2,
            "two active passengers on the named LAN"
        );
        assert_eq!(
            s["rider_vlan_id"], 10,
            "read back from the riders, not assumed"
        );
        assert_eq!(s["rider_lan_name"], "PASSENGER");
        assert_eq!(s["clients_by_vlan"]["10"], 2);
        assert_eq!(s["clients_by_vlan"]["untagged"], 1, "that one is us");
    }

    #[test]
    fn riders_are_null_rather_than_over_counted_when_the_lan_is_unknown() {
        let s = build_snapshot(&real_wan(), Some(&real_clients()), None, None, None, None);
        assert!(s["riders_associated"].is_null());
        assert!(s["rider_vlan_id"].is_null());
        assert_eq!(
            s["clients_by_vlan"]["10"], 2,
            "the breakdown is still reported"
        );
    }

    #[test]
    fn wan_bandwidth_is_carried_through_but_egress_stays_unmeasured() {
        let traffic = json!({"bandwidth": {
            "timestamp": 1786546271, "unit": "kbps",
            "1": {"name":"WAN","overall":{"download":17755,"upload":6549}},
            "2": {"name":"Cellular","overall":{"download":0,"upload":0}},
            "order": [1, 2]
        }});
        let s = build_snapshot(&real_wan(), None, None, None, Some(&traffic), None);
        let bw = s["wan_bandwidth"].as_array().unwrap();
        let wan1 = bw.iter().find(|b| b["name"] == "WAN").unwrap();
        assert_eq!(wan1["download_kbps"], 17755);
        assert_eq!(wan1["upload_kbps"], 6549);
        assert_eq!(s["ap_egress_measured"], false);
    }

    #[test]
    fn gps_false_is_reported_as_unavailable_not_as_a_position() {
        let s = build_snapshot(
            &real_wan(),
            None,
            Some(&json!({"gps": false})),
            None,
            None,
            None,
        );
        assert_eq!(s["gps_available"], false);
        assert!(s["lat"].is_null());
        assert!(s["lng"].is_null());
        assert!(s["ap_gps"].is_null());
    }

    #[test]
    fn gps_true_reads_the_location_block() {
        let s = build_snapshot(&real_wan(), None, Some(&documented_fix()), None, None, None);
        assert_eq!(s["gps_available"], true);
        assert_eq!(s["lat"], 22.340134);
        assert_eq!(s["lng"], 114.152588);
        assert_eq!(s["ap_gps"]["heading_deg"], 356.887);
        assert_eq!(s["ap_gps"]["hdop"], 1.0);
        assert_eq!(s["ap_gps"]["fix_ts"], 1311972720);
        assert_eq!(s["ap_gps"]["speed_raw"], 0.026751);
    }

    #[test]
    fn a_stale_point_under_gps_false_is_not_a_position() {
        let loc = json!({"gps": false, "location": {"latitude": 45.5, "longitude": -73.6}});
        let s = build_snapshot(&real_wan(), None, Some(&loc), None, None, None);
        assert_eq!(s["gps_available"], false);
        assert!(s["lat"].is_null());
    }

    #[test]
    fn the_modem_and_sim_state_are_reported_without_subscriber_identifiers() {
        let sys = json!({"routerName": "MAX_BR1_DA68", "company": "Peplink",
                         "support": {"gps": {"support": true}}});
        let s = build_snapshot(&real_wan(), None, None, None, None, Some(&sys));
        assert_eq!(s["ap_system"]["router_name"], "MAX_BR1_DA68");
        assert_eq!(s["ap_system"]["gps_supported"], true);
        let m = &s["ap_system"]["cellular_module"];
        assert_eq!(m["model"], "RM520N-GL");
        assert_eq!(m["sim"]["1"]["detected"], true);
        assert_eq!(m["sim"]["1"]["active"], false);
        assert_eq!(
            m["sim"]["esim"]["active"], true,
            "the eSIM starving the physical SIM shows"
        );
        assert!(m.get("imei").is_none());
    }

    #[test]
    fn a_401_inside_an_http_200_is_recognised_as_expired() {
        assert!(is_unauthorized(
            r#"{"stat":"fail","code":401,"message":"Unauthorized"}"#
        ));
        assert!(!is_unauthorized(r#"{"stat":"ok","response":{}}"#));
    }

    #[test]
    fn a_failed_envelope_yields_no_response() {
        assert!(inner_response(r#"{"stat":"fail","code":999}"#).is_none());
        assert_eq!(
            inner_response(r#"{"stat":"ok","response":{"a":1}}"#).unwrap()["a"],
            1
        );
    }

    #[test]
    fn the_summary_names_the_carrying_wan_and_the_riders() {
        let s = build_snapshot(
            &real_wan(),
            Some(&real_clients()),
            None,
            Some(&real_lan()),
            None,
            None,
        );
        assert_eq!(
            summary(&s, 42),
            "API OK 42ms: WAN carrying (1 up), cellular standby, 2 riders"
        );
    }
}
