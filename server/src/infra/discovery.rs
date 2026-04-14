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
    let subnets: Vec<String> = payload
        .get("subnets")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    let infra = state.infra_state.clone();
    info!("Starting LAN discovery scan (subnets: {:?})", subnets);
    let start = std::time::Instant::now();
    let started_at = super::now_iso();

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
    set_progress(&infra, mk_progress("arp", 1, &HashMap::new()));
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
    set_progress(&infra, mk_progress("arp", 1, &devices));

    // Phase 2: Ping sweep (if subnets provided)
    set_progress(&infra, mk_progress("ping", 2, &devices));
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
    }
    set_progress(&infra, mk_progress("ping", 2, &devices));

    // Phase 3: Port probe on discovered IPs
    set_progress(&infra, mk_progress("ports", 3, &devices));
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
    set_progress(&infra, mk_progress("ports", 3, &devices));

    // Phase 4: Hostname resolution via reverse DNS
    set_progress(&infra, mk_progress("hostname", 4, &devices));
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

    // Mark scan complete
    set_progress(
        &infra,
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
fn set_progress(infra: &Option<Arc<Mutex<InfraState>>>, progress: DiscoveryProgress) {
    if let Some(ref infra) = infra {
        if let Ok(mut g) = infra.try_lock() {
            g.discovery_progress = progress;
        }
    }
}

// ─── Scan implementations ────────────────────────────────────────────

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
                Some((parts[0].to_string(), parts[1].to_string()))
            } else {
                None
            }
        })
        .collect();

    debug!("ARP scan: {} entries", entries.len());
    Ok(entries)
}

/// Ping sweep a subnet using nmap or fping.
async fn ping_sweep(subnet: &str) -> Result<Vec<String>, String> {
    if !checks::validate_cidr(subnet) {
        return Err(format!("invalid subnet CIDR: {subnet}"));
    }
    // Try nmap first, fall back to manual ping
    let output = checks::exec_simple_pub(
        &format!(
            "nmap -sn -n {subnet} -oG - 2>/dev/null | grep 'Status: Up' | awk '{{print $2}}' || \
             (for i in $(seq 1 254); do \
                ip=$(echo {subnet} | sed 's|/.*||' | sed 's/\\.[0-9]*$//').$i; \
                ping -c 1 -W 1 $ip >/dev/null 2>&1 && echo $ip & \
             done; wait)"
        ),
        60000,
    )
    .await?;

    let ips: Vec<String> = output
        .1
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
        .filter(|l| l.chars().all(|c| c.is_ascii_digit() || c == '.'))
        .collect();

    debug!("Ping sweep {subnet}: {} hosts up", ips.len());
    Ok(ips)
}

/// Probe common ports on a set of IPs.
async fn probe_ports(ips: &[String]) -> Result<HashMap<String, Vec<u16>>, String> {
    for ip in ips {
        if !checks::validate_ipv4(ip) {
            return Err(format!("invalid IP in probe list: {ip}"));
        }
    }

    let ports = "22,80,443,554,8080,8443,161,53,3389";

    // Use args-based execution to avoid shell injection on IP list
    let ip_refs: Vec<&str> = ips.iter().map(|s| s.as_str()).collect();
    let mut args: Vec<&str> = vec!["-p", ports, "--open", "-n", "-oG", "-"];
    args.extend(&ip_refs);

    let Ok(output) = checks::exec_args_pub("nmap", &args, 60000).await else {
        return Ok(HashMap::new()); // nmap not available, skip
    };

    let mut result: HashMap<String, Vec<u16>> = HashMap::new();

    // Parse nmap greppable output: "Host: 192.168.1.1 ()	Ports: 22/open/tcp//ssh///, 80/open/tcp//http///"
    for line in output.1.lines() {
        if !line.starts_with("Host:") || !line.contains("Ports:") {
            continue;
        }
        let ip = line.split_whitespace().nth(1).unwrap_or("").to_string();

        let ports_part = line.split("Ports:").nth(1).unwrap_or("");
        let open_ports: Vec<u16> = ports_part
            .split(',')
            .filter_map(|p| {
                let port_str = p.trim().split('/').next()?;
                if p.contains("open") {
                    port_str.parse().ok()
                } else {
                    None
                }
            })
            .collect();

        if !open_ports.is_empty() {
            result.insert(ip, open_ports);
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
