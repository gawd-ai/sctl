//! LAN device discovery — ARP scan, ping sweep, port probe, mDNS.
//!
//! Triggered on-demand via `POST /api/infra/discover`. Results are cached
//! in `InfraState` until the next scan.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{extract::State, http::StatusCode, Json};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;
use tracing::{debug, info};

use super::checks;
use super::{DiscoveryProgress, InfraState};
use crate::AppState;

/// A discovered LAN device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredDevice {
    pub ip: String,
    pub mac: Option<String>,
    pub hostname: Option<String>,
    pub open_ports: Vec<u16>,
    pub inferred_type: String,
    pub mdns_services: Vec<String>,
}

/// Discovery scan results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryResults {
    pub ts: String,
    pub devices: Vec<DiscoveredDevice>,
    pub scan_duration_ms: u64,
}

/// `POST /api/infra/discover` — trigger a LAN scan.
///
/// Accepts an optional `subnets` array in the request body. If omitted,
/// scans the local ARP table only.
pub async fn discover(
    State(state): State<AppState>,
    Json(payload): Json<Value>,
) -> Result<Json<DiscoveryResults>, (StatusCode, Json<Value>)> {
    let mut subnets: Vec<String> = payload
        .get("subnets")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    // Auto-detect LAN subnets if none provided
    if subnets.is_empty() {
        subnets = auto_detect_subnets().await;
    }

    let infra = state.infra_state.clone();
    info!("Starting LAN discovery scan (subnets: {:?})", subnets);
    let start = std::time::Instant::now();
    let started_at = super::now_iso();

    // Background task: tick elapsed_ms every 500ms so progress endpoint stays fresh
    let elapsed_ticker = {
        let infra = infra.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                if let Some(ref infra) = infra {
                    if let Ok(mut g) = infra.try_lock() {
                        if g.discovery_progress.active {
                            #[allow(clippy::cast_possible_truncation)]
                            {
                                g.discovery_progress.elapsed_ms =
                                    start.elapsed().as_millis() as u64;
                            }
                        } else {
                            break;
                        }
                    }
                } else {
                    break;
                }
            }
        })
    };

    // Helper: update progress in shared state (non-blocking)
    let mk_progress = |phase: &str, num: u8, devices: &HashMap<String, DiscoveredDevice>| {
        #[allow(clippy::cast_possible_truncation)]
        DiscoveryProgress {
            active: true,
            phase: phase.to_string(),
            phase_number: num,
            total_phases: 4,
            hosts_found: devices.len(),
            devices: devices.values().cloned().collect(),
            started_at: Some(started_at.clone()),
            elapsed_ms: start.elapsed().as_millis() as u64,
        }
    };

    // Phase 1: ARP table scan
    set_progress(infra.as_ref(), mk_progress("arp", 1, &HashMap::new()));
    let mut devices: HashMap<String, DiscoveredDevice> = HashMap::new();
    if let Ok(arp_entries) = scan_arp().await {
        for (ip, mac) in arp_entries {
            devices.insert(
                ip.clone(),
                DiscoveredDevice {
                    ip,
                    mac: Some(mac),
                    hostname: None,
                    open_ports: Vec::new(),
                    inferred_type: "other".to_string(),
                    mdns_services: Vec::new(),
                },
            );
        }
    }
    set_progress(infra.as_ref(), mk_progress("arp", 1, &devices));

    // Phase 2: Ping sweep — update progress every 500ms as hosts respond
    set_progress(infra.as_ref(), mk_progress("ping", 2, &devices));
    for subnet in &subnets {
        if let Ok(ips) = ping_sweep(subnet).await {
            for ip in ips {
                devices
                    .entry(ip.clone())
                    .or_insert_with(|| DiscoveredDevice {
                        ip,
                        mac: None,
                        hostname: None,
                        open_ports: Vec::new(),
                        inferred_type: "other".to_string(),
                        mdns_services: Vec::new(),
                    });
            }
        }
        set_progress(infra.as_ref(), mk_progress("ping", 2, &devices));
    }
    set_progress(infra.as_ref(), mk_progress("ping", 2, &devices));

    // Phase 3: Port probe on discovered IPs
    set_progress(infra.as_ref(), mk_progress("ports", 3, &devices));
    let ips: Vec<String> = devices.keys().cloned().collect();
    if !ips.is_empty() {
        if let Ok(port_map) = probe_ports(&ips).await {
            for (ip, ports) in port_map {
                if let Some(dev) = devices.get_mut(&ip) {
                    dev.open_ports = ports;
                    dev.inferred_type = infer_type(&dev.open_ports, dev.mac.as_deref());
                }
            }
        }
    }
    set_progress(infra.as_ref(), mk_progress("ports", 3, &devices));

    // Phase 4: Hostname resolution via reverse DNS
    set_progress(infra.as_ref(), mk_progress("hostname", 4, &devices));
    for dev in devices.values_mut() {
        if let Ok(hostname) = resolve_hostname(&dev.ip).await {
            dev.hostname = Some(hostname);
        }
    }

    #[allow(clippy::cast_possible_truncation)]
    let scan_duration_ms = start.elapsed().as_millis() as u64;

    let results = DiscoveryResults {
        ts: super::now_iso(),
        devices: devices.into_values().collect(),
        scan_duration_ms,
    };

    // Stop the elapsed ticker
    elapsed_ticker.abort();

    // Mark scan complete
    set_progress(
        infra.as_ref(),
        DiscoveryProgress {
            active: false,
            phase: "complete".to_string(),
            phase_number: 4,
            total_phases: 4,
            hosts_found: results.devices.len(),
            devices: results.devices.clone(),
            started_at: Some(started_at),
            elapsed_ms: scan_duration_ms,
        },
    );

    info!(
        "Discovery complete: {} devices found in {scan_duration_ms}ms",
        results.devices.len()
    );

    Ok(Json(results))
}

/// Update discovery progress in shared InfraState (non-blocking).
fn set_progress(infra: Option<&Arc<Mutex<InfraState>>>, progress: DiscoveryProgress) {
    if let Some(infra) = infra {
        if let Ok(mut g) = infra.try_lock() {
            g.discovery_progress = progress;
        }
    }
}

// ─── Scan implementations ────────────────────────────────────────────

/// Auto-detect LAN subnets from network interfaces (public for routes module).
///
/// Looks for IPv4 addresses on bridge (br-lan, br-*) and LAN interfaces,
/// excluding WAN, loopback, WireGuard, and tunnel interfaces.
pub async fn auto_detect_subnets() -> Vec<String> {
    let output = checks::exec_simple_pub(
        "ip -4 addr show | awk '/^[0-9]+:/ {iface=$2} /inet / {print iface, $2}'",
        5000,
    )
    .await;

    let Ok(output) = output else {
        info!("Failed to detect LAN subnets");
        return Vec::new();
    };

    let skip_prefixes = ["lo:", "wg", "wwan", "docker", "veth", "tun"];

    let subnets: Vec<String> = output
        .1
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 2 {
                return None;
            }
            let iface = parts[0].trim_end_matches(':');
            let cidr = parts[1];

            // Skip non-LAN interfaces
            if skip_prefixes.iter().any(|p| iface.starts_with(p)) {
                return None;
            }
            // Skip /32 point-to-point and loopback
            if cidr.ends_with("/32") || cidr.starts_with("127.") {
                return None;
            }
            // Validate it's a real CIDR
            if checks::validate_cidr(cidr) {
                Some(cidr.to_string())
            } else {
                None
            }
        })
        .collect();

    info!("Auto-detected LAN subnets: {:?}", subnets);
    subnets
}

/// Parse the ARP table for IP→MAC mappings.
async fn scan_arp() -> Result<Vec<(String, String)>, String> {
    let output = checks::exec_simple_pub(
        "ip neigh show | grep -v FAILED | awk '{print $1, $5}'",
        5000,
    )
    .await?;

    let entries: Vec<(String, String)> = output
        .1
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 && parts[1].contains(':') {
                let ip = parts[0];
                // Skip IPv6 link-local — not useful for infrastructure discovery
                if ip.starts_with("fe80:") || ip.contains(':') {
                    return None;
                }
                Some((ip.to_string(), parts[1].to_string()))
            } else {
                None
            }
        })
        .collect();

    debug!("ARP scan: {} entries", entries.len());
    Ok(entries)
}

/// Ping sweep a subnet — tries nmap, falls back to busybox-compatible parallel ping.
async fn ping_sweep(subnet: &str) -> Result<Vec<String>, String> {
    if !checks::validate_cidr(subnet) {
        return Err(format!("invalid subnet CIDR: {subnet}"));
    }

    // Try nmap first (fast + reliable)
    let nmap_result = checks::exec_simple_pub(
        &format!("command -v nmap >/dev/null 2>&1 && nmap -sn -n {subnet} -oG - 2>/dev/null | grep 'Status: Up' | awk '{{print $2}}'"),
        60000,
    )
    .await;

    if let Ok(ref output) = nmap_result {
        let ips: Vec<String> = output
            .1
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.trim().to_string())
            .filter(|l| checks::validate_ipv4(l))
            .collect();
        if !ips.is_empty() {
            debug!("Ping sweep {subnet} (nmap): {} hosts up", ips.len());
            return Ok(ips);
        }
    }

    // Fallback: parallel ping using tokio — updates progress as hosts respond
    let base = subnet
        .split('/')
        .next()
        .unwrap_or("")
        .rsplit_once('.')
        .map_or("", |x| x.0);
    if base.is_empty() {
        return Err(format!("cannot extract base from subnet: {subnet}"));
    }

    let found = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));

    // Ping all 254 IPs in parallel via individual ping commands
    let mut handles = Vec::new();
    for i in 1..=254u16 {
        let ip = format!("{base}.{i}");
        let found = found.clone();
        handles.push(tokio::spawn(async move {
            let output = checks::exec_simple_pub(
                &format!("ping -c 1 -W 1 {ip} >/dev/null 2>&1 && echo UP"),
                3000,
            )
            .await;
            if let Ok(out) = output {
                if out.1.contains("UP") {
                    if let Ok(mut f) = found.lock() {
                        f.push(ip);
                    }
                }
            }
        }));
    }

    // Wait for all pings to complete
    for handle in handles {
        let _ = handle.await;
    }

    let ips = found.lock().map(|f| f.clone()).unwrap_or_default();
    debug!("Ping sweep {subnet} (ping): {} hosts up", ips.len());
    Ok(ips)
}

/// Probe common ports on a set of IPs using native TCP connects.
async fn probe_ports(ips: &[String]) -> Result<HashMap<String, Vec<u16>>, String> {
    let port_list: Vec<u16> = vec![22, 80, 443, 554, 8080, 8443, 161, 53, 3389, 1337];

    let mut result: HashMap<String, Vec<u16>> = HashMap::new();

    let mut handles = Vec::new();
    for ip in ips {
        let ip = ip.clone();
        let ports = port_list.clone();
        handles.push(tokio::spawn(async move {
            let mut open = Vec::new();
            let mut port_handles = Vec::new();
            for port in ports {
                let ip = ip.clone();
                port_handles.push(tokio::spawn(async move {
                    let addr = format!("{ip}:{port}");
                    let connect = tokio::net::TcpStream::connect(&addr);
                    match tokio::time::timeout(std::time::Duration::from_secs(2), connect).await {
                        Ok(Ok(_stream)) => Some(port),
                        _ => None,
                    }
                }));
            }
            for h in port_handles {
                if let Ok(Some(port)) = h.await {
                    open.push(port);
                }
            }
            open.sort_unstable();
            (ip, open)
        }));
    }

    for handle in handles {
        if let Ok((ip, ports)) = handle.await {
            if !ports.is_empty() {
                result.insert(ip, ports);
            }
        }
    }

    Ok(result)
}

/// Reverse DNS lookup (args-based, no shell interpretation).
async fn resolve_hostname(ip: &str) -> Result<String, String> {
    if !checks::validate_ipv4(ip) {
        return Err(format!("invalid IP: {ip}"));
    }
    // Use args-based execution to avoid shell injection
    let output = checks::exec_args_pub("nslookup", &[ip], 3000).await?;

    // Parse "name = hostname" from nslookup output
    let hostname = output
        .1
        .lines()
        .find(|l| l.contains("name ="))
        .and_then(|l| l.split("name =").nth(1))
        .map(|s| s.trim().trim_end_matches('.').to_string())
        .unwrap_or_default();

    if hostname.is_empty() {
        Err("no hostname".to_string())
    } else {
        Ok(hostname)
    }
}

/// Infer device type from open ports and MAC OUI.
fn infer_type(ports: &[u16], _mac: Option<&str>) -> String {
    // Port-based heuristics
    if ports.contains(&554) {
        return "camera".to_string(); // RTSP
    }
    if ports.contains(&80) && ports.contains(&443) && ports.contains(&22) {
        return "router".to_string();
    }
    if ports.contains(&161) && !ports.contains(&80) {
        return "switch".to_string(); // SNMP only, likely managed switch
    }
    if ports.contains(&3389) {
        return "server".to_string(); // RDP
    }
    if ports.contains(&80) || ports.contains(&443) {
        return "server".to_string();
    }

    // TODO: OUI-based inference using MAC address prefix database

    "other".to_string()
}
