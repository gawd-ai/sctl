//! LAN device discovery: the neighbour table, a sweep, a port probe and
//! reverse lookups.
//!
//! Triggered on demand via `POST /api/infra/discover`. Results are cached
//! in `InfraState` until the next scan.
//!
//! A scan sends only on the unit's LAN side, as the net tools' guard
//! ([`crate::net_tools::guard`]) judges it (ADR-012 decisions 10 to 13):
//!
//! - **What is swept.** With no subnets in the request, the networks the
//!   unit holds an address on, on LAN-side interfaces: never its LTE or its
//!   tunnels, nor a /24 guessed from a default route or heard by the sniff.
//!   nmap and the ping fallback sweep a subnet only when it lies wholly
//!   under a no-gateway main-table route through a LAN-side interface
//!   holding an address, and only on that interface. Any other subnet is
//!   `not_swept` as `not_on_lan`, and arp-scan, which stays on the link,
//!   searches it on the LAN-side interfaces. arp-scan runs only on a subnet
//!   it can search within its limit, a /19 at most. An entry of a request
//!   that is not an IPv4 network is `not_swept` as `not_ipv4` (an IPv6 one)
//!   or `invalid`.
//! - **The port probe** judges each host by the guard and connects from a
//!   socket bound where the guard approved. The guard refuses the unit's
//!   own addresses (`own_address`), so they get none.
//! - **A host's MAC** is the one seen answering during the scan: the
//!   sweep's ARP answer, and the kernel's entry on the interface that
//!   reaches the host, REACHABLE (or permanent) once the port probe is
//!   done; with neither, the answer to one ARP request. An address two MACs
//!   answered for gets none, whether the sweep heard both or the entry
//!   names another MAC than the sweep's.
//! - **The unit itself.** A host at an address the unit holds, or with one
//!   of its MACs, carries `own`, the name of that interface.
//!
//! Every probe goes through the `Probes` trait, so a test runs a scan with
//! fakes and sends nothing.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io;
use std::net::Ipv4Addr;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{extract::State, http::StatusCode, Json};
use futures_util::future::{join_all, BoxFuture};
use futures_util::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use super::checks;
use super::{DiscoveryProgress, InfraState};
use crate::error::codes;
use crate::net_tools::guard::{self, Approved, Iface, Lte, Mac, Network, Role};
use crate::netwatch::netlink::{self, Neighbour, Route};
use crate::AppState;

/// The ports the probe tries on every host found.
const PORTS: [u16; 10] = [22, 80, 443, 554, 8080, 8443, 161, 53, 3389, 1337];
/// How long the probe waits for one port.
const PORT_TIMEOUT: Duration = Duration::from_secs(2);
/// The most hosts the ping fallback pings in one subnet: a /24's, as many
/// as it pinged before 0.6.12.
const FALLBACK_MAX: u64 = 254;
/// Probes in flight at once: the fallback's pings, the hosts of the port
/// probe (ten connects each), the ARP requests. 254 shells at once on a
/// one-core unit is the load the monitor's semaphore avoids elsewhere.
const AT_ONCE: usize = 32;
/// nmap's and arp-scan's limit, and one ping's.
const SWEEP_TIMEOUT_MS: u64 = 60_000;
const PING_TIMEOUT_MS: u64 = 3_000;
/// The most hosts arp-scan is asked to search in one subnet: a /19's. It
/// paces its requests to 256 kbit/s, 380 to 500 a second, and asks each
/// silent host twice (`--retry=2`), so a /19 takes 33 to 43 s, within
/// [`SWEEP_TIMEOUT_MS`]. A /18 would not end in time, and a run cut at the
/// limit keeps none of its answers: it would only fill the LAN with ARP
/// broadcasts.
const ARP_SCAN_MAX: u64 = 8190;
/// The packets the passive sniff takes on one interface at most: tcpdump
/// stops after them (`-c`), and the reader after as many lines.
const SNIFF_PACKETS: usize = 200;
/// ARP and the broadcast UDP protocols the passive sniff listens for: DHCP,
/// mDNS, SSDP, NetBIOS and Ubiquiti's discovery.
const SNIFF_FILTER: &str = "arp or (udp and (port 67 or port 68 or port 5353 or port 1900 or port 137 or port 138 or portrange 5245-5246))";
/// The most a listened-to child's stdout is read: 200 of tcpdump's lines
/// take a few tens of KiB.
const LISTEN_BYTES: u64 = 256 * 1024;
/// Interfaces discovery leaves out by name, whatever their role: container
/// bridges, veth pairs and legacy tunnels. The loopback is left out by its
/// flag.
const SKIPPED: [&str; 3] = ["docker", "veth", "tun"];

/// A discovered LAN device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredDevice {
    pub ip: String,
    /// A MAC seen answering at `ip` during the scan; none when nothing
    /// answered with one, or two MACs did.
    pub mac: Option<String>,
    pub hostname: Option<String>,
    pub open_ports: Vec<u16>,
    pub inferred_type: String,
    pub mdns_services: Vec<String>,
    /// The unit itself: the interface that holds `ip`, or the one whose MAC
    /// answered for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub own: Option<String>,
}

/// Why a scan did not sweep a subnet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotSweptReason {
    /// It does not lie wholly under a no-gateway route through a LAN-side
    /// interface holding an address: searched by ARP alone, on the LAN
    /// side, when it is no larger than a /19.
    NotOnLan,
    /// It holds more hosts than the ping fallback pings, and neither nmap
    /// nor arp-scan swept it: arp-scan searches at most a /19.
    TooLarge,
    /// An IPv6 address or network: a scan sweeps IPv4 networks only.
    NotIpv4,
    /// Not a network in `a.b.c.d/p` form: no prefix, a prefix over 32, a
    /// word.
    Invalid,
}

/// A subnet the scan did not sweep, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotSwept {
    /// In network form; as the request gave it when it is not an IPv4
    /// network.
    pub cidr: String,
    pub reason: NotSweptReason,
}

/// Discovery scan results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryResults {
    pub ts: String,
    pub devices: Vec<DiscoveredDevice>,
    pub scan_duration_ms: u64,
    /// Each subnet the scan did not sweep, and why.
    #[serde(default)]
    pub not_swept: Vec<NotSwept>,
}

/// `POST /api/infra/discover`: run a LAN scan.
///
/// Takes an optional `subnets` array in the request body. With none, or an
/// empty one, it sweeps the networks the unit holds an address on, on
/// LAN-side interfaces (ADR-012 decision 10); a subnet the request names is
/// swept only where the guard lets it be, and `not_swept` says which were
/// not, an entry that is not an IPv4 network included. Answers
/// `503 NET_UNAVAILABLE` when the unit's network cannot be read: nothing is
/// judged, so nothing is sent.
pub async fn discover(
    State(state): State<AppState>,
    Json(payload): Json<Value>,
) -> Result<Json<DiscoveryResults>, (StatusCode, Json<Value>)> {
    let probes = AgentProbes {
        lte: Lte::from_config(Some(&state.config)),
    };
    discover_with(&probes, state.infra_state.clone(), &payload).await
}

/// [`discover`] with the probes given: the agent's own, or a test's fakes.
/// It reads the request, runs the scan and keeps its progress in `infra`.
async fn discover_with(
    probes: &dyn Probes,
    infra: Option<Arc<Mutex<InfraState>>>,
    payload: &Value,
) -> Result<Json<DiscoveryResults>, (StatusCode, Json<Value>)> {
    let requested = requested_entries(payload);
    info!(
        "Starting LAN discovery scan (subnets: {requested:?}; none: the unit's own LAN networks)"
    );
    let progress = Progress {
        infra: infra.clone(),
        start: Instant::now(),
        started_at: super::now_iso(),
    };

    // Background task: tick elapsed_ms every 500ms so progress endpoint stays fresh
    let elapsed_ticker = {
        let infra = infra.clone();
        let start = progress.start;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                if let Some(ref infra) = infra {
                    if let Ok(mut g) = infra.try_lock() {
                        if g.discovery_progress.active {
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

    let scanned = scan(probes, &requested, &progress).await;
    elapsed_ticker.abort();
    let scan_duration_ms = progress.start.elapsed().as_millis() as u64;

    let scanned = match scanned {
        Ok(scanned) => scanned,
        Err(e) => {
            warn!("Discovery: {e}");
            set_progress(
                infra.as_ref(),
                DiscoveryProgress {
                    active: false,
                    phase: "error".to_string(),
                    phase_number: 0,
                    total_phases: 4,
                    hosts_found: 0,
                    devices: Vec::new(),
                    started_at: Some(progress.started_at.clone()),
                    elapsed_ms: scan_duration_ms,
                },
            );
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": e, "code": codes::NET_UNAVAILABLE, "message": e})),
            ));
        }
    };

    let results = DiscoveryResults {
        ts: super::now_iso(),
        devices: scanned.devices,
        scan_duration_ms,
        not_swept: scanned.not_swept,
    };

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
            started_at: Some(progress.started_at.clone()),
            elapsed_ms: scan_duration_ms,
        },
    );

    info!(
        "Discovery complete: {} devices found in {scan_duration_ms}ms, {} subnets not swept",
        results.devices.len(),
        results.not_swept.len()
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

/// Where a scan reports its phases: the Infra state's progress, which the
/// progress route reads, or nowhere.
pub(crate) struct Progress {
    infra: Option<Arc<Mutex<InfraState>>>,
    start: Instant,
    started_at: String,
}

impl Progress {
    fn phase(&self, phase: &str, number: u8, hosts: &BTreeMap<Ipv4Addr, Host>) {
        set_progress(
            self.infra.as_ref(),
            DiscoveryProgress {
                active: true,
                phase: phase.to_string(),
                phase_number: number,
                total_phases: 4,
                hosts_found: hosts.len(),
                devices: devices(hosts),
                started_at: Some(self.started_at.clone()),
                elapsed_ms: self.start.elapsed().as_millis() as u64,
            },
        );
    }
}

// ─── Networks ────────────────────────────────────────────────────────

/// An IPv4 network: an address with its host bits clear, and a prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Cidr {
    net: Ipv4Addr,
    prefix: u8,
}

/// The netmask of a prefix, as a number.
fn mask(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix.min(32)))
    }
}

impl Cidr {
    /// The network `ip` lies in under `prefix`; None for a prefix over 32.
    pub fn new(ip: Ipv4Addr, prefix: u8) -> Option<Self> {
        (prefix <= 32).then(|| Self {
            net: Ipv4Addr::from(u32::from(ip) & mask(prefix)),
            prefix,
        })
    }

    /// `a.b.c.d/p`, host bits allowed and cleared.
    pub fn parse(text: &str) -> Option<Self> {
        let (ip, prefix) = text.trim().split_once('/')?;
        Self::new(ip.parse().ok()?, prefix.parse().ok()?)
    }

    pub fn contains(self, ip: Ipv4Addr) -> bool {
        u32::from(ip) & mask(self.prefix) == u32::from(self.net)
    }

    /// Whether every address of `inner` lies in this network.
    fn covers(self, inner: Self) -> bool {
        self.prefix <= inner.prefix && self.contains(inner.net)
    }

    /// Whether the two share an address.
    fn overlaps(self, other: Self) -> bool {
        self.covers(other) || other.covers(self)
    }

    /// How many hosts it holds: every address but the network and the
    /// broadcast one, which a /31 and a /32 do not have.
    fn host_count(self) -> u64 {
        match self.prefix {
            32 => 1,
            31 => 2,
            prefix => (1u64 << (32 - prefix)) - 2,
        }
    }

    /// Its hosts, in order.
    fn hosts(self) -> impl Iterator<Item = Ipv4Addr> {
        let first = u64::from(u32::from(self.net));
        let size = 1u64 << (32 - self.prefix);
        let (from, to) = if self.prefix >= 31 {
            (0, size)
        } else {
            (1, size - 1)
        };
        (from..to).map(move |i| Ipv4Addr::from((first + i) as u32))
    }
}

impl fmt::Display for Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.net, self.prefix)
    }
}

/// A route's destination as a network.
fn route_net(route: &Route) -> Option<Cidr> {
    Cidr::new(route.dst.unwrap_or(Ipv4Addr::UNSPECIFIED), route.dst_len)
}

/// Whether discovery lists `iface` and uses it: not the loopback, and not
/// a container bridge, a veth or a legacy tunnel by its name.
fn listed(iface: &Iface) -> bool {
    !iface.loopback() && !SKIPPED.iter().any(|p| iface.name.starts_with(p))
}

/// The interface's MAC as the subnet list gives it: none for a link
/// without one, a tunnel's, or all zeros.
fn mac_of(iface: &Iface) -> Option<String> {
    iface
        .mac
        .filter(|m| *m != [0; 6])
        .map(|m| Mac(m).to_string())
}

/// The LAN side as discovery uses it: every up `lan` interface it lists,
/// bridges and bonds first, their ports left to them. arp-scan and the
/// passive sniff run on these.
fn lan_ifaces(net: &Network) -> Vec<&Iface> {
    let masters: Vec<&str> = net
        .ifaces
        .iter()
        .filter_map(|i| i.port_of.as_deref())
        .collect();
    let mut lan: Vec<&Iface> = net
        .ifaces
        .iter()
        .filter(|i| i.lan_side() && listed(i) && !i.enslaved())
        .collect();
    lan.sort_by_key(|i| !masters.contains(&i.name.as_str()));
    lan
}

/// The networks a scan given no subnets sweeps (ADR-012 decision 10): each
/// one an up `lan` interface holds an address on, each once.
fn default_subnets(net: &Network) -> Vec<Cidr> {
    let mut out = Vec::new();
    for iface in net.ifaces.iter().filter(|i| i.lan_side() && listed(i)) {
        for &(ip, prefix) in &iface.ipv4 {
            match Cidr::new(ip, prefix) {
                Some(cidr) if !ip.is_loopback() && !out.contains(&cidr) => out.push(cidr),
                _ => {}
            }
        }
    }
    out
}

/// The entries of a request's `subnets`, each read on its own: a string as
/// given, anything else as its JSON text, which names no network, so
/// `not_swept` names it as `invalid`. Absent or null, none: the default
/// pick. A value that is not a list is one entry. A list that held one
/// entry of another type used to read as no list at all, and the scan then
/// swept the default networks, naming nothing it was asked for.
fn requested_entries(payload: &Value) -> Vec<String> {
    let text = |entry: &Value| {
        entry
            .as_str()
            .map_or_else(|| entry.to_string(), str::to_string)
    };
    match payload.get("subnets") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(entries)) => entries.iter().map(text).collect(),
        Some(other) => vec![text(other)],
    }
}

/// The subnets a request names, in network form, each once, and each entry
/// that is not an IPv4 network, once as given, with why (ADR-012 decision
/// 10: `not_swept` names every subnet the scan did not sweep).
fn named_subnets(requested: &[String]) -> (Vec<Cidr>, Vec<NotSwept>) {
    let mut subnets = Vec::new();
    let mut unread: Vec<NotSwept> = Vec::new();
    for text in requested {
        match Cidr::parse(text) {
            Some(cidr) if !subnets.contains(&cidr) => subnets.push(cidr),
            Some(_) => {}
            None if unread.iter().any(|n| n.cidr == *text) => {}
            None => {
                let reason = if ipv6_entry(text) {
                    NotSweptReason::NotIpv4
                } else {
                    NotSweptReason::Invalid
                };
                debug!("Discovery: {text:?} is not an IPv4 network ({reason:?}), not swept");
                unread.push(NotSwept {
                    cidr: text.clone(),
                    reason,
                });
            }
        }
    }
    (subnets, unread)
}

/// Whether a request's entry is an IPv6 address, or an IPv6 network with a
/// prefix of at most 128.
fn ipv6_entry(text: &str) -> bool {
    let text = text.trim();
    let (ip, prefix) = match text.split_once('/') {
        Some((ip, prefix)) => (ip, Some(prefix)),
        None => (text, None),
    };
    ip.parse::<std::net::Ipv6Addr>().is_ok()
        && prefix.is_none_or(|p| p.parse::<u8>().is_ok_and(|p| p <= 128))
}

/// Where nmap and the ping fallback may sweep `subnet` (ADR-012 decision
/// 10): the up `lan` interface holding an address whose no-gateway
/// main-table route the kernel takes for every address of it. None when the
/// best route to the subnet, or to any part of it, has a gateway, leaves by
/// another interface or is not unicast.
fn sweep_iface(net: &Network, subnet: Cidr) -> Option<&Iface> {
    let routes = net.routes();
    let best = routes
        .iter()
        .filter(|r| route_net(r).is_some_and(|n| n.covers(subnet)))
        .min_by_key(|r| (Reverse(r.dst_len), r.priority))?;
    let oif = best.oif?;
    let on_link =
        |r: &Route| r.kind == netlink::RTN_UNICAST && r.gateway.is_none() && r.oif == Some(oif);
    let part_elsewhere = routes.iter().any(|r| {
        r.dst_len > subnet.prefix && route_net(r).is_some_and(|n| subnet.covers(n)) && !on_link(r)
    });
    if !on_link(best) || part_elsewhere {
        return None;
    }
    net.iface_at(oif)
        .filter(|i| i.lan_side() && !i.ipv4.is_empty())
}

/// Every address the unit holds.
fn own_addresses(net: &Network) -> Vec<Ipv4Addr> {
    net.ifaces
        .iter()
        .flat_map(|i| i.ipv4.iter().map(|(ip, _)| *ip))
        .collect()
}

/// The interface that makes `ip` the unit's own (ADR-012 decision 12): the
/// one holding it, else the one whose MAC answered for it, a bridge before
/// the port it shares its MAC with.
fn own_iface(net: &Network, ip: Ipv4Addr, mac: Option<Mac>) -> Option<String> {
    if let Some(holder) = net.holder_of(ip) {
        return Some(holder.name.clone());
    }
    let mac = mac.filter(|m| m.0 != [0; 6])?;
    let holders: Vec<&Iface> = net
        .ifaces
        .iter()
        .filter(|i| !i.loopback() && i.mac == Some(mac.0))
        .collect();
    holders
        .iter()
        .find(|i| !i.enslaved())
        .or(holders.first())
        .map(|i| i.name.clone())
}

// ─── The subnet list ─────────────────────────────────────────────────

/// A detected L3 subnet with its owning interface.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubnetEntry {
    pub iface: String,
    /// Network CIDR, normalized: host bits masked off.
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
    /// How this subnet was learned, for the UI. A scan does not read it:
    /// it judges each subnet by the unit's routes, and searches one off
    /// the LAN side by ARP alone from 0.0.0.0, whatever its source (ADR-012
    /// decision 10).
    #[serde(default)]
    pub source: SubnetSource,
    /// The interface's role by the net tools' guard (ADR-012 decision 2):
    /// `lte` for the agent's `[lte] interface` (with none the unit has, a
    /// modem bearer's name), `tunnel` for a point-to-point or no-ARP link,
    /// else `lan`. A scan given no subnets sweeps the `addr` networks of up
    /// `lan` interfaces.
    pub role: Role,
}

/// Provenance of a detected subnet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubnetSource {
    /// The agent has an IPv4 address on this subnet.
    #[default]
    Addr,
    /// A default route's gateway sits here, but the agent has no local IP.
    /// Common in downstream-of-another-router topologies where the BPI is
    /// plugged into an existing LAN port.
    Route,
    /// Passive ARP sniff learned this subnet from observed traffic.
    Arp,
    /// A brief DHCP `DISCOVER` on an unbound port got an `OFFER`: the
    /// port is on an upstream LAN but isn't configured as a DHCP client.
    DhcpProbe,
}

/// The unit's subnets, each with its role by the agent's `[lte] interface`
/// (`lte`) and its source: every IPv4 network an interface holds (`addr`),
/// a /24 around each default route's gateway (`route`), and the /24s a 5 s
/// passive sniff of the LAN side hears (`arp`). A scan given no subnets
/// sweeps only the first kind, on up `lan` interfaces; the others only when
/// a request names them. Answers `GET /api/infra/discover/subnets` and the
/// tunnel's `tunnel.infra.discover.subnets`.
pub async fn auto_detect_subnets(lte: &Lte) -> Result<Vec<SubnetEntry>, String> {
    let net = Network::read(lte)
        .await
        .map_err(|e| format!("cannot read this unit's network: {e}"))?;
    let mut subnets = listed_subnets(&net);

    // Pass #8 Phase C: passive sniff. Listens on each LAN-side interface
    // for a few seconds, parses sender IPs from ARP + common broadcast UDP
    // (DHCP, mDNS, SSDP, NetBIOS, UBNT discovery). Any /24 that doesn't
    // overlap an existing Addr/Route entry joins the list as source=Arp so
    // the UI shows it with an observed badge.
    //
    // Catches the downstream-bridge topology the default-gateway inference
    // can't reach: BPI in br-lan bridging eth5 to a WiFi router's LAN, the
    // WiFi router advertising on 192.168.58.1: passive capture sees that
    // broadcast even though the BPI has no L3 identity on that subnet.
    //
    // Bounded 5s. Silent no-op when tcpdump isn't installed so auto-detect
    // doesn't break on minimal BPIs. It only listens.
    let lan: Vec<String> = lan_ifaces(&net).iter().map(|i| i.name.clone()).collect();
    for (iface, cidr) in passive_sniff_subnets(lan, 5).await {
        let known = subnets
            .iter()
            .any(|s| Cidr::parse(&s.cidr).is_some_and(|c| c.overlaps(cidr)));
        let Some(at) = net.iface(&iface) else {
            continue;
        };
        if !known {
            subnets.push(SubnetEntry {
                iface,
                cidr: cidr.to_string(),
                host_ip: String::new(),
                mac: mac_of(at),
                source: SubnetSource::Arp,
                role: at.role,
            });
        }
    }

    info!("Auto-detected subnets: {subnets:?}");
    Ok(subnets)
}

/// The subnets the unit's network itself shows: each IPv4 network an
/// interface holds, and a /24 around each default route's gateway.
fn listed_subnets(net: &Network) -> Vec<SubnetEntry> {
    let mut out: Vec<SubnetEntry> = Vec::new();
    for iface in net.ifaces.iter().filter(|i| listed(i)) {
        for &(ip, prefix) in &iface.ipv4 {
            if ip.is_loopback() {
                continue;
            }
            let Some(cidr) = Cidr::new(ip, prefix) else {
                continue;
            };
            out.push(SubnetEntry {
                iface: iface.name.clone(),
                cidr: cidr.to_string(),
                host_ip: ip.to_string(),
                mac: mac_of(iface),
                source: SubnetSource::Addr,
                role: iface.role,
            });
        }
    }

    // Pass #8 Phase A: default-gateway subnet inference. Catches the
    // downstream topology where the BPI is plugged into an existing LAN port
    // and has no IP on the upstream subnet (e.g. ETH5 in br-lan cabled to a
    // WiFi router's LAN, WAN side unbound). A default route tells us the
    // gateway exists + which iface reaches it; we assume /24 (the
    // overwhelmingly common home/SMB case) and surface it so the UI can
    // offer it as a scannable subnet. It is swept only when a request names
    // it.
    //
    // Dedup: skip any route candidate whose gateway IP is already covered by
    // an Addr-sourced subnet on the same iface (e.g. the LTE gateway sits
    // inside a /28 we already report; a second /24 entry there is spurious).
    for route in net.routes() {
        if route.dst_len != 0 {
            continue;
        }
        let (Some(gateway), Some(oif)) = (route.gateway, route.oif) else {
            continue;
        };
        let Some(iface) = net.iface_at(oif).filter(|i| listed(i)) else {
            continue;
        };
        let Some(cidr) = Cidr::new(gateway, 24) else {
            continue;
        };
        let known = out.iter().any(|s| {
            s.iface == iface.name
                && (s.cidr == cidr.to_string()
                    || (s.source == SubnetSource::Addr
                        && Cidr::parse(&s.cidr).is_some_and(|c| c.contains(gateway))))
        });
        if !known {
            out.push(SubnetEntry {
                iface: iface.name.clone(),
                cidr: cidr.to_string(),
                host_ip: String::new(),
                mac: mac_of(iface),
                source: SubnetSource::Route,
                role: iface.role,
            });
        }
    }
    out
}

/// Passive sniff on each LAN-side interface for `secs` seconds. Returns
/// `(iface, /24)` for every distinct /24 observed from sender IPs in ARP +
/// common broadcast UDP protocols (DHCP, mDNS, SSDP, NetBIOS, UBNT
/// discovery). Silent no-op when `tcpdump` isn't installed.
///
/// Sniffs run per-iface in parallel (Linux kernels don't support promisc
/// mode on the `any` pseudo-iface). Each iface's capture lasts at most
/// `secs`, so auto-detect takes about `secs` whatever the number of
/// interfaces.
async fn passive_sniff_subnets(ifaces: Vec<String>, secs: u64) -> Vec<(String, Cidr)> {
    let handles: Vec<_> = ifaces
        .into_iter()
        .map(|iface| tokio::spawn(async move { sniff_iface(iface, secs).await }))
        .collect();

    let mut out: Vec<(String, Cidr)> = Vec::new();
    for h in handles {
        if let Ok(observations) = h.await {
            for (iface, cidr) in observations {
                if !out.iter().any(|(i, c)| *i == iface && *c == cidr) {
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
///
/// tcpdump runs as an argument vector ([`sniff_args`]) and its output is
/// read from its stdout as it comes ([`listen`]): no shell, and no file.
async fn sniff_iface(iface: String, secs: u64) -> Vec<(String, Cidr)> {
    if !iface
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Vec::new();
    }
    let args = sniff_args(&iface);
    let Some(lines) = listen(
        "tcpdump",
        &strs(&args),
        Duration::from_secs(secs),
        SNIFF_PACKETS,
    )
    .await
    else {
        return Vec::new();
    };

    let mut cidrs: BTreeSet<Cidr> = BTreeSet::new();
    for line in &lines {
        if let Some(ip) = parse_sender_ip(line) {
            if !is_usable_sender(ip) {
                continue;
            }
            if let Some(cidr) = Cidr::new(ip, 24) {
                cidrs.insert(cidr);
            }
        }
    }
    cidrs.into_iter().map(|c| (iface.clone(), c)).collect()
}

/// tcpdump's arguments for the passive sniff on `iface`: numeric, a line
/// per packet as it comes, [`SNIFF_PACKETS`] packets at most, of ARP and
/// the broadcast UDP protocols [`SNIFF_FILTER`] names.
fn sniff_args(iface: &str) -> Vec<String> {
    let count = SNIFF_PACKETS.to_string();
    ["-i", iface, "-nn", "-l", "-c", count.as_str(), SNIFF_FILTER]
        .map(String::from)
        .to_vec()
}

/// What `program` prints on stdout within `limit`, [`read_lines_until`]'s
/// way: then the child is killed, and what it printed by then is kept.
/// None when it cannot be started, as when the unit lacks it. It runs as
/// an argument vector, with no shell; its stdin is empty and its stderr
/// dropped.
async fn listen(program: &str, args: &[&str], limit: Duration, max: usize) -> Option<Vec<String>> {
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| debug!("Discovery: {program} not run: {e}"))
        .ok()?;
    let deadline = tokio::time::Instant::now() + limit;
    let lines = match child.stdout.take() {
        Some(stdout) => read_lines_until(stdout, deadline, max).await,
        None => Vec::new(),
    };
    // It may have ended already; either way it is reaped here, or by
    // tokio once this returns.
    let _ = child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    Some(lines)
}

/// The lines `reader` gives until `deadline`, `max` lines or
/// [`LISTEN_BYTES`] bytes, or until it ends. A line the deadline cuts is
/// left out; bytes that are not UTF-8 are replaced.
async fn read_lines_until(
    reader: impl AsyncRead + Unpin,
    deadline: tokio::time::Instant,
    max: usize,
) -> Vec<String> {
    let mut reader = BufReader::new(reader.take(LISTEN_BYTES));
    let mut lines = Vec::new();
    let mut line = Vec::new();
    while lines.len() < max {
        line.clear();
        match tokio::time::timeout_at(deadline, reader.read_until(b'\n', &mut line)).await {
            Ok(Ok(1..)) => lines.push(String::from_utf8_lossy(&line).trim_end().to_string()),
            _ => break,
        }
    }
    lines
}

/// Extract the most plausible sender IPv4 from one tcpdump text line.
/// Handles both `IP <sender>[.<port>] > <dest>[.<port>]: ...` and ARP's
/// `Request who-has <tgt> tell <sender>` / `Reply <sender> is-at ...` forms.
fn parse_sender_ip(line: &str) -> Option<Ipv4Addr> {
    // ARP "tell <sender>": the sender is unambiguous
    if let Some(rest) = line.split(" tell ").nth(1) {
        if let Some(ip_token) = rest.split(|c: char| c == ',' || c.is_whitespace()).next() {
            if let Ok(ip) = ip_token.parse() {
                return Some(ip);
            }
        }
    }
    // ARP "Reply <sender> is-at ...": sender is the first field after Reply
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
            // Strip trailing port: "192.168.58.1.5246" is "192.168.58.1"
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
fn is_usable_sender(ip: Ipv4Addr) -> bool {
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

// ─── The probes ──────────────────────────────────────────────────────

/// A host that answered the sweep, with the MAC its answer carried: an ARP
/// answer carries one (arp-scan's, or nmap's report of one), a ping none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Answered {
    pub ip: Ipv4Addr,
    pub mac: Option<Mac>,
}

/// What a scan asks of the unit: its network and the kernel's answers, and
/// the probes that send. The agent's own ([`AgentProbes`]) read the kernel
/// and run the tools; a test's fakes send nothing.
pub(crate) trait Probes: Send + Sync {
    /// The unit's links, addresses and main table, read now.
    fn network(&self) -> BoxFuture<'_, io::Result<Network>>;
    /// The kernel's IPv4 neighbour table, read now.
    fn neighbours(&self) -> BoxFuture<'_, io::Result<Vec<Neighbour>>>;
    /// `nmap -sn` over `subnet`, sending by `dev`: the hosts it found up.
    /// None when nmap did not run to its end.
    fn nmap<'a>(&'a self, dev: &'a str, subnet: Cidr) -> BoxFuture<'a, Option<Vec<Answered>>>;
    /// `arp-scan` over `subnet` on `dev`, from the interface's address or,
    /// `addressless`, from 0.0.0.0: every answer. None when it did not run
    /// to its end.
    fn arp_scan<'a>(
        &'a self,
        dev: &'a str,
        subnet: Cidr,
        addressless: bool,
    ) -> BoxFuture<'a, Option<Vec<Answered>>>;
    /// One ping to `ip`, bound to `dev`: whether it answered.
    fn ping<'a>(&'a self, dev: &'a str, ip: Ipv4Addr) -> BoxFuture<'a, bool>;
    /// The kernel's route to `ip` now.
    fn route(&self, ip: Ipv4Addr) -> BoxFuture<'_, io::Result<guard::Answer>>;
    /// The `ports` that accept a connection from a socket bound where the
    /// guard approved.
    fn open_ports<'a>(
        &'a self,
        approved: &'a Approved,
        ports: &'a [u16],
    ) -> BoxFuture<'a, Vec<u16>>;
    /// One ARP request for `ip` on `iface`, from `src`: the MACs that
    /// answered.
    fn ask_arp<'a>(
        &'a self,
        iface: &'a Iface,
        src: Ipv4Addr,
        ip: Ipv4Addr,
    ) -> BoxFuture<'a, io::Result<Vec<Mac>>>;
    /// The name the unit's resolver gives `ip`.
    fn hostname(&self, ip: Ipv4Addr) -> BoxFuture<'_, Option<String>>;
}

/// The agent's own probes: the kernel over netlink, the tools on its PATH,
/// each run as an argument vector.
struct AgentProbes {
    lte: Lte,
}

impl Probes for AgentProbes {
    fn network(&self) -> BoxFuture<'_, io::Result<Network>> {
        Box::pin(Network::read(&self.lte))
    }

    fn neighbours(&self) -> BoxFuture<'_, io::Result<Vec<Neighbour>>> {
        Box::pin(guard::read_neighbours())
    }

    fn nmap<'a>(&'a self, dev: &'a str, subnet: Cidr) -> BoxFuture<'a, Option<Vec<Answered>>> {
        Box::pin(async move {
            let args = nmap_args(dev, subnet);
            let (code, stdout, _) = checks::exec_args_pub("nmap", &strs(&args), SWEEP_TIMEOUT_MS)
                .await
                .ok()?;
            (code == 0).then(|| parse_nmap(&stdout))
        })
    }

    fn arp_scan<'a>(
        &'a self,
        dev: &'a str,
        subnet: Cidr,
        addressless: bool,
    ) -> BoxFuture<'a, Option<Vec<Answered>>> {
        Box::pin(async move {
            let args = arp_scan_args(dev, subnet, addressless);
            let (code, stdout, _) =
                checks::exec_args_pub("arp-scan", &strs(&args), SWEEP_TIMEOUT_MS)
                    .await
                    .ok()?;
            (code == 0).then(|| parse_arp_scan(&stdout))
        })
    }

    fn ping<'a>(&'a self, dev: &'a str, ip: Ipv4Addr) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let args = ping_args(dev, ip);
            checks::exec_args_pub("ping", &strs(&args), PING_TIMEOUT_MS)
                .await
                .is_ok_and(|(code, _, _)| code == 0)
        })
    }

    fn route(&self, ip: Ipv4Addr) -> BoxFuture<'_, io::Result<guard::Answer>> {
        Box::pin(guard::kernel_answer(ip))
    }

    fn open_ports<'a>(
        &'a self,
        approved: &'a Approved,
        ports: &'a [u16],
    ) -> BoxFuture<'a, Vec<u16>> {
        Box::pin(async move {
            let tries = ports.iter().map(|&port| async move {
                let connect = approved.connect_tcp(port);
                matches!(tokio::time::timeout(PORT_TIMEOUT, connect).await, Ok(Ok(_)))
                    .then_some(port)
            });
            let mut open: Vec<u16> = join_all(tries).await.into_iter().flatten().collect();
            open.sort_unstable();
            open
        })
    }

    fn ask_arp<'a>(
        &'a self,
        iface: &'a Iface,
        src: Ipv4Addr,
        ip: Ipv4Addr,
    ) -> BoxFuture<'a, io::Result<Vec<Mac>>> {
        Box::pin(guard::ask_arp(iface, src, ip, guard::ARP_WAIT_MAX))
    }

    fn hostname(&self, ip: Ipv4Addr) -> BoxFuture<'_, Option<String>> {
        Box::pin(async move { resolve_hostname(&ip.to_string()).await.ok() })
    }
}

/// nmap's arguments: a ping sweep of `subnet`, no name lookups, sent by
/// `dev` alone (`-e`), the interface the sweep chose (ADR-012 decision
/// 10).
fn nmap_args(dev: &str, subnet: Cidr) -> Vec<String> {
    let subnet = subnet.to_string();
    ["-sn", "-n", "-e", dev, subnet.as_str()]
        .map(String::from)
        .to_vec()
}

/// arp-scan's arguments: `subnet` searched on `dev`, the interface the
/// sweep chose, from its address or, `addressless`, from 0.0.0.0 to the
/// broadcast MAC, each silent host asked twice.
fn arp_scan_args(dev: &str, subnet: Cidr, addressless: bool) -> Vec<String> {
    let mut args = vec![
        format!("--interface={dev}"),
        "--retry=2".into(),
        "--timeout=300".into(),
    ];
    if addressless {
        args.extend([
            "--arpspa=0.0.0.0".into(),
            "--destaddr=ff:ff:ff:ff:ff:ff".into(),
        ]);
    }
    args.push(subnet.to_string());
    args
}

/// ping's arguments: one echo to `ip`, waiting a second, from a socket
/// bound to `dev` (`-I`), the interface the sweep chose.
fn ping_args(dev: &str, ip: Ipv4Addr) -> Vec<String> {
    let ip = ip.to_string();
    ["-c", "1", "-W", "1", "-I", dev, ip.as_str()]
        .map(String::from)
        .to_vec()
}

/// Each argument as the exec helper takes it.
fn strs(args: &[String]) -> Vec<&str> {
    args.iter().map(String::as_str).collect()
}

/// nmap's report of a ping sweep: each `Nmap scan report for <ip>`, with
/// the MAC of the `MAC Address:` line nmap prints after it when an ARP
/// answer gave one.
fn parse_nmap(stdout: &str) -> Vec<Answered> {
    let mut out: Vec<Answered> = Vec::new();
    // The host the next `MAC Address:` line belongs to.
    let mut current: Option<usize> = None;
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("Nmap scan report for ") {
            current = None;
            if rest.contains("[host down") {
                continue;
            }
            // With -n the address alone; else `name (address)`.
            let last = rest.rsplit(' ').next().unwrap_or(rest);
            if let Ok(ip) = last.trim_matches(|c| c == '(' || c == ')').parse() {
                out.push(Answered { ip, mac: None });
                current = Some(out.len() - 1);
            }
        } else if let Some(rest) = line.strip_prefix("MAC Address: ") {
            let mac = rest.split_whitespace().next().and_then(Mac::parse);
            if let (Some(at), Some(mac)) = (current, mac) {
                out[at].mac.get_or_insert(mac);
            }
        }
    }
    out
}

/// arp-scan's answers: one line per answer, `<ip>\t<mac>\t<vendor>`, a
/// second answer for an address on a line of its own.
fn parse_arp_scan(stdout: &str) -> Vec<Answered> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let ip = fields.next()?.trim().parse().ok()?;
            let mac = Mac::parse(fields.next()?.trim())?;
            Some(Answered { ip, mac: Some(mac) })
        })
        .collect()
}

// ─── The scan ────────────────────────────────────────────────────────

/// One host as the scan learns it.
#[derive(Default)]
struct Host {
    /// The MACs the sweep saw answering for it, each once.
    answered: Vec<Mac>,
    /// Where the guard let the port probe go.
    approved: Option<Approved>,
    open_ports: Vec<u16>,
    /// The host's MAC once `decided`: none when no MAC or two were seen.
    mac: Option<Mac>,
    /// Set once the port probe is done; until then the progress shows the
    /// sweep's one MAC.
    decided: bool,
    own: Option<String>,
    hostname: Option<String>,
}

impl Host {
    fn heard(&mut self, mac: Option<Mac>) {
        if let Some(mac) = mac {
            if !self.answered.contains(&mac) {
                self.answered.push(mac);
            }
        }
    }
}

/// The devices as the answer and the progress show them. Before the MACs
/// are decided, a host the sweep heard one MAC for shows it; once they
/// are, each shows its own, none included.
fn devices(hosts: &BTreeMap<Ipv4Addr, Host>) -> Vec<DiscoveredDevice> {
    hosts
        .iter()
        .map(|(ip, host)| {
            let mac = if host.decided {
                host.mac
            } else {
                match host.answered.as_slice() {
                    [one] => Some(*one),
                    _ => None,
                }
            };
            let mac = mac.map(|m| m.to_string());
            DiscoveredDevice {
                ip: ip.to_string(),
                inferred_type: infer_type(&host.open_ports, mac.as_deref()),
                mac,
                hostname: host.hostname.clone(),
                open_ports: host.open_ports.clone(),
                mdns_services: Vec::new(),
                own: host.own.clone(),
            }
        })
        .collect()
}

/// What a scan found, and the subnets it did not sweep.
pub(crate) struct Scanned {
    pub devices: Vec<DiscoveredDevice>,
    pub not_swept: Vec<NotSwept>,
}

/// One scan (ADR-012 decisions 10 to 13). The unit's network is read once,
/// first: every subnet is judged against it, and a scan that cannot read
/// it sends nothing.
pub(crate) async fn scan(
    probes: &dyn Probes,
    requested: &[String],
    progress: &Progress,
) -> Result<Scanned, String> {
    let net = probes
        .network()
        .await
        .map_err(|e| format!("cannot read this unit's network: {e}"))?;
    // Entries that are not IPv4 networks are named first, as given.
    let (subnets, mut not_swept) = if requested.is_empty() {
        (default_subnets(&net), Vec::new())
    } else {
        named_subnets(requested)
    };
    let names: Vec<String> = subnets.iter().map(ToString::to_string).collect();
    info!("Discovery: subnets {names:?}");
    let own = own_addresses(&net);
    let mut hosts: BTreeMap<Ipv4Addr, Host> = BTreeMap::new();

    // Phase 1: the LAN side's neighbours, as hosts only.
    progress.phase("arp", 1, &hosts);
    match probes.neighbours().await {
        Ok(entries) => {
            for ip in lan_neighbours(&net, &entries) {
                hosts.entry(ip).or_default();
            }
        }
        Err(e) => debug!("Discovery: cannot read the neighbour table: {e}"),
    }
    progress.phase("arp", 1, &hosts);

    // Phase 2: the sweep.
    progress.phase("ping", 2, &hosts);
    for &subnet in &subnets {
        let swept = sweep(probes, &net, subnet, &own).await;
        for answer in swept.answered {
            hosts.entry(answer.ip).or_default().heard(answer.mac);
        }
        if swept.on_lan {
            // The unit's own addresses there are found without a probe.
            for &ip in own.iter().filter(|ip| subnet.contains(**ip)) {
                hosts.entry(ip).or_default();
            }
        }
        if let Some(reason) = swept.not {
            not_swept.push(NotSwept {
                cidr: subnet.to_string(),
                reason,
            });
        }
        progress.phase("ping", 2, &hosts);
    }

    // Phase 3: the port probe, each host judged by the guard.
    progress.phase("ports", 3, &hosts);
    let ips: Vec<Ipv4Addr> = hosts.keys().copied().collect();
    let net_ref = &net;
    let verdicts: Vec<(Ipv4Addr, Option<Approved>, Vec<u16>)> = stream::iter(ips)
        .map(|ip| async move {
            let approved = approve(probes, net_ref, ip, "probed").await;
            let open = match &approved {
                Some(approved) => probes.open_ports(approved, &PORTS).await,
                None => Vec::new(),
            };
            (ip, approved, open)
        })
        .buffer_unordered(AT_ONCE)
        .collect()
        .await;
    for (ip, approved, open) in verdicts {
        if let Some(host) = hosts.get_mut(&ip) {
            host.approved = approved;
            host.open_ports = open;
        }
    }

    // Each host's MAC, now the sweep and the port probe are done, and the
    // unit's own.
    resolve_macs(probes, &net, &mut hosts).await;
    for (ip, host) in &mut hosts {
        host.own = own_iface(&net, *ip, host.mac);
    }
    progress.phase("ports", 3, &hosts);

    // Phase 4: reverse lookups, wherever the unit's resolver sends them.
    progress.phase("hostname", 4, &hosts);
    for (ip, host) in &mut hosts {
        host.hostname = probes.hostname(*ip).await;
    }

    Ok(Scanned {
        devices: devices(&hosts),
        not_swept,
    })
}

/// Phase 1: the hosts the neighbour table names on the LAN side, each once:
/// entries on an up `lan` interface that hold a MAC the kernel would send
/// to (neither resolving, failed, nor needing no ARP), at a unicast address.
/// Their MACs are not kept: the table is read again once the sweep and the
/// port probe are done (ADR-012 decision 11).
fn lan_neighbours(net: &Network, entries: &[Neighbour]) -> BTreeSet<Ipv4Addr> {
    entries
        .iter()
        .filter(|e| net.iface_at(e.index).is_some_and(Iface::lan_side))
        .filter(|e| e.usable_lladdr().is_some() && e.state & netlink::NUD_NOARP == 0)
        .filter(|e| {
            !(e.dst.is_multicast()
                || e.dst.is_broadcast()
                || e.dst.is_unspecified()
                || e.dst.is_loopback())
        })
        .map(|e| e.dst)
        .collect()
}

/// What sweeping one subnet found.
struct Sweep {
    answered: Vec<Answered>,
    /// It lies on the LAN side, where the unit's own addresses count as
    /// found.
    on_lan: bool,
    /// Why it was not swept, when it was not.
    not: Option<NotSweptReason>,
}

impl Sweep {
    fn swept(answered: Vec<Answered>) -> Self {
        Self {
            answered,
            on_lan: true,
            not: None,
        }
    }
}

/// Sweep `subnet` (ADR-012 decisions 10 and 13). On the LAN side, on the
/// interface its route leaves by: nmap, else arp-scan, else the ping
/// fallback, each tried when the one before found nothing; the fallback
/// pings only what the guard passes there. Off it: arp-scan alone, from
/// 0.0.0.0, on each LAN-side interface until one finds hosts. arp-scan
/// runs only on a subnet it can search within its limit
/// ([`ARP_SCAN_MAX`]).
async fn sweep(probes: &dyn Probes, net: &Network, subnet: Cidr, own: &[Ipv4Addr]) -> Sweep {
    let arp_scan_fits = subnet.host_count() <= ARP_SCAN_MAX;
    if !arp_scan_fits {
        debug!("Discovery: {subnet} holds more hosts than arp-scan searches within its limit");
    }
    let Some(iface) = sweep_iface(net, subnet) else {
        let mut answered = Vec::new();
        let searched = if arp_scan_fits {
            lan_ifaces(net)
        } else {
            Vec::new()
        };
        for lan in searched {
            if let Some(found) = probes.arp_scan(&lan.name, subnet, true).await {
                if !found.is_empty() {
                    debug!(
                        "Discovery: {subnet} by ARP alone on {}: {} answers",
                        lan.name,
                        found.len()
                    );
                    answered = found;
                    break;
                }
            }
        }
        return Sweep {
            answered,
            on_lan: false,
            not: Some(NotSweptReason::NotOnLan),
        };
    };
    let dev = iface.name.as_str();
    let mut ran = false;
    if let Some(found) = probes.nmap(dev, subnet).await {
        ran = true;
        if !found.is_empty() {
            debug!(
                "Discovery: {subnet} by nmap on {dev}: {} hosts",
                found.len()
            );
            return Sweep::swept(found);
        }
    }
    let arp_scan = if arp_scan_fits {
        probes.arp_scan(dev, subnet, false).await
    } else {
        None
    };
    if let Some(found) = arp_scan {
        ran = true;
        if !found.is_empty() {
            debug!(
                "Discovery: {subnet} by arp-scan on {dev}: {} answers",
                found.len()
            );
            return Sweep::swept(found);
        }
    }
    let Some(targets) = fallback_targets(subnet, own) else {
        return Sweep {
            answered: Vec::new(),
            on_lan: true,
            not: (!ran).then_some(NotSweptReason::TooLarge),
        };
    };
    let answered = ping_all(probes, net, dev, &targets).await;
    debug!(
        "Discovery: {subnet} by ping on {dev}: {} hosts",
        answered.len()
    );
    Sweep::swept(answered)
}

/// The addresses the ping fallback pings in `subnet` (ADR-012 decision 13):
/// its hosts, without the network and broadcast addresses (a /31 and a /32
/// have none) and the unit's own. None when the subnet holds more than
/// [`FALLBACK_MAX`] hosts.
fn fallback_targets(subnet: Cidr, own: &[Ipv4Addr]) -> Option<Vec<Ipv4Addr>> {
    if subnet.host_count() > FALLBACK_MAX {
        return None;
    }
    Some(subnet.hosts().filter(|ip| !own.contains(ip)).collect())
}

/// The ping fallback: one ping from `dev` to each target the guard passes
/// there, at most [`AT_ONCE`] in flight. Whatever the request names, every
/// send follows decision 1 (ADR-012 decision 10): a target is pinged only
/// when the kernel's route to it is unicast, has no gateway and leaves by
/// `dev`. A broadcast or multicast address a named subnet holds, such as a
/// network's directed broadcast named as a /32, is never pinged.
async fn ping_all(
    probes: &dyn Probes,
    net: &Network,
    dev: &str,
    targets: &[Ipv4Addr],
) -> Vec<Answered> {
    stream::iter(targets.iter().copied())
        .map(|ip| async move {
            let approved = approve(probes, net, ip, "pinged").await?;
            if approved.own {
                debug!("Discovery: {ip} not pinged: it is this unit's own");
                return None;
            }
            if approved.dev != dev {
                debug!(
                    "Discovery: {ip} not pinged: the kernel would send it by {}, not {dev}",
                    approved.dev
                );
                return None;
            }
            probes
                .ping(dev, ip)
                .await
                .then_some(Answered { ip, mac: None })
        })
        .buffer_unordered(AT_ONCE)
        .filter_map(|answered| async move { answered })
        .collect()
        .await
}

/// The guard's verdict on `ip` (ADR-012 decision 1), from the kernel's route
/// now: where a send to it may go, or None when nothing may. `what` names
/// the send in the log: "probed", "pinged".
async fn approve(probes: &dyn Probes, net: &Network, ip: Ipv4Addr, what: &str) -> Option<Approved> {
    let answer = match probes.route(ip).await {
        Ok(answer) => answer,
        Err(e) => {
            debug!("Discovery: {ip} not {what}: cannot ask the kernel for the route: {e}");
            return None;
        }
    };
    match guard::judge(net, ip, &answer, None, false) {
        Ok(approved) => Some(approved),
        Err(refusal) => {
            debug!(
                "Discovery: {ip} not {what} ({}): {}",
                refusal.refused, refusal.reason
            );
            None
        }
    }
}

/// What decides a host's MAC once the sweep and the port probe are done
/// (ADR-012 decision 11).
#[derive(Debug, PartialEq, Eq)]
enum MacStep {
    /// Decided: the one MAC seen answering, none when two were.
    Known(Option<Mac>),
    /// None seen: ask the wire, one ARP request.
    Ask,
}

/// The MACs seen answering at a host's address: each the sweep heard, and
/// the one the kernel's entry on the interface that reaches it names when
/// that entry is REACHABLE or permanent. One is the host's MAC; two give
/// none, the sweep's against the entry's too. A STALE, DELAY or PROBE
/// entry is not one seen: it may name the device that held the address
/// before, so it gives way to the sweep's answer, or leads to the ARP
/// request.
fn mac_step(answered: &[Mac], entry: Option<&Neighbour>) -> MacStep {
    let entry = entry
        .filter(|e| e.state & (netlink::NUD_REACHABLE | netlink::NUD_PERMANENT) != 0)
        .and_then(|e| e.lladdr)
        .map(Mac);
    let mut seen: Vec<Mac> = answered.to_vec();
    if let Some(mac) = entry.filter(|m| !seen.contains(m)) {
        seen.push(mac);
    }
    match seen.as_slice() {
        [] => MacStep::Ask,
        [one] => MacStep::Known(Some(*one)),
        [_, _, ..] => MacStep::Known(None),
    }
}

/// One MAC, or none when there were none or two.
fn one_mac(macs: &[Mac]) -> Option<Mac> {
    let first = *macs.first()?;
    macs.iter().all(|m| *m == first).then_some(first)
}

/// Each host's MAC (ADR-012 decision 11): the one seen answering, by the
/// sweep or in the kernel's entry on the interface that reaches it,
/// REACHABLE or permanent now ([`mac_step`]); with none, one ARP request
/// there. A host the guard keeps from the wire, or the unit's own, is asked
/// nothing.
async fn resolve_macs(probes: &dyn Probes, net: &Network, hosts: &mut BTreeMap<Ipv4Addr, Host>) {
    let entries = probes.neighbours().await.unwrap_or_else(|e| {
        debug!("Discovery: cannot read the neighbour table: {e}");
        Vec::new()
    });
    let mut ask: Vec<(Ipv4Addr, String, Ipv4Addr)> = Vec::new();
    for (&ip, host) in hosts.iter_mut() {
        host.decided = true;
        let reaches = host
            .approved
            .as_ref()
            .filter(|a| !a.own)
            .and_then(|a| net.iface(&a.dev).map(|iface| (iface, a.src)));
        let entry = reaches.and_then(|(iface, _)| {
            entries
                .iter()
                .find(|e| e.index == iface.index && e.dst == ip)
        });
        match mac_step(&host.answered, entry) {
            MacStep::Known(mac) => host.mac = mac,
            MacStep::Ask => {
                if let Some((iface, src)) = reaches {
                    ask.push((ip, iface.name.clone(), src));
                }
            }
        }
    }
    let asked: Vec<(Ipv4Addr, Option<Mac>)> = stream::iter(ask)
        .map(|(ip, dev, src)| async move {
            let Some(iface) = net.iface(&dev) else {
                return (ip, None);
            };
            match probes.ask_arp(iface, src, ip).await {
                Ok(macs) => (ip, one_mac(&macs)),
                Err(e) => {
                    debug!("Discovery: no ARP request for {ip} on {dev}: {e}");
                    (ip, None)
                }
            }
        })
        .buffer_unordered(AT_ONCE)
        .collect()
        .await;
    for (ip, mac) in asked {
        if let Some(host) = hosts.get_mut(&ip) {
            host.mac = mac;
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;

    use crate::netwatch::netlink::tests as nl;
    use crate::netwatch::netlink::{
        Addr, Link, Messages, Order, IFF_LOOPBACK, IFF_NOARP, IFF_POINTOPOINT, IFF_UP, NUD_DELAY,
        NUD_FAILED, NUD_INCOMPLETE, NUD_NOARP, NUD_PERMANENT, NUD_REACHABLE, NUD_STALE,
    };
    use crate::netwatch::source::{KernelRoute, RouteKind};

    const ETHER_UP: u32 = IFF_UP | 0x1000 | 0x40; // UP, MULTICAST, RUNNING
    const MAC_A: Mac = Mac([0x02, 0, 0, 0, 0, 0x0a]);
    const MAC_B: Mac = Mac([0x02, 0, 0, 0, 0, 0x0b]);
    const MAC_C: Mac = Mac([0x02, 0, 0, 0, 0, 0x0c]);
    const MAC_D: Mac = Mac([0x02, 0, 0, 0, 0, 0x0d]);
    /// The unit's br-lan, as `link` gives it.
    const BR_LAN_MAC: Mac = Mac([0x02, 0, 0, 0, 1, 4]);

    fn ip(text: &str) -> Ipv4Addr {
        text.parse().unwrap()
    }

    fn cidr(text: &str) -> Cidr {
        Cidr::parse(text).unwrap()
    }

    fn link(index: u32, name: &str, flags: u32) -> Link {
        Link {
            index,
            kind: netlink::ARPHRD_ETHER,
            flags,
            name: name.into(),
            operstate: 6,
            carrier: Some(true),
            master: None,
            address: Some([0x02, 0, 0, 0, 1, index as u8]),
        }
    }

    /// A WireGuard-like link: no ARP, point-to-point, no MAC.
    fn tunnel(index: u32, name: &str) -> Link {
        Link {
            kind: 65534, // ARPHRD_NONE
            address: None,
            ..link(index, name, IFF_UP | IFF_POINTOPOINT | IFF_NOARP)
        }
    }

    fn addr(index: u32, local: [u8; 4], prefix: u8) -> Addr {
        Addr {
            index,
            prefix,
            secondary: false,
            local: Ipv4Addr::from(local),
        }
    }

    fn route(dst: Option<([u8; 4], u8)>, via: Option<[u8; 4]>, oif: u32, metric: u32) -> Route {
        Route {
            dst_len: dst.map_or(0, |(_, len)| len),
            table: 254,
            protocol: 4,
            scope: if via.is_some() { 0 } else { 253 },
            kind: netlink::RTN_UNICAST,
            flags: 0,
            dst: dst.map(|(d, _)| Ipv4Addr::from(d)),
            gateway: via.map(Ipv4Addr::from),
            oif: Some(oif),
            priority: metric,
            prefsrc: None,
        }
    }

    /// A probe unit on a site: br-lan (port eth0) holding 192.0.2.1/24; the
    /// wired WAN port eth1 holding 192.168.8.20/24; the LTE bearer wwan0,
    /// framed like Ethernet, with the default route; the mesh wg0; a
    /// container bridge; the loopback.
    fn unit_links() -> Vec<Link> {
        let mut eth0 = link(2, "eth0", ETHER_UP);
        eth0.master = Some(4);
        vec![
            link(1, "lo", IFF_UP | IFF_LOOPBACK | 0x40),
            eth0,
            link(3, "eth1", ETHER_UP),
            link(4, "br-lan", ETHER_UP),
            link(5, "wwan0", ETHER_UP),
            tunnel(6, "wg0"),
            link(7, "docker0", ETHER_UP),
        ]
    }

    fn unit_addrs() -> Vec<Addr> {
        vec![
            addr(1, [127, 0, 0, 1], 8),
            addr(4, [192, 0, 2, 1], 24),
            addr(3, [192, 168, 8, 20], 24),
            addr(5, [10, 180, 41, 231], 30),
            addr(6, [10, 99, 0, 2], 24),
            addr(7, [172, 17, 0, 1], 16),
        ]
    }

    fn unit_routes() -> Vec<Route> {
        vec![
            route(None, Some([10, 180, 41, 232]), 5, 40),
            route(Some(([192, 0, 2, 0], 24)), None, 4, 0),
            route(Some(([192, 168, 8, 0], 24)), None, 3, 0),
            route(Some(([10, 180, 41, 228], 30)), None, 5, 0),
            route(Some(([10, 99, 0, 0], 24)), None, 6, 0),
            route(Some(([172, 17, 0, 0], 16)), None, 7, 0),
        ]
    }

    fn network(links: &[Link], addrs: &[Addr], routes: &[Route]) -> Network {
        Network::from_dump(links, addrs, routes, &Lte::named("wwan0"))
    }

    fn unit() -> Network {
        network(&unit_links(), &unit_addrs(), &unit_routes())
    }

    fn neighbour(index: u32, state: u16, dst: &str, mac: Mac) -> Neighbour {
        Neighbour {
            index,
            state,
            dst: ip(dst),
            lladdr: Some(mac.0),
        }
    }

    fn no_progress() -> Progress {
        Progress {
            infra: None,
            start: Instant::now(),
            started_at: String::new(),
        }
    }

    fn requested(subnets: &[&str]) -> Vec<String> {
        subnets.iter().map(ToString::to_string).collect()
    }

    fn device<'s>(scanned: &'s Scanned, at: &str) -> &'s DiscoveredDevice {
        scanned
            .devices
            .iter()
            .find(|d| d.ip == at)
            .unwrap_or_else(|| panic!("{at} was not found"))
    }

    /// The kernel's answer for `ip` on a test network: local for an address
    /// the unit holds; broadcast for the broadcast address of a network an
    /// interface holds, from the local table as the kernel fills it; else
    /// its best main-table route, as a broadcast route for 255.255.255.255
    /// and a multicast one for a multicast address.
    fn kernel(net: &Network, ip: Ipv4Addr) -> guard::Answer {
        if net.holder_of(ip).is_some() {
            return guard::Answer::Route(KernelRoute {
                dev: "lo".into(),
                oif: 1,
                via: None,
                src: Some(ip),
                kind: RouteKind::Local,
            });
        }
        let directed = net.ifaces.iter().find(|i| {
            i.ipv4
                .iter()
                .any(|&(a, p)| p < 31 && Ipv4Addr::from(u32::from(a) | !mask(p)) == ip)
        });
        if let Some(iface) = directed {
            return guard::Answer::Route(KernelRoute {
                dev: iface.name.clone(),
                oif: iface.index,
                via: None,
                src: iface.ipv4.first().map(|(a, _)| *a),
                kind: RouteKind::Broadcast,
            });
        }
        let best = net
            .routes()
            .iter()
            .filter(|r| route_net(r).is_some_and(|n| n.contains(ip)))
            .min_by_key(|r| (Reverse(r.dst_len), r.priority));
        let kind = if ip.is_broadcast() {
            RouteKind::Broadcast
        } else if ip.is_multicast() {
            RouteKind::Multicast
        } else {
            RouteKind::Unicast
        };
        match best.and_then(|r| r.oif.and_then(|oif| net.iface_at(oif)).map(|i| (r, i))) {
            Some((r, iface)) if r.kind == netlink::RTN_UNICAST => {
                guard::Answer::Route(KernelRoute {
                    dev: iface.name.clone(),
                    oif: iface.index,
                    via: r.gateway,
                    src: iface.ipv4.first().map(|(a, _)| *a),
                    kind,
                })
            }
            _ => guard::Answer::NoRoute,
        }
    }

    /// Probes that send nothing: the answers a test gives them, and a log
    /// of every probe a scan asked for that would have sent.
    #[derive(Default)]
    struct Fake {
        net: Network,
        /// The network cannot be read.
        unreadable: bool,
        /// The neighbour table at Phase 1, then once the port probe is done.
        before: Vec<Neighbour>,
        after: Vec<Neighbour>,
        reads: AtomicUsize,
        /// nmap's answers by subnet; None: the unit has no nmap.
        nmap: Option<HashMap<Cidr, Vec<Answered>>>,
        /// arp-scan's answers by interface and subnet; None: no arp-scan.
        arp_scan: Option<HashMap<(String, Cidr), Vec<Answered>>>,
        /// The addresses that answer a ping.
        up: Vec<Ipv4Addr>,
        /// The ports open on each address.
        open: HashMap<Ipv4Addr, Vec<u16>>,
        /// The MACs that answer an ARP request for each address.
        arp: HashMap<Ipv4Addr, Vec<Mac>>,
        /// Every probe, as `<probe> <dev> <target>`.
        log: StdMutex<Vec<String>>,
        in_flight: AtomicUsize,
        most_in_flight: AtomicUsize,
    }

    impl Fake {
        fn on(net: Network) -> Self {
            Self {
                net,
                ..Self::default()
            }
        }

        fn note(&self, line: String) {
            self.log.lock().unwrap().push(line);
        }

        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }

        /// The probes of one kind, sorted: those that run at once finish in
        /// any order.
        fn log_of(&self, probe: &str) -> Vec<String> {
            let mut lines: Vec<String> = self
                .log()
                .into_iter()
                .filter(|l| l.split(' ').next() == Some(probe))
                .collect();
            lines.sort();
            lines
        }
    }

    impl Probes for Fake {
        fn network(&self) -> BoxFuture<'_, io::Result<Network>> {
            Box::pin(async move {
                if self.unreadable {
                    Err(io::Error::from_raw_os_error(libc::EPERM))
                } else {
                    Ok(self.net.clone())
                }
            })
        }

        fn neighbours(&self) -> BoxFuture<'_, io::Result<Vec<Neighbour>>> {
            Box::pin(async move {
                let first = self.reads.fetch_add(1, Ordering::SeqCst) == 0;
                Ok(if first {
                    self.before.clone()
                } else {
                    self.after.clone()
                })
            })
        }

        fn nmap<'a>(&'a self, dev: &'a str, subnet: Cidr) -> BoxFuture<'a, Option<Vec<Answered>>> {
            Box::pin(async move {
                let answers = self.nmap.as_ref()?;
                self.note(format!("nmap {dev} {subnet}"));
                Some(answers.get(&subnet).cloned().unwrap_or_default())
            })
        }

        fn arp_scan<'a>(
            &'a self,
            dev: &'a str,
            subnet: Cidr,
            addressless: bool,
        ) -> BoxFuture<'a, Option<Vec<Answered>>> {
            Box::pin(async move {
                let answers = self.arp_scan.as_ref()?;
                let from = if addressless { " from 0.0.0.0" } else { "" };
                self.note(format!("arp-scan {dev} {subnet}{from}"));
                Some(
                    answers
                        .get(&(dev.to_string(), subnet))
                        .cloned()
                        .unwrap_or_default(),
                )
            })
        }

        fn ping<'a>(&'a self, dev: &'a str, ip: Ipv4Addr) -> BoxFuture<'a, bool> {
            Box::pin(async move {
                self.note(format!("ping {dev} {ip}"));
                let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                self.most_in_flight.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(2)).await;
                self.in_flight.fetch_sub(1, Ordering::SeqCst);
                self.up.contains(&ip)
            })
        }

        fn route(&self, ip: Ipv4Addr) -> BoxFuture<'_, io::Result<guard::Answer>> {
            Box::pin(async move { Ok(kernel(&self.net, ip)) })
        }

        fn open_ports<'a>(
            &'a self,
            approved: &'a Approved,
            ports: &'a [u16],
        ) -> BoxFuture<'a, Vec<u16>> {
            Box::pin(async move {
                self.note(format!(
                    "ports {} {} from {}",
                    approved.dev, approved.address, approved.src
                ));
                let open = self.open.get(&approved.address).cloned();
                open.unwrap_or_default()
                    .into_iter()
                    .filter(|p| ports.contains(p))
                    .collect()
            })
        }

        fn ask_arp<'a>(
            &'a self,
            iface: &'a Iface,
            src: Ipv4Addr,
            ip: Ipv4Addr,
        ) -> BoxFuture<'a, io::Result<Vec<Mac>>> {
            Box::pin(async move {
                self.note(format!("arp {} {ip} from {src}", iface.name));
                Ok(self.arp.get(&ip).cloned().unwrap_or_default())
            })
        }

        fn hostname(&self, _ip: Ipv4Addr) -> BoxFuture<'_, Option<String>> {
            Box::pin(async { None })
        }
    }

    #[test]
    fn cidrs_parse_in_network_form() {
        let lan = cidr("192.0.2.77/24");
        assert_eq!(lan.to_string(), "192.0.2.0/24");
        assert_eq!(cidr(" 198.51.100.9/32 ").to_string(), "198.51.100.9/32");
        assert_eq!(cidr("10.1.2.3/0").to_string(), "0.0.0.0/0");
        for bad in [
            "",
            "192.0.2.0",
            "192.0.2.0/33",
            "192.0.2/24",
            "192.0.2.0/-1",
            "2001:db8::/64",
            "192.0.2.0/24/8",
        ] {
            assert_eq!(Cidr::parse(bad), None, "{bad:?}");
        }
        assert!(lan.contains(ip("192.0.2.255")));
        let private = cidr("10.0.0.0/24");
        assert!(private.contains(ip("10.0.0.255")) && !private.contains(ip("10.0.1.0")));
        let upper = cidr("192.0.2.128/25");
        assert!(lan.covers(upper) && !upper.covers(lan));
        assert!(lan.overlaps(upper) && upper.overlaps(lan));
        assert!(!upper.overlaps(cidr("192.0.2.0/25")));
        let (subnets, unread) = named_subnets(&requested(&[
            "192.0.2.7/24",
            "192.0.2.0/24",
            "x/24",
            "198.51.100.0/24",
        ]));
        assert_eq!(
            subnets,
            [lan, cidr("198.51.100.0/24")],
            "each once, in network form"
        );
        assert_eq!(
            unread,
            [NotSwept {
                cidr: "x/24".into(),
                reason: NotSweptReason::Invalid,
            }],
            "a word named as not swept, as given"
        );
    }

    /// ADR-012 decision 10: `not_swept` names each subnet the scan did not
    /// sweep, and why. An entry that is not an IPv4 network is named as
    /// given, once: `not_ipv4` for an IPv6 address or network, `invalid`
    /// for anything else that is not `a.b.c.d/p`. It is never swept nor
    /// searched.
    #[tokio::test]
    async fn an_entry_that_is_not_an_ipv4_network_is_named_in_not_swept() {
        let fake = Fake::on(unit());
        let scanned = scan(
            &fake,
            &requested(&[
                "2001:db8::/64",
                "192.0.2.0/33",
                "2001:db8::1",
                "192.0.2.5",
                "2001:db8::/129",
                "bogus",
                "2001:db8::/64",
                "",
            ]),
            &no_progress(),
        )
        .await
        .unwrap();
        let named: Vec<(&str, NotSweptReason)> = scanned
            .not_swept
            .iter()
            .map(|n| (n.cidr.as_str(), n.reason))
            .collect();
        assert_eq!(
            named,
            [
                ("2001:db8::/64", NotSweptReason::NotIpv4),
                ("192.0.2.0/33", NotSweptReason::Invalid),
                ("2001:db8::1", NotSweptReason::NotIpv4),
                ("192.0.2.5", NotSweptReason::Invalid),
                ("2001:db8::/129", NotSweptReason::Invalid),
                ("bogus", NotSweptReason::Invalid),
                ("", NotSweptReason::Invalid),
            ]
        );
        assert!(
            fake.log()
                .iter()
                .all(|l| !["nmap", "arp-scan", "ping", "ports", "arp "]
                    .iter()
                    .any(|tool| l.starts_with(tool))),
            "nothing swept or searched: {:?}",
            fake.log()
        );
        let json = serde_json::to_value(&scanned.not_swept[..2]).unwrap();
        assert_eq!(
            json,
            json!([
                {"cidr": "2001:db8::/64", "reason": "not_ipv4"},
                {"cidr": "192.0.2.0/33", "reason": "invalid"}
            ])
        );

        // Beside an IPv4 network the request names, which is swept.
        let fake = Fake::on(unit());
        let scanned = scan(
            &fake,
            &requested(&["2001:db8::/64", "192.0.2.0/24"]),
            &no_progress(),
        )
        .await
        .unwrap();
        assert_eq!(
            scanned.not_swept,
            [NotSwept {
                cidr: "2001:db8::/64".into(),
                reason: NotSweptReason::NotIpv4,
            }]
        );
        assert!(fake
            .log_of("ping")
            .iter()
            .all(|p| p.starts_with("ping br-lan 192.0.2.")));
        assert!(!fake.log_of("ping").is_empty(), "the IPv4 network is swept");
    }

    /// Each entry of a request's `subnets` is read on its own: one that is
    /// not a string is named by its JSON text, as `invalid`, and the rest
    /// of the list still counts. It used to make the whole list unreadable,
    /// and the scan then swept the default networks instead.
    #[tokio::test]
    async fn an_entry_that_is_not_a_string_is_named_and_the_list_still_counts() {
        assert!(requested_entries(&json!({})).is_empty(), "the default pick");
        assert!(requested_entries(&json!({"subnets": null})).is_empty());
        assert!(requested_entries(&json!({"subnets": []})).is_empty());
        assert_eq!(
            requested_entries(&json!({"subnets": ["192.0.2.0/24", 5, {"a": 1}, ["x"], true]})),
            ["192.0.2.0/24", "5", r#"{"a":1}"#, r#"["x"]"#, "true"]
        );
        assert_eq!(
            requested_entries(&json!({"subnets": "192.0.2.0/24"})),
            ["192.0.2.0/24"],
            "one entry not in a list"
        );
        assert_eq!(requested_entries(&json!({"subnets": 24})), ["24"]);

        let fake = Fake::on(unit());
        let requested = requested_entries(&json!({"subnets": [5, "192.0.2.0/24", {"a": 1}]}));
        let scanned = scan(&fake, &requested, &no_progress()).await.unwrap();
        assert_eq!(
            serde_json::to_value(&scanned.not_swept).unwrap(),
            json!([
                {"cidr": "5", "reason": "invalid"},
                {"cidr": r#"{"a":1}"#, "reason": "invalid"}
            ])
        );
        assert!(
            !fake.log_of("ping").is_empty()
                && fake
                    .log_of("ping")
                    .iter()
                    .all(|p| p.starts_with("ping br-lan 192.0.2.")),
            "the network named is swept, and only it: {:?}",
            fake.log()
        );
    }

    /// The handler itself reads each entry on its own, the route's and the
    /// tunnel's alike: both call [`discover`], which only adds the agent's
    /// own probes to [`discover_with`]. A list that held one entry of
    /// another type used to make the whole list unreadable there, and the
    /// scan swept the default networks.
    #[tokio::test]
    async fn the_handler_reads_each_entry_of_the_requests_subnets_on_its_own() {
        let fake = Fake::on(unit());
        let payload = json!({"subnets": [5, "192.0.2.0/24", {"a": 1}]});
        let Ok(Json(results)) = discover_with(&fake, None, &payload).await else {
            panic!("the scan ran");
        };
        assert_eq!(
            serde_json::to_value(&results.not_swept).unwrap(),
            json!([
                {"cidr": "5", "reason": "invalid"},
                {"cidr": r#"{"a":1}"#, "reason": "invalid"}
            ])
        );
        let pings = fake.log_of("ping");
        assert!(
            !pings.is_empty() && pings.iter().all(|p| p.starts_with("ping br-lan 192.0.2.")),
            "the network named is swept, and only it: {:?}",
            fake.log()
        );
        assert!(
            !fake.log().iter().any(|line| line.contains("192.168.8.")),
            "no default network is swept: {:?}",
            fake.log()
        );

        // With no list, the default pick: both of the unit's LAN networks.
        let fake = Fake::on(unit());
        assert!(discover_with(&fake, None, &json!({})).await.is_ok());
        assert!(
            fake.log().iter().any(|line| line.contains("192.168.8.")),
            "{:?}",
            fake.log()
        );
    }

    /// TRD-9 3.5: the hosts a prefix yields, and a /23 too large for the
    /// fallback (ADR-012 decision 13).
    #[test]
    fn the_fallback_pings_the_hosts_of_the_prefix_and_never_more_than_254() {
        let hosts = |subnet: &str, own: &[Ipv4Addr]| fallback_targets(cidr(subnet), own);
        let slash_24 = hosts("192.0.2.0/24", &[]).unwrap();
        assert_eq!(slash_24.len(), 254);
        assert_eq!(
            (slash_24[0], slash_24[253]),
            (ip("192.0.2.1"), ip("192.0.2.254")),
            "neither the network nor the broadcast address"
        );
        assert_eq!(
            hosts("192.0.2.4/30", &[]).unwrap(),
            [ip("192.0.2.5"), ip("192.0.2.6")]
        );
        assert_eq!(
            hosts("192.0.2.4/31", &[]).unwrap(),
            [ip("192.0.2.4"), ip("192.0.2.5")],
            "a /31 has no network or broadcast address"
        );
        assert_eq!(hosts("192.0.2.9/32", &[]).unwrap(), [ip("192.0.2.9")]);
        assert_eq!(
            hosts("192.0.2.9/32", &[ip("192.0.2.9")]).unwrap(),
            Vec::<Ipv4Addr>::new(),
            "the unit's own address is never pinged"
        );
        let without_own = hosts("192.0.2.0/24", &[ip("192.0.2.1")]).unwrap();
        assert_eq!(without_own.len(), 253);
        assert!(!without_own.contains(&ip("192.0.2.1")));
        assert_eq!(hosts("10.0.0.0/23", &[]), None, "a /23 is too large");
        assert_eq!(hosts("10.0.0.0/8", &[]), None);
    }

    /// One neighbour dump: 192.0.2.50 on br-lan (REACHABLE) and on eth1
    /// (STALE), one address with two MACs; one entry still resolving, one
    /// failed, one set by hand, mDNS's multicast entry, and one on the LTE
    /// bearer.
    fn neighbour_dump(order: Order) -> Vec<Vec<u8>> {
        let messages = [
            nl::neighbour_message(order, 4, NUD_REACHABLE, [192, 0, 2, 50], Some(MAC_A.0)),
            nl::neighbour_message(order, 3, NUD_STALE, [192, 0, 2, 50], Some(MAC_B.0)),
            nl::neighbour_message(order, 4, NUD_INCOMPLETE, [192, 0, 2, 51], None),
            nl::neighbour_message(order, 4, NUD_FAILED, [192, 0, 2, 52], Some(MAC_C.0)),
            nl::neighbour_message(order, 4, NUD_PERMANENT, [192, 0, 2, 53], Some(MAC_C.0)),
            nl::neighbour_message(
                order,
                4,
                NUD_NOARP,
                [224, 0, 0, 251],
                Some([0x01, 0, 0x5e, 0, 0, 0xfb]),
            ),
            nl::neighbour_message(order, 5, NUD_REACHABLE, [10, 180, 41, 232], Some(MAC_D.0)),
        ];
        messages
            .iter()
            .flat_map(|m| {
                Messages::new(m, order)
                    .map(|m| m.payload.to_vec())
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// TRD-9 3.5: the neighbour reader over netlink fixtures, in both byte
    /// orders: each entry with its interface, its state and its MAC.
    #[test]
    fn the_neighbour_reader_keeps_each_entry_s_interface_state_and_mac() {
        for order in [Order::Little, Order::Big] {
            let entries = guard::parse_neighbours(&neighbour_dump(order), order);
            let seen: Vec<(u32, Ipv4Addr, &str, Option<[u8; 6]>)> = entries
                .iter()
                .map(|e| (e.index, e.dst, e.state_word(), e.lladdr))
                .collect();
            assert_eq!(
                seen,
                [
                    (4, ip("192.0.2.50"), "reachable", Some(MAC_A.0)),
                    (3, ip("192.0.2.50"), "stale", Some(MAC_B.0)),
                    (4, ip("192.0.2.51"), "incomplete", None),
                    (4, ip("192.0.2.52"), "failed", Some(MAC_C.0)),
                    (4, ip("192.0.2.53"), "permanent", Some(MAC_C.0)),
                    (
                        4,
                        ip("224.0.0.251"),
                        "noarp",
                        Some([0x01, 0, 0x5e, 0, 0, 0xfb])
                    ),
                    (5, ip("10.180.41.232"), "reachable", Some(MAC_D.0)),
                ],
                "{order:?}"
            );
        }
    }

    /// Phase 1 keeps the LAN side's hosts only: an address two interfaces
    /// name once, an entry set by hand; not one still resolving or failed,
    /// a multicast entry, or one on the LTE bearer.
    #[test]
    fn phase_1_keeps_the_hosts_the_lan_side_s_entries_name() {
        let entries = guard::parse_neighbours(&neighbour_dump(Order::NATIVE), Order::NATIVE);
        let hosts: Vec<Ipv4Addr> = lan_neighbours(&unit(), &entries).into_iter().collect();
        assert_eq!(hosts, [ip("192.0.2.50"), ip("192.0.2.53")]);
    }

    /// TRD-9 3.5: each interface's role, with an injected `[lte] interface`
    /// (`usb0`, `qmimux0`), by its flags (`IFF_NOARP`, `IFF_POINTOPOINT`)
    /// and, with none the unit has, by a modem bearer's name.
    #[test]
    fn each_interface_s_role_comes_from_the_config_then_the_names_then_the_flags() {
        let links = vec![
            link(2, "eth0", ETHER_UP),
            link(3, "usb0", ETHER_UP),
            Link {
                address: None,
                ..link(4, "qmimux0", IFF_UP | IFF_NOARP | IFF_POINTOPOINT)
            },
            link(5, "wwan0", ETHER_UP),
            tunnel(6, "wg0"),
            Link {
                address: None,
                ..link(7, "ppp0", IFF_UP | IFF_POINTOPOINT)
            },
            link(8, "lan9", IFF_UP | IFF_NOARP),
        ];
        let addrs: Vec<Addr> = (2..=8u8)
            .map(|i| addr(u32::from(i), [10, i, 0, 2], 24))
            .collect();
        let roles = |lte: &Lte| -> Vec<(String, Role)> {
            let net = Network::from_dump(&links, &addrs, &[], lte);
            listed_subnets(&net)
                .into_iter()
                .map(|s| (s.iface, s.role))
                .collect()
        };
        let named = |pairs: &[(&str, Role)]| -> Vec<(String, Role)> {
            pairs.iter().map(|(n, r)| ((*n).to_string(), *r)).collect()
        };
        // The configured bearer the unit has: the names decide nothing.
        assert_eq!(
            roles(&Lte::named("usb0")),
            named(&[
                ("eth0", Role::Lan),
                ("usb0", Role::Lte),
                ("qmimux0", Role::Tunnel),
                ("wwan0", Role::Lan),
                ("wg0", Role::Tunnel),
                ("ppp0", Role::Tunnel),
                ("lan9", Role::Tunnel),
            ])
        );
        assert_eq!(
            roles(&Lte::named("qmimux0")),
            named(&[
                ("eth0", Role::Lan),
                ("usb0", Role::Lan),
                ("qmimux0", Role::Lte),
                ("wwan0", Role::Lan),
                ("wg0", Role::Tunnel),
                ("ppp0", Role::Tunnel),
                ("lan9", Role::Tunnel),
            ])
        );
        // No [lte] interface, or one the unit lacks: a bearer's name.
        for lte in [Lte::default(), Lte::named("wwan9")] {
            assert_eq!(
                roles(&lte),
                named(&[
                    ("eth0", Role::Lan),
                    ("usb0", Role::Lte),
                    ("qmimux0", Role::Lte),
                    ("wwan0", Role::Lte),
                    ("wg0", Role::Tunnel),
                    ("ppp0", Role::Tunnel),
                    ("lan9", Role::Tunnel),
                ]),
                "{lte:?}"
            );
        }
    }

    /// The subnet list: every network an interface holds with its role and
    /// source, the loopback and a container bridge left out; a /24 around
    /// each default route's gateway, with its interface's role, unless a
    /// network that interface holds has the gateway.
    #[test]
    fn the_subnet_list_gives_every_network_its_role_and_source() {
        let mut routes = unit_routes();
        routes.push(route(None, Some([192, 168, 9, 1]), 3, 50));
        routes.push(route(None, Some([192, 168, 8, 1]), 3, 60));
        let net = network(&unit_links(), &unit_addrs(), &routes);
        let listed: Vec<Value> = listed_subnets(&net)
            .iter()
            .map(|s| serde_json::to_value(s).unwrap())
            .collect();
        assert_eq!(
            listed,
            [
                json!({"iface": "eth1", "cidr": "192.168.8.0/24", "host_ip": "192.168.8.20", "mac": "02:00:00:00:01:03", "source": "addr", "role": "lan"}),
                json!({"iface": "br-lan", "cidr": "192.0.2.0/24", "host_ip": "192.0.2.1", "mac": "02:00:00:00:01:04", "source": "addr", "role": "lan"}),
                json!({"iface": "wwan0", "cidr": "10.180.41.228/30", "host_ip": "10.180.41.231", "mac": "02:00:00:00:01:05", "source": "addr", "role": "lte"}),
                json!({"iface": "wg0", "cidr": "10.99.0.0/24", "host_ip": "10.99.0.2", "source": "addr", "role": "tunnel"}),
                // The LTE bearer's gateway lies outside its /30.
                json!({"iface": "wwan0", "cidr": "10.180.41.0/24", "mac": "02:00:00:00:01:05", "source": "route", "role": "lte"}),
                json!({"iface": "eth1", "cidr": "192.168.9.0/24", "mac": "02:00:00:00:01:03", "source": "route", "role": "lan"}),
            ]
        );
    }

    /// TRD-9 3.5: with no subnets, a scan picks the unit's own `lan`
    /// networks only: never the LTE, the mesh or a container bridge, nor a
    /// port that is down, nor the /24 guessed from a default route through
    /// the wired WAN port, which the subnet list gives role `lan`.
    #[test]
    fn a_scan_given_no_subnets_picks_the_unit_s_own_lan_networks() {
        let mut routes = unit_routes();
        routes.push(route(None, Some([192, 168, 9, 1]), 3, 50));
        let net = network(&unit_links(), &unit_addrs(), &routes);
        assert!(
            listed_subnets(&net)
                .iter()
                .any(|s| s.cidr == "192.168.9.0/24"
                    && s.role == Role::Lan
                    && s.source == SubnetSource::Route),
            "listed, with role lan"
        );
        assert_eq!(
            default_subnets(&net),
            [cidr("192.168.8.0/24"), cidr("192.0.2.0/24")]
        );
        assert_eq!(default_subnets(&unit()), default_subnets(&net));
        let mut links = unit_links();
        links[2].flags &= !IFF_UP;
        let down = network(&links, &unit_addrs(), &routes);
        assert_eq!(default_subnets(&down), [cidr("192.0.2.0/24")]);
    }

    /// ADR-012 decision 10: nmap and the ping fallback sweep a subnet only
    /// where a no-gateway main-table route through a LAN-side interface
    /// holding an address covers all of it.
    #[test]
    fn a_subnet_is_swept_only_where_a_no_gateway_lan_route_holds_all_of_it() {
        let on =
            |net: &Network, subnet: &str| sweep_iface(net, cidr(subnet)).map(|i| i.name.clone());
        let net = unit();
        assert_eq!(on(&net, "192.0.2.0/24").as_deref(), Some("br-lan"));
        assert_eq!(on(&net, "192.0.2.128/25").as_deref(), Some("br-lan"));
        assert_eq!(
            on(&net, "192.168.8.0/24").as_deref(),
            Some("eth1"),
            "a wired WAN port is on the LAN side"
        );
        assert_eq!(
            on(&net, "192.168.8.0/23"),
            None,
            "half of it lies behind the gateway"
        );
        assert_eq!(on(&net, "203.0.113.0/24"), None, "behind the LTE gateway");
        assert_eq!(
            on(&net, "10.180.41.228/30"),
            None,
            "the LTE bearer's network"
        );
        assert_eq!(on(&net, "10.99.0.0/24"), None, "the mesh");

        // A runbook's /32 on the wired port and a host route: that host only.
        let mut addrs = unit_addrs();
        addrs.push(addr(3, [192, 168, 1, 250], 32));
        let mut routes = unit_routes();
        routes.push(route(Some(([192, 168, 1, 50], 32)), None, 3, 0));
        let runbook = network(&unit_links(), &addrs, &routes);
        assert_eq!(on(&runbook, "192.168.1.50/32").as_deref(), Some("eth1"));
        assert_eq!(on(&runbook, "192.168.1.0/24"), None);

        // A gateway route inside the network: not all of it is on the link.
        routes.push(route(
            Some(([192, 0, 2, 128], 25)),
            Some([192, 0, 2, 5]),
            4,
            0,
        ));
        let split = network(&unit_links(), &addrs, &routes);
        assert_eq!(on(&split, "192.0.2.0/24"), None);
        assert_eq!(on(&split, "192.0.2.0/25").as_deref(), Some("br-lan"));
        assert_eq!(on(&split, "192.0.2.128/25"), None);

        // A port that lost its address, or is down: nothing.
        let mut addrs = unit_addrs();
        addrs.retain(|a| a.index != 3);
        let no_address = network(&unit_links(), &addrs, &unit_routes());
        assert_eq!(on(&no_address, "192.168.8.0/24"), None);
        let mut links = unit_links();
        links[2].flags &= !IFF_UP;
        let down = network(&links, &unit_addrs(), &unit_routes());
        assert_eq!(on(&down, "192.168.8.0/24"), None);
    }

    /// TRD-9 3.5: a scan given no subnets, on a unit with a wired network,
    /// an LTE bearer and a mesh, sweeps the wired networks only, on their
    /// own interfaces; the port probe goes where the guard approves, from
    /// the address it bound; the unit's own addresses are never pinged nor
    /// probed. The /24 guessed from a default route through the wired WAN
    /// port is not swept, nor searched, nor named in `not_swept`.
    #[tokio::test]
    async fn a_scan_given_no_subnets_sweeps_only_the_lan() {
        let mut routes = unit_routes();
        routes.push(route(None, Some([192, 168, 9, 1]), 3, 50));
        let mut fake = Fake::on(network(&unit_links(), &unit_addrs(), &routes));
        fake.arp_scan = Some(HashMap::new());
        fake.up = vec![ip("192.0.2.10"), ip("192.168.8.1")];
        fake.open = HashMap::from([(ip("192.0.2.10"), vec![80, 7000])]);
        fake.after = vec![
            neighbour(4, NUD_REACHABLE, "192.0.2.10", MAC_A),
            neighbour(3, NUD_REACHABLE, "192.168.8.1", MAC_B),
        ];
        let scanned = scan(&fake, &[], &no_progress()).await.unwrap();
        assert!(scanned.not_swept.is_empty(), "{:?}", scanned.not_swept);

        let pings = fake.log_of("ping");
        assert_eq!(pings.len(), 253 + 253, "every host but the unit's own");
        assert!(pings.iter().all(
            |p| p.starts_with("ping br-lan 192.0.2.") || p.starts_with("ping eth1 192.168.8.")
        ));
        for own in ["ping br-lan 192.0.2.1", "ping eth1 192.168.8.20"] {
            assert!(!pings.iter().any(|p| p == own), "{own}");
        }
        assert_eq!(
            fake.log_of("ports"),
            [
                "ports br-lan 192.0.2.10 from 192.0.2.1",
                "ports eth1 192.168.8.1 from 192.168.8.20",
            ],
            "the unit's own addresses are refused by the guard (own_address)"
        );
        assert!(fake.log_of("arp").is_empty(), "every entry was REACHABLE");

        let ips: Vec<&str> = scanned.devices.iter().map(|d| d.ip.as_str()).collect();
        assert_eq!(
            ips,
            ["192.0.2.1", "192.0.2.10", "192.168.8.1", "192.168.8.20"]
        );
        let found = device(&scanned, "192.0.2.10");
        assert_eq!(found.mac.as_deref(), Some("02:00:00:00:00:0a"));
        assert_eq!(found.open_ports, [80], "only the probe's ports");
        assert_eq!(found.inferred_type, "server");
        assert_eq!(found.own, None);
        assert_eq!(
            device(&scanned, "192.168.8.1").mac.as_deref(),
            Some("02:00:00:00:00:0b")
        );
        assert!(
            fake.log().iter().all(|l| !l.contains("192.168.9.")),
            "nothing for the guessed /24: {:?}",
            fake.log_of("arp-scan")
        );
    }

    /// TRD-9 3.5: a named subnet behind a gateway, the LTE bearer's network
    /// and the mesh are not swept (`not_on_lan`): no nmap, no ping, and
    /// arp-scan only on the LAN side, from 0.0.0.0. A host ARP finds keeps
    /// its answer's MAC; the guard keeps it from the port probe.
    #[tokio::test]
    async fn a_named_subnet_off_the_lan_side_is_not_swept_and_is_searched_by_arp_alone() {
        let mut fake = Fake::on(unit());
        fake.nmap = Some(HashMap::new());
        fake.arp_scan = Some(HashMap::from([(
            ("eth1".to_string(), cidr("203.0.113.0/24")),
            vec![Answered {
                ip: ip("203.0.113.9"),
                mac: Some(MAC_D),
            }],
        )]));
        let scanned = scan(
            &fake,
            &requested(&["203.0.113.0/24", "10.180.41.228/30", "10.99.0.0/24"]),
            &no_progress(),
        )
        .await
        .unwrap();
        let not_swept = |text: &str| NotSwept {
            cidr: text.to_string(),
            reason: NotSweptReason::NotOnLan,
        };
        assert_eq!(
            scanned.not_swept,
            [
                not_swept("203.0.113.0/24"),
                not_swept("10.180.41.228/30"),
                not_swept("10.99.0.0/24"),
            ]
        );
        assert!(fake.log_of("nmap").is_empty() && fake.log_of("ping").is_empty());
        assert_eq!(
            fake.log(),
            [
                // A bridge first; eth1 finds the host, so the search stops.
                "arp-scan br-lan 203.0.113.0/24 from 0.0.0.0",
                "arp-scan eth1 203.0.113.0/24 from 0.0.0.0",
                "arp-scan br-lan 10.180.41.228/30 from 0.0.0.0",
                "arp-scan eth1 10.180.41.228/30 from 0.0.0.0",
                "arp-scan br-lan 10.99.0.0/24 from 0.0.0.0",
                "arp-scan eth1 10.99.0.0/24 from 0.0.0.0",
            ],
            "nothing by the LTE bearer, the mesh or the container bridge, no port probe, no ARP request"
        );
        let ips: Vec<&str> = scanned.devices.iter().map(|d| d.ip.as_str()).collect();
        assert_eq!(
            ips,
            ["203.0.113.9"],
            "the unit's LTE and mesh addresses are not found"
        );
        let found = device(&scanned, "203.0.113.9");
        assert_eq!(found.mac.as_deref(), Some("02:00:00:00:00:0d"));
        assert!(found.open_ports.is_empty());

        // Without arp-scan nothing searches them, and nothing is sent.
        let fake = Fake::on(unit());
        let scanned = scan(&fake, &requested(&["203.0.113.0/24"]), &no_progress())
            .await
            .unwrap();
        assert_eq!(scanned.not_swept, [not_swept("203.0.113.0/24")]);
        assert!(fake.log().is_empty(), "{:?}", fake.log());
    }

    /// ADR-012 decision 10: the port probe judges each host by the guard,
    /// one found by ARP alone in a subnet off the LAN side too. With a
    /// runbook's /32 on the wired port and a host route to one device, the
    /// named /24 is not swept, the device under the host route gets the
    /// port probe from the /32, and another host ARP found there, behind
    /// the LTE gateway by the routes, gets none. With a gateway route
    /// splitting br-lan's network, a host in its on-link part gets the
    /// probe and one behind the gateway does not. Each keeps its answer's
    /// MAC.
    #[tokio::test]
    async fn a_host_found_by_arp_alone_gets_the_port_probe_where_the_guard_approves_it() {
        let mut addrs = unit_addrs();
        addrs.retain(|a| a.index != 3);
        addrs.push(addr(3, [192, 168, 1, 250], 32));
        let mut routes = unit_routes();
        routes.retain(|r| r.oif != Some(3));
        routes.push(route(Some(([192, 168, 1, 50], 32)), None, 3, 0));
        let mut fake = Fake::on(network(&unit_links(), &addrs, &routes));
        fake.arp_scan = Some(HashMap::from([(
            ("eth1".to_string(), cidr("192.168.1.0/24")),
            vec![
                Answered {
                    ip: ip("192.168.1.50"),
                    mac: Some(MAC_A),
                },
                Answered {
                    ip: ip("192.168.1.60"),
                    mac: Some(MAC_B),
                },
            ],
        )]));
        let scanned = scan(&fake, &requested(&["192.168.1.0/24"]), &no_progress())
            .await
            .unwrap();
        assert_eq!(
            scanned.not_swept,
            [NotSwept {
                cidr: "192.168.1.0/24".into(),
                reason: NotSweptReason::NotOnLan,
            }]
        );
        assert_eq!(
            fake.log_of("ports"),
            ["ports eth1 192.168.1.50 from 192.168.1.250"]
        );
        let mac = |scanned: &Scanned, at: &str| device(scanned, at).mac.clone();
        assert_eq!(
            mac(&scanned, "192.168.1.50").as_deref(),
            Some("02:00:00:00:00:0a")
        );
        assert_eq!(
            mac(&scanned, "192.168.1.60").as_deref(),
            Some("02:00:00:00:00:0b")
        );

        let mut routes = unit_routes();
        routes.push(route(
            Some(([192, 0, 2, 128], 25)),
            Some([192, 0, 2, 5]),
            4,
            0,
        ));
        let mut fake = Fake::on(network(&unit_links(), &unit_addrs(), &routes));
        fake.arp_scan = Some(HashMap::from([(
            ("br-lan".to_string(), cidr("192.0.2.0/24")),
            vec![
                Answered {
                    ip: ip("192.0.2.20"),
                    mac: Some(MAC_C),
                },
                Answered {
                    ip: ip("192.0.2.200"),
                    mac: Some(MAC_D),
                },
            ],
        )]));
        let scanned = scan(&fake, &requested(&["192.0.2.0/24"]), &no_progress())
            .await
            .unwrap();
        assert_eq!(
            scanned.not_swept,
            [NotSwept {
                cidr: "192.0.2.0/24".into(),
                reason: NotSweptReason::NotOnLan,
            }]
        );
        assert_eq!(
            fake.log_of("ports"),
            ["ports br-lan 192.0.2.20 from 192.0.2.1"]
        );
        assert_eq!(
            mac(&scanned, "192.0.2.20").as_deref(),
            Some("02:00:00:00:00:0c")
        );
        assert_eq!(
            mac(&scanned, "192.0.2.200").as_deref(),
            Some("02:00:00:00:00:0d")
        );
        assert!(fake.log_of("ping").is_empty() && fake.log_of("nmap").is_empty());
    }

    /// ADR-012 decision 13: a LAN subnet larger than the fallback's 254
    /// hosts is swept by nmap or arp-scan, else is `too_large`.
    #[tokio::test]
    async fn a_lan_subnet_larger_than_the_fallback_is_too_large_without_nmap_or_arp_scan() {
        let mut addrs = unit_addrs();
        addrs[1] = addr(4, [10, 0, 0, 1], 23);
        let mut routes = unit_routes();
        routes[1] = route(Some(([10, 0, 0, 0], 23)), None, 4, 0);
        let net = network(&unit_links(), &addrs, &routes);

        let fake = Fake::on(net.clone());
        let scanned = scan(&fake, &[], &no_progress()).await.unwrap();
        assert_eq!(
            scanned.not_swept,
            [NotSwept {
                cidr: "10.0.0.0/23".into(),
                reason: NotSweptReason::TooLarge,
            }]
        );
        assert!(fake
            .log_of("ping")
            .iter()
            .all(|p| p.starts_with("ping eth1 ")));
        assert_eq!(
            device(&scanned, "10.0.0.1").own.as_deref(),
            Some("br-lan"),
            "the unit's own address there is found without a probe"
        );
        let json = serde_json::to_value(&scanned.not_swept).unwrap();
        assert_eq!(
            json,
            json!([{"cidr": "10.0.0.0/23", "reason": "too_large"}])
        );

        let mut fake = Fake::on(net.clone());
        fake.nmap = Some(HashMap::from([(
            cidr("10.0.0.0/23"),
            vec![Answered {
                ip: ip("10.0.1.7"),
                mac: Some(MAC_A),
            }],
        )]));
        let scanned = scan(&fake, &requested(&["10.0.0.0/23"]), &no_progress())
            .await
            .unwrap();
        assert!(scanned.not_swept.is_empty());
        assert_eq!(fake.log_of("nmap"), ["nmap br-lan 10.0.0.0/23"]);
        assert_eq!(
            device(&scanned, "10.0.1.7").mac.as_deref(),
            Some("02:00:00:00:00:0a")
        );

        let mut fake = Fake::on(net);
        fake.arp_scan = Some(HashMap::new());
        let scanned = scan(&fake, &requested(&["10.0.0.0/23"]), &no_progress())
            .await
            .unwrap();
        assert!(
            scanned.not_swept.is_empty(),
            "arp-scan swept it, finding nothing"
        );
        assert_eq!(
            fake.log(),
            ["arp-scan br-lan 10.0.0.0/23"],
            "the unit's own address gets no port probe"
        );
    }

    /// arp-scan cannot search more than a /19 within its 60 s limit, and a
    /// run cut at the limit keeps none of its answers, so it is not run on
    /// a larger subnet: a /16 the unit holds is swept by nmap, else is
    /// `too_large`; a /19 inside it is searched and a /18 is not; off the
    /// LAN side a named /16 is not searched by ARP alone, and a /24 is.
    #[tokio::test]
    async fn arp_scan_runs_only_on_a_subnet_it_can_search_within_its_limit() {
        let mut addrs = unit_addrs();
        addrs[1] = addr(4, [10, 0, 0, 1], 16);
        let mut routes = unit_routes();
        routes[1] = route(Some(([10, 0, 0, 0], 16)), None, 4, 0);
        let net = network(&unit_links(), &addrs, &routes);
        let too_large = |text: &str| NotSwept {
            cidr: text.to_string(),
            reason: NotSweptReason::TooLarge,
        };
        let not_on_lan = |text: &str| NotSwept {
            cidr: text.to_string(),
            reason: NotSweptReason::NotOnLan,
        };

        // A scan given no subnets, without nmap.
        let mut fake = Fake::on(net.clone());
        fake.arp_scan = Some(HashMap::new());
        let scanned = scan(&fake, &[], &no_progress()).await.unwrap();
        assert_eq!(scanned.not_swept, [too_large("10.0.0.0/16")]);
        assert_eq!(fake.log_of("arp-scan"), ["arp-scan eth1 192.168.8.0/24"]);
        assert!(fake
            .log_of("ping")
            .iter()
            .all(|p| p.starts_with("ping eth1 192.168.8.")));

        // nmap sweeps it.
        let mut fake = Fake::on(net.clone());
        fake.nmap = Some(HashMap::new());
        fake.arp_scan = Some(HashMap::new());
        let scanned = scan(&fake, &requested(&["10.0.0.0/16"]), &no_progress())
            .await
            .unwrap();
        assert!(scanned.not_swept.is_empty(), "{:?}", scanned.not_swept);
        assert_eq!(fake.log_of("nmap"), ["nmap br-lan 10.0.0.0/16"]);
        assert!(fake.log_of("arp-scan").is_empty(), "{:?}", fake.log());

        // The bound: a /19's 8190 hosts, not a /18's.
        let mut fake = Fake::on(net.clone());
        fake.arp_scan = Some(HashMap::new());
        let scanned = scan(
            &fake,
            &requested(&["10.0.32.0/19", "10.0.64.0/18"]),
            &no_progress(),
        )
        .await
        .unwrap();
        assert_eq!(scanned.not_swept, [too_large("10.0.64.0/18")]);
        assert_eq!(fake.log_of("arp-scan"), ["arp-scan br-lan 10.0.32.0/19"]);
        assert_eq!(ARP_SCAN_MAX, cidr("10.0.32.0/19").host_count());

        // Off the LAN side, by ARP alone.
        let mut fake = Fake::on(net);
        fake.arp_scan = Some(HashMap::new());
        let scanned = scan(
            &fake,
            &requested(&["172.16.0.0/16", "172.16.5.0/24"]),
            &no_progress(),
        )
        .await
        .unwrap();
        assert_eq!(
            scanned.not_swept,
            [not_on_lan("172.16.0.0/16"), not_on_lan("172.16.5.0/24")]
        );
        assert_eq!(
            fake.log_of("arp-scan"),
            [
                "arp-scan br-lan 172.16.5.0/24 from 0.0.0.0",
                "arp-scan eth1 172.16.5.0/24 from 0.0.0.0",
            ]
        );
    }

    /// TRD-9 3.5: the fallback never has more than 32 pings in flight.
    #[tokio::test]
    async fn the_fallback_never_has_more_than_32_pings_in_flight() {
        let fake = Fake::on(unit());
        let targets = fallback_targets(cidr("192.0.2.0/24"), &[ip("192.0.2.1")]).unwrap();
        let answered = ping_all(&fake, &fake.net, "br-lan", &targets).await;
        assert!(answered.is_empty());
        assert_eq!(fake.log_of("ping").len(), 253);
        assert_eq!(fake.most_in_flight.load(Ordering::SeqCst), AT_ONCE);
        assert_eq!(AT_ONCE, 32);
    }

    /// ADR-012 decisions 1 and 10: the ping fallback pings only a target
    /// the guard passes on the interface it sweeps. br-lan's directed
    /// broadcast, named as a /32 or inside a /31, and a multicast range
    /// under a no-gateway default route through br-lan lie on the LAN side
    /// as the routes read, and none of their broadcast or multicast
    /// addresses is pinged; the /31's other address is.
    #[tokio::test]
    async fn the_ping_fallback_pings_no_broadcast_or_multicast_address() {
        let mut routes = unit_routes();
        routes.push(route(None, None, 4, 10));
        let fake = Fake::on(network(&unit_links(), &unit_addrs(), &routes));
        let named = ["192.0.2.255/32", "192.0.2.254/31", "233.252.0.0/30"];
        for subnet in named {
            assert_eq!(
                sweep_iface(&fake.net, cidr(subnet)).map(|i| i.name.as_str()),
                Some("br-lan"),
                "{subnet}"
            );
        }
        let scanned = scan(&fake, &requested(&named), &no_progress())
            .await
            .unwrap();
        assert!(scanned.not_swept.is_empty(), "{:?}", scanned.not_swept);
        assert_eq!(fake.log_of("ping"), ["ping br-lan 192.0.2.254"]);
        assert!(
            fake.log()
                .iter()
                .all(|l| !l.contains("192.0.2.255") && !l.contains("233.252.0.")),
            "{:?}",
            fake.log()
        );
        // The guard's own reasons: the kernel's broadcast route, and a
        // multicast address whatever the route says.
        for (at, kind) in [
            ("192.0.2.255", RouteKind::Broadcast),
            ("233.252.0.1", RouteKind::Multicast),
        ] {
            let answer = kernel(&fake.net, ip(at));
            assert!(
                matches!(&answer, guard::Answer::Route(r) if r.kind == kind),
                "{answer:?}"
            );
            let refusal = guard::judge(&fake.net, ip(at), &answer, None, false).unwrap_err();
            assert_eq!(refusal.refused, guard::Code::NotUnicast, "{at}");
        }
    }

    /// ADR-012 decision 11, step by step: the sweep's answers and a
    /// REACHABLE or permanent entry are each a MAC seen answering; one is
    /// the host's, two give none; with none seen, the wire is asked.
    #[test]
    fn a_host_s_mac_is_the_one_seen_by_the_sweep_or_a_reachable_entry_else_the_wire_s() {
        let entry = |state: u16| neighbour(4, state, "192.0.2.50", MAC_B);
        for state in [NUD_STALE, NUD_DELAY, 0x10, NUD_FAILED, NUD_INCOMPLETE] {
            assert_eq!(
                mac_step(&[MAC_A], Some(&entry(state))),
                MacStep::Known(Some(MAC_A)),
                "the sweep's answer over an older entry, {state:#x}"
            );
        }
        for state in [NUD_REACHABLE, NUD_PERMANENT] {
            assert_eq!(
                mac_step(&[MAC_A], Some(&entry(state))),
                MacStep::Known(None),
                "the sweep heard one MAC and the entry names another, {state:#x}"
            );
            assert_eq!(
                mac_step(&[MAC_B], Some(&entry(state))),
                MacStep::Known(Some(MAC_B)),
                "both name one MAC, {state:#x}"
            );
        }
        assert_eq!(mac_step(&[MAC_A], None), MacStep::Known(Some(MAC_A)));
        assert_eq!(mac_step(&[MAC_A, MAC_C], None), MacStep::Known(None));
        assert_eq!(
            mac_step(&[MAC_A, MAC_B], Some(&entry(NUD_REACHABLE))),
            MacStep::Known(None)
        );
        for state in [NUD_REACHABLE, NUD_PERMANENT] {
            assert_eq!(
                mac_step(&[], Some(&entry(state))),
                MacStep::Known(Some(MAC_B))
            );
        }
        for state in [NUD_STALE, NUD_DELAY, 0x10, NUD_FAILED, NUD_INCOMPLETE] {
            assert_eq!(
                mac_step(&[], Some(&entry(state))),
                MacStep::Ask,
                "{state:#x}"
            );
        }
        assert_eq!(mac_step(&[], None), MacStep::Ask);
        assert_eq!(one_mac(&[MAC_A, MAC_A]), Some(MAC_A));
        assert_eq!(one_mac(&[MAC_A, MAC_B]), None);
        assert_eq!(one_mac(&[]), None);
    }

    /// TRD-9 3.5: the sweep's answer is the MAC, whatever an older entry
    /// says (STALE before the sweep and still STALE after it), and no ARP
    /// request follows.
    #[tokio::test]
    async fn a_sweep_s_answer_wins_over_an_older_entry() {
        let mut fake = Fake::on(unit());
        fake.arp_scan = Some(HashMap::from([(
            ("br-lan".to_string(), cidr("192.0.2.0/24")),
            vec![Answered {
                ip: ip("192.0.2.50"),
                mac: Some(MAC_B),
            }],
        )]));
        fake.before = vec![neighbour(4, NUD_STALE, "192.0.2.50", MAC_A)];
        fake.after = vec![neighbour(4, NUD_STALE, "192.0.2.50", MAC_A)];
        let scanned = scan(&fake, &requested(&["192.0.2.0/24"]), &no_progress())
            .await
            .unwrap();
        assert_eq!(
            device(&scanned, "192.0.2.50").mac.as_deref(),
            Some("02:00:00:00:00:0b")
        );
        assert!(fake.log_of("arp").is_empty(), "{:?}", fake.log());
    }

    /// ADR-012 decision 11: nmap reports one MAC per host, and the kernel
    /// keeps the first answer to its own request, so when two devices
    /// answer at one address the two can name different MACs. A REACHABLE
    /// or permanent entry that names another MAC than the sweep's is a
    /// second MAC seen: the host gets none, and no ARP request follows.
    #[tokio::test]
    async fn a_reachable_entry_naming_another_mac_than_the_sweep_s_gives_none() {
        let mut fake = Fake::on(unit());
        let heard = |at: &str| Answered {
            ip: ip(at),
            mac: Some(MAC_B),
        };
        fake.nmap = Some(HashMap::from([(
            cidr("192.0.2.0/24"),
            vec![
                heard("192.0.2.73"),
                heard("192.0.2.74"),
                heard("192.0.2.75"),
                heard("192.0.2.76"),
            ],
        )]));
        fake.after = vec![
            neighbour(4, NUD_REACHABLE, "192.0.2.73", MAC_A),
            neighbour(4, NUD_PERMANENT, "192.0.2.74", MAC_A),
            neighbour(4, NUD_REACHABLE, "192.0.2.75", MAC_B),
            // On another interface: not the one that reaches the host.
            neighbour(3, NUD_REACHABLE, "192.0.2.76", MAC_A),
        ];
        let scanned = scan(&fake, &requested(&["192.0.2.0/24"]), &no_progress())
            .await
            .unwrap();
        let mac = |at: &str| device(&scanned, at).mac.clone();
        assert_eq!(mac("192.0.2.73"), None, "nmap's B, a REACHABLE A");
        assert_eq!(mac("192.0.2.74"), None, "nmap's B, a permanent A");
        assert_eq!(mac("192.0.2.75").as_deref(), Some("02:00:00:00:00:0b"));
        assert_eq!(mac("192.0.2.76").as_deref(), Some("02:00:00:00:00:0b"));
        assert!(fake.log_of("arp").is_empty(), "{:?}", fake.log());
    }

    /// TRD-9 3.5: a STALE entry leads to an ARP request, whose one answer
    /// is the MAC; a REACHABLE or permanent entry on the interface that
    /// reaches the host is taken as it is; an entry on another interface
    /// counts for nothing.
    #[tokio::test]
    async fn a_stale_entry_leads_to_an_arp_request() {
        let mut fake = Fake::on(unit());
        fake.up = [
            "192.0.2.60",
            "192.0.2.61",
            "192.0.2.62",
            "192.0.2.63",
            "192.0.2.64",
        ]
        .iter()
        .map(|a| ip(a))
        .collect();
        fake.after = vec![
            neighbour(4, NUD_STALE, "192.0.2.60", MAC_A),
            neighbour(4, NUD_REACHABLE, "192.0.2.61", MAC_B),
            neighbour(4, NUD_PERMANENT, "192.0.2.62", MAC_C),
            neighbour(3, NUD_REACHABLE, "192.0.2.63", MAC_D),
            neighbour(4, NUD_DELAY, "192.0.2.64", MAC_A),
        ];
        fake.arp = HashMap::from([
            (ip("192.0.2.60"), vec![MAC_C]),
            (ip("192.0.2.63"), vec![MAC_D]),
        ]);
        let scanned = scan(&fake, &requested(&["192.0.2.0/24"]), &no_progress())
            .await
            .unwrap();
        let mac = |at: &str| device(&scanned, at).mac.clone();
        assert_eq!(
            mac("192.0.2.60").as_deref(),
            Some("02:00:00:00:00:0c"),
            "the answer, not the stale entry"
        );
        assert_eq!(mac("192.0.2.61").as_deref(), Some("02:00:00:00:00:0b"));
        assert_eq!(mac("192.0.2.62").as_deref(), Some("02:00:00:00:00:0c"));
        assert_eq!(mac("192.0.2.63").as_deref(), Some("02:00:00:00:00:0d"));
        assert_eq!(mac("192.0.2.64"), None, "nothing answered");
        assert_eq!(
            fake.log_of("arp"),
            [
                "arp br-lan 192.0.2.60 from 192.0.2.1",
                "arp br-lan 192.0.2.63 from 192.0.2.1",
                "arp br-lan 192.0.2.64 from 192.0.2.1",
            ]
        );
    }

    /// TRD-9 3.5: an address two MACs answered for gets none, from the sweep
    /// or from the ARP request.
    #[tokio::test]
    async fn two_macs_give_none() {
        let mut fake = Fake::on(unit());
        fake.arp_scan = Some(HashMap::from([(
            ("br-lan".to_string(), cidr("192.0.2.0/24")),
            vec![
                Answered {
                    ip: ip("192.0.2.70"),
                    mac: Some(MAC_A),
                },
                Answered {
                    ip: ip("192.0.2.71"),
                    mac: Some(MAC_C),
                },
                Answered {
                    ip: ip("192.0.2.70"),
                    mac: Some(MAC_B),
                },
            ],
        )]));
        fake.before = vec![neighbour(4, NUD_STALE, "192.0.2.72", MAC_A)];
        fake.after = vec![
            neighbour(4, NUD_REACHABLE, "192.0.2.70", MAC_A),
            neighbour(4, NUD_STALE, "192.0.2.72", MAC_A),
        ];
        fake.arp = HashMap::from([(ip("192.0.2.72"), vec![MAC_A, MAC_B])]);
        let scanned = scan(&fake, &requested(&["192.0.2.0/24"]), &no_progress())
            .await
            .unwrap();
        assert_eq!(device(&scanned, "192.0.2.70").mac, None);
        assert_eq!(
            device(&scanned, "192.0.2.71").mac.as_deref(),
            Some("02:00:00:00:00:0c")
        );
        assert_eq!(device(&scanned, "192.0.2.72").mac, None);
        assert_eq!(
            fake.log_of("arp"),
            ["arp br-lan 192.0.2.72 from 192.0.2.1"],
            "none for the address the sweep heard two MACs for"
        );
    }

    /// TRD-9 3.5: `own` by address and by MAC, a bridge named before the
    /// port it shares its MAC with; the unit's own addresses are never
    /// asked by ARP.
    #[tokio::test]
    async fn the_unit_s_own_addresses_and_macs_carry_own() {
        let mut links = unit_links();
        links[1].address = Some(BR_LAN_MAC.0);
        let mut fake = Fake::on(network(&links, &unit_addrs(), &unit_routes()));
        fake.arp_scan = Some(HashMap::from([(
            ("br-lan".to_string(), cidr("192.0.2.0/24")),
            vec![
                Answered {
                    ip: ip("192.0.2.80"),
                    mac: Some(BR_LAN_MAC),
                },
                Answered {
                    ip: ip("192.0.2.81"),
                    mac: Some(MAC_A),
                },
                // Another device at the unit's own address.
                Answered {
                    ip: ip("192.0.2.1"),
                    mac: Some(MAC_C),
                },
            ],
        )]));
        let scanned = scan(&fake, &[], &no_progress()).await.unwrap();
        let own = |at: &str| device(&scanned, at).own.clone();
        assert_eq!(own("192.0.2.1").as_deref(), Some("br-lan"));
        assert_eq!(own("192.168.8.20").as_deref(), Some("eth1"));
        assert_eq!(own("192.0.2.80").as_deref(), Some("br-lan"), "by its MAC");
        assert_eq!(own("192.0.2.81"), None);
        assert!(
            !fake
                .log()
                .iter()
                .any(|l| l.ends_with(" 192.168.8.20") && l.starts_with("ping")),
            "the fallback skips the unit's addresses"
        );
        let json = serde_json::to_value(device(&scanned, "192.0.2.1")).unwrap();
        assert_eq!(json["own"], "br-lan");
        let json = serde_json::to_value(device(&scanned, "192.0.2.81")).unwrap();
        assert!(json.get("own").is_none(), "{json}");
        // The unit's own addresses are never asked: one carries a MAC only
        // when another device answered the sweep there, an address
        // conflict.
        assert_eq!(
            device(&scanned, "192.0.2.1").mac.as_deref(),
            Some("02:00:00:00:00:0c")
        );
        assert_eq!(device(&scanned, "192.168.8.20").mac, None);
        assert!(
            fake.log_of("arp").iter().all(|l| {
                let asked = l.split(' ').nth(2);
                asked != Some("192.0.2.1") && asked != Some("192.168.8.20")
            }),
            "{:?}",
            fake.log_of("arp")
        );
    }

    /// A scan that cannot read the unit's network judges nothing, so it
    /// sends nothing.
    #[tokio::test]
    async fn a_scan_that_cannot_read_the_network_sends_nothing() {
        let mut fake = Fake::on(unit());
        fake.unreadable = true;
        fake.up = vec![ip("192.0.2.10")];
        let error = scan(&fake, &[], &no_progress())
            .await
            .err()
            .expect("refused");
        assert!(
            error.starts_with("cannot read this unit's network"),
            "{error}"
        );
        assert!(fake.log().is_empty());
    }

    /// ADR-012 decision 10: nmap, arp-scan and the ping fallback send only
    /// on the interface the sweep chose: each tool's arguments name it, and
    /// arp-scan's search off the LAN side goes from 0.0.0.0.
    #[test]
    fn each_sweep_tool_sends_by_the_interface_the_sweep_chose() {
        let subnet = cidr("192.0.2.0/24");
        assert_eq!(
            nmap_args("br-lan", subnet),
            ["-sn", "-n", "-e", "br-lan", "192.0.2.0/24"]
        );
        assert_eq!(
            arp_scan_args("eth1", subnet, false),
            [
                "--interface=eth1",
                "--retry=2",
                "--timeout=300",
                "192.0.2.0/24"
            ]
        );
        assert_eq!(
            arp_scan_args("eth1", subnet, true),
            [
                "--interface=eth1",
                "--retry=2",
                "--timeout=300",
                "--arpspa=0.0.0.0",
                "--destaddr=ff:ff:ff:ff:ff:ff",
                "192.0.2.0/24"
            ]
        );
        assert_eq!(
            ping_args("br-lan", ip("192.0.2.7")),
            ["-c", "1", "-W", "1", "-I", "br-lan", "192.0.2.7"]
        );
    }

    /// ADR-012 decision 10: the port probe connects from the socket the
    /// guard bound. On the loopback the port opens; with an interface the
    /// unit lacks the bind fails, nothing is sent and the port reads
    /// closed, where an unbound connect would open it.
    #[tokio::test]
    async fn the_port_probe_connects_from_the_socket_the_guard_bound() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let probes = AgentProbes {
            lte: Lte::default(),
        };
        let approved = guard::Guard::default()
            .approve(Ipv4Addr::LOCALHOST)
            .await
            .unwrap();
        assert_eq!(approved.dev, "lo");
        assert_eq!(probes.open_ports(&approved, &[port]).await, [port]);
        let gone = Approved {
            dev: "sctl-none0".into(),
            ..approved
        };
        assert_eq!(
            probes.open_ports(&gone, &[port]).await,
            Vec::<u16>::new(),
            "bound to an interface the unit lacks, nothing is sent"
        );
    }

    #[test]
    fn nmap_s_report_gives_the_hosts_up_and_the_macs_it_heard() {
        let report = "Starting Nmap 7.94 ( https://nmap.org ) at 2026-10-24 12:00 UTC\n\
            Nmap scan report for 192.0.2.1\n\
            Host is up.\n\
            Nmap scan report for 192.0.2.10\n\
            Host is up (0.00042s latency).\n\
            MAC Address: 02:00:00:00:00:0A (Unknown)\n\
            Nmap scan report for printer.example (192.0.2.11)\n\
            Host is up (0.00051s latency).\n\
            MAC Address: 02:00:00:00:00:0B (Unknown)\n\
            Nmap scan report for 192.0.2.12 [host down]\n\
            MAC Address: 02:00:00:00:00:0C (Unknown)\n\
            Nmap done: 256 IP addresses (3 hosts up) scanned in 1.92 seconds\n";
        assert_eq!(
            parse_nmap(report),
            [
                Answered {
                    ip: ip("192.0.2.1"),
                    mac: None,
                },
                Answered {
                    ip: ip("192.0.2.10"),
                    mac: Some(MAC_A),
                },
                Answered {
                    ip: ip("192.0.2.11"),
                    mac: Some(MAC_B),
                },
            ]
        );
    }

    #[test]
    fn arp_scan_s_lines_give_every_answer_a_second_one_included() {
        let output = "Interface: br-lan, type: EN10MB, MAC: 02:00:00:00:01:04, IPv4: 192.0.2.1\n\
            Starting arp-scan 1.10.0 with 256 hosts (https://github.com/royhills/arp-scan)\n\
            192.0.2.10\t02:00:00:00:00:0a\t(Unknown: locally administered)\n\
            192.0.2.70\t02:00:00:00:00:0a\t(Unknown: locally administered)\n\
            192.0.2.70\t02:00:00:00:00:0b\t(Unknown: locally administered) (DUP: 2)\n\
            \n\
            3 packets received by filter, 0 packets dropped by kernel\n\
            Ending arp-scan 1.10.0: 256 hosts scanned in 1.871 seconds (136.83 hosts/sec). 3 responded\n";
        assert_eq!(
            parse_arp_scan(output),
            [
                Answered {
                    ip: ip("192.0.2.10"),
                    mac: Some(MAC_A),
                },
                Answered {
                    ip: ip("192.0.2.70"),
                    mac: Some(MAC_A),
                },
                Answered {
                    ip: ip("192.0.2.70"),
                    mac: Some(MAC_B),
                },
            ]
        );
    }

    /// The passive sniff runs tcpdump itself, as an argument vector: no
    /// shell, no file in /tmp, the filter one argument.
    #[test]
    fn the_sniff_runs_tcpdump_as_an_argument_vector() {
        assert_eq!(
            sniff_args("br-lan"),
            ["-i", "br-lan", "-nn", "-l", "-c", "200", SNIFF_FILTER]
        );
        assert!(SNIFF_FILTER.starts_with("arp or (udp and (port 67 "));
    }

    /// What a child printed by the deadline is kept, a line the deadline
    /// cuts left out; no more than `max` lines are taken; the end of the
    /// stream ends the read; bytes that are not UTF-8 are replaced.
    #[tokio::test]
    async fn a_listened_child_s_lines_are_kept_to_the_deadline() {
        use tokio::io::AsyncWriteExt;
        let arp = "ARP, Request who-has 192.0.2.1 tell 192.0.2.7, length 46";
        let (mut writer, reader) = tokio::io::duplex(4096);
        writer
            .write_all(format!("{arp}\nhalf a li").as_bytes())
            .await
            .unwrap();
        let started = Instant::now();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
        let lines = read_lines_until(reader, deadline, 10).await;
        assert_eq!(lines, [arp], "the writer is still open");
        assert!(started.elapsed() < Duration::from_secs(5));

        let (mut writer, reader) = tokio::io::duplex(4096);
        writer.write_all(b"one\ntwo\nthree\n").await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        assert_eq!(read_lines_until(reader, deadline, 2).await, ["one", "two"]);

        let (mut writer, reader) = tokio::io::duplex(4096);
        writer.write_all(b"one\n\xff two\n").await.unwrap();
        drop(writer);
        let started = Instant::now();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        assert_eq!(
            read_lines_until(reader, deadline, 10).await,
            ["one", "\u{fffd} two"]
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the end ends it"
        );
    }

    /// A child is run as an argument vector and read to its end; one still
    /// running at its limit is killed, so the listen ends with the limit;
    /// one the unit lacks is None. Each program only prints or sleeps.
    #[tokio::test]
    async fn a_listened_child_is_killed_at_its_limit_and_a_missing_one_is_none() {
        // Only printed: no shell reads the second argument.
        let probe = "a;b $(printf EXPANDED)";
        let printed = listen(
            "printf",
            &["%s\n", "one", probe],
            Duration::from_secs(30),
            10,
        )
        .await;
        assert_eq!(printed.unwrap(), ["one", probe]);

        let started = Instant::now();
        let slept = listen("sleep", &["30"], Duration::from_millis(300), 10).await;
        assert_eq!(slept.unwrap(), Vec::<String>::new());
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "killed at its limit, not waited for: {:?}",
            started.elapsed()
        );

        assert_eq!(
            listen("sctl-no-such-program-0", &[], Duration::from_secs(1), 10).await,
            None
        );
    }
}
