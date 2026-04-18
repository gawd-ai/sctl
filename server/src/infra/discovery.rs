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
        if let Ok(entries) = auto_detect_subnets().await {
            subnets = entries.into_iter().map(|e| e.cidr).collect();
        }
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

/// A detected L3 subnet with its owning interface.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubnetEntry {
    pub iface: String,
    /// Network CIDR (normalized — host bits masked off).
    pub cidr: String,
    /// The agent's own IPv4 on this interface (host form). Used by the UI
    /// to cluster the agent's own IPs in scan results as a single entity.
    /// Empty when we learned the subnet without having a local address
    /// (e.g. via default-gateway inference on a downstream topology).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub host_ip: String,
    /// The interface's hardware address when one exists. `None` for pure
    /// L3 interfaces like `wg0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    /// How this subnet was learned. Lets the UI show provenance and the
    /// scanner choose the right probe strategy (e.g. `--arpspa=0.0.0.0`
    /// for `Route`-sourced subnets where the agent has no local IP).
    #[serde(default)]
    pub source: SubnetSource,
}

/// Provenance of a detected subnet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubnetSource {
    /// `ip -4 addr show` — the agent has an IPv4 address on this subnet.
    #[default]
    Addr,
    /// `ip -4 route show default` — the agent's default gateway sits here,
    /// but the agent has no local IP. Common in downstream-of-another-router
    /// topologies where the BPI is plugged into an existing LAN port.
    Route,
    /// Passive ARP sniff learned this subnet from observed traffic.
    Arp,
    /// A brief DHCP `DISCOVER` on an unbound port got an `OFFER` —
    /// the port is on an upstream LAN but isn't configured as a DHCP client.
    DhcpProbe,
}

/// Extract the host IPv4 from a host-form CIDR like `192.168.1.5/24`.
fn host_ip_of(cidr: &str) -> Option<String> {
    let (ip, _) = cidr.split_once('/')?;
    let _: std::net::Ipv4Addr = ip.parse().ok()?;
    Some(ip.to_string())
}

/// Read interface MAC addresses via `ip -br link show`. Returns a map
/// `iface → mac`. MAC is lowercased; interfaces without a MAC are skipped.
async fn iface_macs() -> std::collections::HashMap<String, String> {
    let Ok((code, stdout, _)) = checks::exec_simple_pub("ip -br link show", 5000).await else {
        return std::collections::HashMap::new();
    };
    if code != 0 {
        return std::collections::HashMap::new();
    }
    // `ip -br link show` format: `iface@alias  STATE  xx:xx:xx:xx:xx:xx  <flags>`
    // (alias is optional, separated by `@`). Take the iface name and the
    // first 17-char MAC-shaped token.
    stdout
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 3 {
                return None;
            }
            let iface = parts[0].split('@').next()?.to_string();
            let mac = parts.iter().find(|t| {
                t.len() == 17
                    && t.chars().enumerate().all(|(i, c)| match i % 3 {
                        2 => c == ':',
                        _ => c.is_ascii_hexdigit(),
                    })
            })?;
            let mac = mac.to_ascii_lowercase();
            if mac == "00:00:00:00:00:00" {
                return None;
            }
            Some((iface, mac))
        })
        .collect()
}

/// Normalize a host CIDR like `192.168.1.5/24` into the network form `192.168.1.0/24`.
fn normalize_cidr(cidr: &str) -> Option<String> {
    let (ip, prefix) = cidr.split_once('/')?;
    let ip: std::net::Ipv4Addr = ip.parse().ok()?;
    let prefix: u8 = prefix.parse().ok()?;
    if prefix > 32 {
        return None;
    }
    let mask = if prefix == 0 {
        0u32
    } else {
        u32::MAX << (32 - prefix)
    };
    let net = std::net::Ipv4Addr::from(u32::from(ip) & mask);
    Some(format!("{net}/{prefix}"))
}

/// Auto-detect L3 subnets from every UP interface with an IPv4 address
/// (public for routes module).
///
/// Includes physical LAN/WAN bridges, cellular (`wwan*`), and WireGuard
/// overlay peers (`wg*`). Skips loopback, docker bridges, veth pairs, and
/// legacy tunnel devices.
pub async fn auto_detect_subnets() -> Result<Vec<SubnetEntry>, String> {
    let (code, stdout, stderr) = checks::exec_simple_pub(
        "ip -4 addr show | awk '/^[0-9]+:/ {iface=$2} /inet / {print iface, $2}'",
        5000,
    )
    .await
    .map_err(|e| format!("ip command failed: {e}"))?;

    if code != 0 {
        return Err(format!(
            "ip command exited {code}: {}",
            stderr.trim().lines().next().unwrap_or("")
        ));
    }

    let skip_prefixes = ["lo", "docker", "veth", "tun"];
    let macs = iface_macs().await;

    let mut subnets: Vec<SubnetEntry> = stdout
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 2 {
                return None;
            }
            let iface = parts[0].trim_end_matches(':');
            let cidr = parts[1];

            // Skip loopback + container / legacy-tunnel noise
            if skip_prefixes.iter().any(|p| iface.starts_with(p)) {
                return None;
            }
            // Skip 127.0.0.0/8 regardless of iface
            if cidr.starts_with("127.") {
                return None;
            }
            if !checks::validate_cidr(cidr) {
                return None;
            }
            let normalized = normalize_cidr(cidr)?;
            let host_ip = host_ip_of(cidr).unwrap_or_default();
            let mac = macs.get(iface).cloned();
            Some(SubnetEntry {
                iface: iface.to_string(),
                cidr: normalized,
                host_ip,
                mac,
                source: SubnetSource::Addr,
            })
        })
        .collect();

    // Pass #8 Phase A — default-gateway subnet inference. Catches the
    // downstream topology where the BPI is plugged into an existing LAN port
    // and has no IP on the upstream subnet (e.g. ETH5 in br-lan cabled to a
    // WiFi router's LAN, WAN side unbound). `ip -4 route show default` tells
    // us the gateway exists + which iface reaches it; we assume /24 (the
    // overwhelmingly common home/SMB case) and surface it so the UI can
    // offer it as a scannable subnet.
    //
    // Dedup: skip any route candidate whose gateway IP is already covered by
    // an Addr-sourced subnet on the same iface (e.g. the LTE gateway sits
    // inside a /28 we already report — a second /24 entry there is spurious).
    let addr_entries: Vec<(String, String)> = subnets
        .iter()
        .map(|s| (s.iface.clone(), s.cidr.clone()))
        .collect();
    for (entry, gw_ip) in detect_route_subnets(&macs).await {
        let overlaps = addr_entries
            .iter()
            .any(|(iface, cidr)| *iface == entry.iface && cidr_contains(cidr, gw_ip));
        if !overlaps {
            subnets.push(entry);
        }
    }

    // Pass #8 Phase C — passive sniff. Listens on each candidate LAN iface
    // for a few seconds, parses sender IPs from ARP + common broadcast UDP
    // (DHCP, mDNS, SSDP, NetBIOS, UBNT discovery). Any /24 that doesn't
    // overlap an existing Addr/Route entry joins the list as source=Arp so
    // the UI shows it with a ⚡ observed badge.
    //
    // Catches the downstream-bridge topology the default-gateway inference
    // can't reach: BPI in br-lan bridging eth5 to a WiFi router's LAN, the
    // WiFi router advertising on 192.168.58.1 — passive capture sees that
    // broadcast even though the BPI has no L3 identity on that subnet.
    //
    // Bounded 5s. Silent no-op when tcpdump isn't installed so auto-detect
    // doesn't break on minimal BPIs.
    for (iface, cidr) in passive_sniff_subnets(5).await {
        let already_known = subnets.iter().any(|s| cidr_overlaps(&s.cidr, &cidr));
        if !already_known {
            let mac = macs.get(&iface).cloned();
            subnets.push(SubnetEntry {
                iface,
                cidr,
                host_ip: String::new(),
                mac,
                source: SubnetSource::Arp,
            });
        }
    }

    info!("Auto-detected LAN subnets: {:?}", subnets);
    Ok(subnets)
}

/// True iff two CIDRs share any addresses (either contains the other's
/// network address). Used to dedup sniff-learned /24s against Addr/Route
/// entries that might already cover the same range at a different prefix.
fn cidr_overlaps(a: &str, b: &str) -> bool {
    let parse = |c: &str| -> Option<std::net::Ipv4Addr> {
        let (ip, _) = c.split_once('/')?;
        ip.parse().ok()
    };
    match (parse(a), parse(b)) {
        (Some(ip_a), Some(ip_b)) => cidr_contains(a, ip_b) || cidr_contains(b, ip_a),
        _ => false,
    }
}

/// True iff `ip` falls within the network described by `cidr`.
fn cidr_contains(cidr: &str, ip: std::net::Ipv4Addr) -> bool {
    let Some((net, prefix)) = cidr.split_once('/') else {
        return false;
    };
    let Ok(net): Result<std::net::Ipv4Addr, _> = net.parse() else {
        return false;
    };
    let Ok(prefix): Result<u8, _> = prefix.parse() else {
        return false;
    };
    if prefix > 32 {
        return false;
    }
    let mask = if prefix == 0 {
        0u32
    } else {
        u32::MAX << (32 - prefix)
    };
    (u32::from(ip) & mask) == (u32::from(net) & mask)
}

/// Inspect `ip -4 route show default` and return `(SubnetEntry, gateway_ip)`
/// pairs. The caller uses the gateway IP to dedup against Addr-sourced
/// entries on the same iface (so an LTE gateway inside a /28 we already
/// have doesn't show up a second time as a redundant /24).
///
/// Gateway subnet is assumed `/24` — right for essentially all home/SMB
/// deployments; operators with non-/24 uplinks can still add the correct
/// CIDR manually in the UI.
async fn detect_route_subnets(
    macs: &std::collections::HashMap<String, String>,
) -> Vec<(SubnetEntry, std::net::Ipv4Addr)> {
    let Ok((code, stdout, _)) = checks::exec_simple_pub("ip -4 route show default", 3000).await
    else {
        return Vec::new();
    };
    if code != 0 {
        return Vec::new();
    }

    let mut out = Vec::new();
    for line in stdout.lines() {
        // Canonical form: `default via 192.168.58.1 dev br-lan proto dhcp ...`
        let parts: Vec<&str> = line.split_whitespace().collect();
        let mut gw: Option<&str> = None;
        let mut iface: Option<&str> = None;
        let mut i = 0;
        while i + 1 < parts.len() {
            match parts[i] {
                "via" => gw = Some(parts[i + 1]),
                "dev" => iface = Some(parts[i + 1]),
                _ => {}
            }
            i += 1;
        }
        let Some(gw) = gw else { continue };
        let Some(iface) = iface else { continue };

        // Skip tunnels / loopback for the same reasons as the addr path.
        let skip_prefixes = ["lo", "docker", "veth", "tun"];
        if skip_prefixes.iter().any(|p| iface.starts_with(p)) {
            continue;
        }

        let gw_ip: std::net::Ipv4Addr = match gw.parse() {
            Ok(ip) => ip,
            Err(_) => continue,
        };
        let host_form = format!("{gw_ip}/24");
        let Some(normalized) = normalize_cidr(&host_form) else {
            continue;
        };
        let mac = macs.get(iface).cloned();
        out.push((
            SubnetEntry {
                iface: iface.to_string(),
                cidr: normalized,
                host_ip: String::new(),
                mac,
                source: SubnetSource::Route,
            },
            gw_ip,
        ));
    }
    out
}

/// Look up the iface and host IP the agent uses to reach a subnet.
/// Returns `None` when the CIDR isn't in any auto-detected entry. `host_ip`
/// will be empty for Route-sourced subnets the agent has no local IP on.
async fn find_subnet_iface(cidr: &str) -> Option<(String, String)> {
    let entries = auto_detect_subnets().await.ok()?;
    entries
        .into_iter()
        .find(|e| e.cidr == cidr)
        .map(|e| (e.iface, e.host_ip))
}

/// Candidate LAN interfaces for source-any ARP scanning. Returns every UP
/// bridge or physical ethernet that isn't a loopback / docker / veth / tun.
/// Used when a manually-entered subnet isn't in auto-detect — we don't know
/// which iface reaches it, so we try them in order and take the first that
/// returns hosts. Bridges are preferred because they reach every enslaved
/// port with a single scan.
async fn candidate_lan_ifaces() -> Vec<String> {
    let Ok((code, stdout, _)) = checks::exec_simple_pub("ip -br link show", 5000).await else {
        return Vec::new();
    };
    if code != 0 {
        return Vec::new();
    }
    let skip_prefixes = ["lo", "docker", "veth", "tun", "wg", "wwan"];
    let mut bridges = Vec::new();
    let mut physical = Vec::new();
    for line in stdout.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 2 {
            continue;
        }
        let Some(iface) = parts[0].split('@').next() else {
            continue;
        };
        if skip_prefixes.iter().any(|p| iface.starts_with(p)) {
            continue;
        }
        let state = parts[1];
        if state != "UP" && state != "UNKNOWN" {
            continue;
        }
        if iface.starts_with("br") || iface.starts_with("bond") {
            bridges.push(iface.to_string());
        } else if iface.starts_with("eth") || iface.starts_with("en") || iface.starts_with("lan") {
            physical.push(iface.to_string());
        }
    }
    bridges.extend(physical);
    bridges
}

/// Passive sniff on every candidate LAN iface for `secs` seconds. Returns
/// `(iface, cidr/24)` tuples for every distinct /24 observed from sender
/// IPs in ARP + common broadcast UDP protocols (DHCP, mDNS, SSDP, NetBIOS,
/// UBNT discovery). Silent no-op when `tcpdump` isn't installed.
///
/// Sniffs run per-iface in parallel (Linux kernels don't support promisc
/// mode on the `any` pseudo-iface). Each iface's capture is bounded by
/// wall-clock `secs + 2` via the exec timeout, so total auto-detect
/// latency is ~`secs + 1` regardless of iface count.
async fn passive_sniff_subnets(secs: u64) -> Vec<(String, String)> {
    let ifaces = candidate_lan_ifaces().await;
    if ifaces.is_empty() {
        return Vec::new();
    }

    let handles: Vec<_> = ifaces
        .into_iter()
        .map(|iface| tokio::spawn(async move { sniff_iface(iface, secs).await }))
        .collect();

    let mut out: Vec<(String, String)> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for h in handles {
        if let Ok(observations) = h.await {
            for (iface, cidr) in observations {
                let key = format!("{iface}|{cidr}");
                if seen.insert(key) {
                    out.push((iface, cidr));
                }
            }
        }
    }
    out
}

/// Sniff a single iface for `secs` seconds. Parses sender IPs from tcpdump
/// text output and groups by /24. Reserved / link-local / multicast sender
/// IPs are filtered out.
async fn sniff_iface(iface: String, secs: u64) -> Vec<(String, String)> {
    if !iface
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Vec::new();
    }

    let filter = "arp or (udp and (port 67 or port 68 or port 5353 or port 1900 or port 137 or port 138 or portrange 5245-5246))";
    let tmp = format!("/tmp/sctl-sniff-{iface}.out");
    let cmd = format!(
        "command -v tcpdump >/dev/null 2>&1 && \
         ( tcpdump -i {iface} -nn -l -c 200 '{filter}' > {tmp} 2>/dev/null & \
           P=$!; sleep {secs}; kill -TERM $P 2>/dev/null; wait $P 2>/dev/null; \
           cat {tmp} 2>/dev/null; rm -f {tmp} )"
    );

    let exec_timeout_ms = secs * 1000 + 3000;
    let Ok((_code, stdout, _stderr)) = checks::exec_simple_pub(&cmd, exec_timeout_ms).await else {
        return Vec::new();
    };

    let mut cidrs: std::collections::HashSet<String> = std::collections::HashSet::new();
    for line in stdout.lines() {
        if let Some(ip) = parse_sender_ip(line) {
            if !is_usable_sender(ip) {
                continue;
            }
            let octets = ip.octets();
            let net = std::net::Ipv4Addr::new(octets[0], octets[1], octets[2], 0);
            cidrs.insert(format!("{net}/24"));
        }
    }
    cidrs.into_iter().map(|c| (iface.clone(), c)).collect()
}

/// Extract the most plausible sender IPv4 from one tcpdump text line.
/// Handles both `IP <sender>[.<port>] > <dest>[.<port>]: ...` and ARP's
/// `Request who-has <tgt> tell <sender>` / `Reply <sender> is-at ...` forms.
fn parse_sender_ip(line: &str) -> Option<std::net::Ipv4Addr> {
    // ARP "tell <sender>" — the sender is unambiguous
    if let Some(rest) = line.split(" tell ").nth(1) {
        if let Some(ip_token) = rest.split(|c: char| c == ',' || c.is_whitespace()).next() {
            if let Ok(ip) = ip_token.parse() {
                return Some(ip);
            }
        }
    }
    // ARP "Reply <sender> is-at ..." — sender is the first field after Reply
    if let Some(rest) = line.split("ARP, Reply ").nth(1) {
        if let Some(ip_token) = rest.split_whitespace().next() {
            if let Ok(ip) = ip_token.parse() {
                return Some(ip);
            }
        }
    }
    // "IP <sender>[.<port>] > ..." form (UDP / most broadcast chatter)
    if let Some(rest) = line.strip_prefix("IP ").or_else(|| {
        // Sometimes prefixed with a timestamp like "19:33:49.376452 IP ..."
        line.split(" IP ").nth(1)
    }) {
        if let Some(token) = rest.split_whitespace().next() {
            // Strip trailing port "192.168.58.1.5246" → "192.168.58.1"
            let ip_str = token.rsplit_once('.').map_or(token, |(prefix, _)| prefix);
            if let Ok(ip) = ip_str.parse() {
                return Some(ip);
            }
        }
    }
    None
}

/// Filter sender IPs that wouldn't identify a useful subnet. Excludes
/// loopback, link-local, multicast, broadcast, and the all-zeros address
/// that ARP probes sometimes use.
fn is_usable_sender(ip: std::net::Ipv4Addr) -> bool {
    let o = ip.octets();
    if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() || ip.is_broadcast() {
        return false;
    }
    // 169.254.0.0/16 link-local APIPA
    if o[0] == 169 && o[1] == 254 {
        return false;
    }
    true
}

/// Source-any ARP scan for a subnet the agent has no IP on. Uses `arp-scan`
/// with `--arpspa=0.0.0.0` so replies route back to our MAC even though our
/// L3 identity isn't on this subnet. Requires the `arp-scan` binary; silently
/// returns empty if it's not installed so scans don't break on minimal BPIs.
async fn arp_scan_addressless(iface: &str, cidr: &str) -> Result<Vec<String>, String> {
    if !checks::validate_cidr(cidr) {
        return Err(format!("invalid cidr: {cidr}"));
    }
    if !iface
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(format!("invalid iface: {iface}"));
    }

    let cmd = format!(
        "command -v arp-scan >/dev/null 2>&1 && \
         arp-scan --interface={iface} --arpspa=0.0.0.0 \
                  --destaddr=ff:ff:ff:ff:ff:ff --retry=2 --timeout=300 {cidr} \
                  2>/dev/null | awk '/^[0-9]+\\./ {{print $1}}'"
    );
    let (_code, stdout, _stderr) = checks::exec_simple_pub(&cmd, 60_000).await?;
    let ips: Vec<String> = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
        .filter(|l| checks::validate_ipv4(l))
        .collect();
    Ok(ips)
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
///
/// For subnets the agent has no IPv4 address on (Pass #8 Phase A route-
/// inferred subnets, downstream-topology deployments), if the standard
/// probes come up empty we follow up with a source-any ARP scan —
/// `arp-scan --arpspa=0.0.0.0` forges source IP 0.0.0.0 in the ARP request
/// so neighbours still reply even though the BPI isn't participating in the
/// subnet L3-wise.
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

    // Source-any ARP scan fallback. Two distinct cases:
    //   1. Auto-detect knows this CIDR but host_ip is empty (Route-sourced /
    //      downstream topology) — scan on that iface.
    //   2. Auto-detect doesn't know the CIDR at all (user manually entered an
    //      upstream subnet that only the bridge-forwarding side sees; e.g.
    //      BPI plugged into a WiFi router's LAN, scanning the WiFi's /24).
    //      Iterate candidate LAN interfaces and return the first that finds
    //      hosts. Bridges go first since they reach every enslaved port.
    match find_subnet_iface(subnet).await {
        Some((iface, host_ip)) if host_ip.is_empty() => {
            if let Ok(ips) = arp_scan_addressless(&iface, subnet).await {
                if !ips.is_empty() {
                    debug!(
                        "Ping sweep {subnet} (arp-scan addressless via {iface}): {} hosts up",
                        ips.len()
                    );
                    return Ok(ips);
                }
            }
        }
        None => {
            for iface in candidate_lan_ifaces().await {
                if let Ok(ips) = arp_scan_addressless(&iface, subnet).await {
                    if !ips.is_empty() {
                        debug!(
                            "Ping sweep {subnet} (arp-scan manual subnet via {iface}): {} hosts up",
                            ips.len()
                        );
                        return Ok(ips);
                    }
                }
            }
        }
        _ => {}
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
