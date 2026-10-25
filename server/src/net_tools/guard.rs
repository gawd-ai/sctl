//! The guard of the net tools (ADR-012): before `sctl net snmp`, `http` or
//! `tcp` sends anything, and before the Infra checks that send a stored
//! secret (SNMP, the Peplink login) do, it asks the kernel for the route to
//! the destination and lets the request go only when it stays on the
//! unit's LAN side. The socket is then bound to the interface it approved
//! and to the route's source address, so an address that goes away, or a
//! mark that would route the packet again (mwan3), makes the send fail
//! rather than move.
//!
//! - **The LAN side.** An interface that is up, not loopback, takes ARP
//!   (`IFF_NOARP` clear), is not point-to-point and is not the LTE bearer.
//!   The LTE bearer is the agent's `[lte] interface`; with no config, none
//!   naming `[lte]`, or one naming an interface the unit lacks, an interface
//!   named like a supported model's modem bearer (`wwan*`, `qmimux*`,
//!   `usb*`) counts as LTE, and the answer says the name decided it.
//! - **On the LAN.** The kernel's route (`source::route_get`) is unicast,
//!   has no gateway and leaves by a LAN-side interface holding an IPv4
//!   address. A destination the unit holds itself passes: nothing leaves.
//!   With `--dev` the named interface must be on the LAN side, hold an
//!   address, and the destination must lie under a main-table route through
//!   it with no gateway, judged from the routes, never from a lookup tied to
//!   the interface (the kernel answers that one as if any address were on
//!   the link).
//! - **`--expect-mac`.** One ARP request on the approved interface, from a
//!   packet socket; every answer, and the kernel's neighbour entry if it
//!   holds one, must name the expected MAC.
//!
//! A refusal ([`Refusal`]) says why and that nothing was sent: at most a
//! name lookup and the ARP request left the unit. It never carries a
//! secret: the tools pass none to the guard.

use std::cmp::Reverse;
use std::ffi::OsString;
use std::fmt;
use std::io::{self, Read};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde::{Serialize, Serializer};
use serde_json::{json, Value};
use tokio::io::unix::AsyncFd;
use tokio::net::{TcpSocket, TcpStream, UdpSocket};

use crate::config::Config;
use crate::netwatch::netlink::{self, Addr, Link, NlSocket, Order, Route};
use crate::netwatch::source::{self, KernelRoute, RouteKind};

/// The variable the agent sets, in the environment of every command it
/// runs, to the absolute path of the config file it loaded.
pub const AGENT_CONFIG_ENV: &str = "SCTL_AGENT_CONFIG";

/// Names a supported model gives its modem's bearer: `wwan0` (the XE300,
/// framed like Ethernet), `qmimux0` (the RUT241), `usb0` (the WE826).
const BEARER_PREFIXES: [&str; 3] = ["wwan", "qmimux", "usb"];

/// The longest wait for answers to the ARP request, and the shortest.
const ARP_WAIT_MAX: Duration = Duration::from_secs(1);
const ARP_WAIT_MIN: Duration = Duration::from_millis(100);
/// After the first answer, how long a second one is still waited for.
const ARP_SETTLE: Duration = Duration::from_millis(300);

/// The largest config file a tool reads: a unit's is a few KiB.
const MAX_CONFIG_BYTES: u64 = 1 << 20;

// ─── refusals ───────────────────────────────────────────────────────────

/// Why a request was refused. Every code means the request never left.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Code {
    /// The kernel would send it through a gateway, by an interface off the
    /// LAN side, or nowhere.
    NotOnLan,
    /// The interface it would leave by holds no IPv4 address.
    NoAddress,
    /// A broadcast or multicast destination: the tools speak to one device.
    NotUnicast,
    /// An IPv6 destination: the agent reads IPv4 networks only.
    Ipv6,
    /// `--expect-mac` with a destination the unit holds itself.
    OwnAddress,
    /// Another MAC answered, two did, or the kernel's entry names another.
    MacMismatch,
    /// Nothing answered the ARP request.
    NoAnswer,
    /// The agent's config could not be read or parsed.
    ConfigUnreadable,
    /// The guard could not read the routes, ask the wire or bind.
    GuardFailed,
}

impl Code {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotOnLan => "not_on_lan",
            Self::NoAddress => "no_address",
            Self::NotUnicast => "not_unicast",
            Self::Ipv6 => "ipv6",
            Self::OwnAddress => "own_address",
            Self::MacMismatch => "mac_mismatch",
            Self::NoAnswer => "no_answer",
            Self::ConfigUnreadable => "config_unreadable",
            Self::GuardFailed => "guard_failed",
        }
    }
}

impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What an interface is to the guard.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Takes ARP, is not point-to-point and is not the LTE bearer: the
    /// site's network, a wired WAN port included.
    Lan,
    /// The LTE bearer.
    Lte,
    /// A point-to-point or no-ARP link: WireGuard, tun, PPP, a raw-IP modem.
    Tunnel,
}

/// What decided an interface's role.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RoleBy {
    /// The agent's `[lte] interface`.
    Config,
    /// A modem bearer's name, with no `[lte] interface` the unit has.
    Name,
    /// The link's flags.
    Flags,
}

/// The route the kernel would have used, as a refusal shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RouteView {
    pub dev: String,
    /// None for the loopback.
    pub role: Option<Role>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role_by: Option<RoleBy>,
    pub gateway: Option<Ipv4Addr>,
    #[serde(rename = "type")]
    pub kind: &'static str,
}

/// One LAN-side interface and its IPv4 addresses, as a refusal shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LanView {
    pub dev: String,
    pub ipv4: Vec<String>,
}

/// A refused request: printed as one JSON object, exit 3.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Refusal {
    pub refused: Code,
    /// One line, for people.
    pub reason: String,
    /// The address judged, or the host as given before one was.
    pub host: String,
    /// The name as given, when the host was a name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `--dev`, as given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dev: Option<String>,
    /// The route the kernel would have used; absent when it has none (an
    /// IPv6 destination has none it is asked for) or it could not be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route: Option<RouteView>,
    /// The unit's LAN side, by interface; absent when the unit's network,
    /// or the config that gives its roles, could not be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lan: Option<Vec<LanView>>,
    /// After an ARP request: the MACs that answered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub macs: Option<Vec<Mac>>,
    /// After an ARP request: the MAC the kernel's neighbour entry names.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub neighbour: Option<Mac>,
}

impl Refusal {
    pub fn new(code: Code, reason: impl Into<String>, host: impl Into<String>) -> Self {
        Self {
            refused: code,
            reason: reason.into(),
            host: host.into(),
            name: None,
            dev: None,
            route: None,
            lan: None,
            macs: None,
            neighbour: None,
        }
    }

    /// The refusal with what the guard read about the request.
    #[must_use]
    pub fn with_seen(mut self, seen: &Seen) -> Self {
        self.dev.clone_from(&seen.dev);
        self.route.clone_from(&seen.route);
        self.lan.clone_from(&seen.lan);
        self
    }

    /// The object a tool prints: `ok: false` and the refusal's fields.
    pub fn to_json(&self) -> Value {
        let mut value = serde_json::to_value(self).unwrap_or_else(|_| json!({}));
        value["ok"] = json!(false);
        value
    }
}

/// What the guard read about a request, as a refusal of it shows (ADR-012
/// decision 5): `--dev` as given, the route the kernel would have used and
/// the unit's LAN side. Each is None when the guard did not read it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Seen {
    pub dev: Option<String>,
    pub route: Option<RouteView>,
    pub lan: Option<Vec<LanView>>,
}

/// Why a guarded request did not start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stop {
    /// The guard refused it: nothing was sent (exit 3).
    Refused(Box<Refusal>),
    /// The host's name could not be looked up (exit 1).
    Failed(String),
}

// ─── MACs ───────────────────────────────────────────────────────────────

/// A MAC address, written `aa:bb:cc:dd:ee:ff`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Mac(pub [u8; 6]);

impl Mac {
    /// Six bytes of two hex digits each, separated by `:` or `-`, in
    /// either case. Anything else is None.
    pub fn parse(text: &str) -> Option<Self> {
        let mut bytes = [0u8; 6];
        let mut parts = text.split([':', '-']);
        for byte in &mut bytes {
            let part = parts.next()?;
            if part.len() != 2 || !part.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            *byte = u8::from_str_radix(part, 16).ok()?;
        }
        parts.next().is_none().then_some(Self(bytes))
    }
}

impl fmt::Display for Mac {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, byte) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(":")?;
            }
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl Serialize for Mac {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

// ─── the unit's network ─────────────────────────────────────────────────

/// What names the LTE bearer: the agent's `[lte] interface`, when its
/// config has one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Lte {
    configured: Option<String>,
}

impl Lte {
    /// From the agent's config; None, or a config without `[lte]`, names
    /// none, and the bearer names decide.
    pub fn from_config(config: Option<&Config>) -> Self {
        Self {
            configured: config.and_then(|c| c.lte.as_ref().map(|l| l.interface.clone())),
        }
    }

    /// The bearer named `interface`, as an `[lte] interface` would.
    pub fn named(interface: &str) -> Self {
        Self {
            configured: Some(interface.to_string()),
        }
    }
}

/// One interface of the unit, as the guard judges it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Iface {
    pub index: u32,
    pub name: String,
    /// `IFF_*`.
    flags: u32,
    /// `ifi_type`.
    kind: u16,
    pub mac: Option<[u8; 6]>,
    /// The bridge (or bond) this is a port of, by name, when enslaved.
    pub port_of: Option<String>,
    enslaved: bool,
    pub role: Role,
    pub role_by: RoleBy,
    /// Every IPv4 address with its prefix, primary ones first.
    pub ipv4: Vec<(Ipv4Addr, u8)>,
}

impl Iface {
    pub fn up(&self) -> bool {
        self.flags & netlink::IFF_UP != 0
    }

    pub fn loopback(&self) -> bool {
        self.flags & netlink::IFF_LOOPBACK != 0
    }

    /// Up, not loopback, and a LAN by its role.
    pub fn lan_side(&self) -> bool {
        self.up() && !self.loopback() && self.role == Role::Lan
    }

    /// The role, as a view shows it: none for the loopback.
    fn view_role(&self) -> Option<Role> {
        (!self.loopback()).then_some(self.role)
    }

    /// "lte", "lte, by its name", "tunnel", "lan".
    fn role_words(&self) -> String {
        if self.loopback() {
            return "loopback".into();
        }
        let role = match self.role {
            Role::Lan => "lan",
            Role::Lte => "lte",
            Role::Tunnel => "tunnel",
        };
        if self.role_by == RoleBy::Name {
            format!("{role}, by its name")
        } else {
            role.to_string()
        }
    }

    /// The address the kernel would send from: one in `dst`'s network,
    /// primary first, else the first.
    fn source_for(&self, dst: Ipv4Addr) -> Option<Ipv4Addr> {
        self.ipv4
            .iter()
            .find(|(addr, prefix)| contains(*addr, *prefix, dst))
            .or_else(|| self.ipv4.first())
            .map(|(addr, _)| *addr)
    }
}

/// Whether `ip` lies in `net/prefix`.
fn contains(net: Ipv4Addr, prefix: u8, ip: Ipv4Addr) -> bool {
    if prefix == 0 {
        return true;
    }
    let mask = u32::MAX << (32 - u32::from(prefix.min(32)));
    u32::from(net) & mask == u32::from(ip) & mask
}

/// Each interface's role: the configured LTE bearer, else (with no
/// configured bearer the unit has) a bearer's name, else the flags.
fn role_of(link: &Link, lte: &Lte, configured_present: bool) -> (Role, RoleBy) {
    if configured_present && lte.configured.as_deref() == Some(link.name.as_str()) {
        return (Role::Lte, RoleBy::Config);
    }
    if !configured_present && BEARER_PREFIXES.iter().any(|p| link.name.starts_with(p)) {
        return (Role::Lte, RoleBy::Name);
    }
    if link.flags & (netlink::IFF_NOARP | netlink::IFF_POINTOPOINT) != 0 {
        return (Role::Tunnel, RoleBy::Flags);
    }
    (Role::Lan, RoleBy::Flags)
}

/// The unit's interfaces and main-table routes, read over netlink.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Network {
    pub ifaces: Vec<Iface>,
    /// The main table's routes.
    routes: Vec<Route>,
}

impl Network {
    /// Build from the three dumps. Every address is kept, secondary ones
    /// included; only the main table's routes are.
    pub(crate) fn from_dump(links: &[Link], addrs: &[Addr], routes: &[Route], lte: &Lte) -> Self {
        let configured_present = lte
            .configured
            .as_deref()
            .is_some_and(|name| links.iter().any(|l| l.name == name));
        let ifaces = links
            .iter()
            .map(|link| {
                let (role, role_by) = role_of(link, lte, configured_present);
                let mut ipv4: Vec<(Ipv4Addr, u8, bool)> = addrs
                    .iter()
                    .filter(|a| a.index == link.index)
                    .map(|a| (a.local, a.prefix, a.secondary))
                    .collect();
                // Primary first; the dump's order otherwise.
                ipv4.sort_by_key(|(_, _, secondary)| *secondary);
                Iface {
                    index: link.index,
                    name: link.name.clone(),
                    flags: link.flags,
                    kind: link.kind,
                    mac: link.address,
                    port_of: link
                        .master
                        .and_then(|m| links.iter().find(|l| l.index == m).map(|l| l.name.clone())),
                    enslaved: link.master.is_some(),
                    role,
                    role_by,
                    ipv4: ipv4.into_iter().map(|(a, p, _)| (a, p)).collect(),
                }
            })
            .collect();
        let routes = routes
            .iter()
            .filter(|r| r.table == u32::from(netlink::RT_TABLE_MAIN))
            .cloned()
            .collect();
        Self { ifaces, routes }
    }

    /// Read the links, every IPv4 address and the main table now.
    pub async fn read(lte: &Lte) -> io::Result<Self> {
        let mut socket = NlSocket::open(0)?;
        let mut buf = vec![0; netlink::RECV_BUF];
        let [links, addrs, routes] = netlink::dump_requests();
        let order = Order::NATIVE;
        let links: Vec<Link> = socket
            .dump(links.0, links.1, &links.2, &mut buf)
            .await?
            .iter()
            .filter_map(|p| netlink::parse_link(p, order))
            .collect();
        let addrs: Vec<Addr> = socket
            .dump(addrs.0, addrs.1, &addrs.2, &mut buf)
            .await?
            .iter()
            .filter_map(|p| netlink::parse_addr(p, order))
            .collect();
        let routes: Vec<Route> = socket
            .dump(routes.0, routes.1, &routes.2, &mut buf)
            .await?
            .iter()
            .filter_map(|p| netlink::parse_route(p, order))
            .collect();
        Ok(Self::from_dump(&links, &addrs, &routes, lte))
    }

    pub fn iface(&self, name: &str) -> Option<&Iface> {
        self.ifaces.iter().find(|i| i.name == name)
    }

    fn iface_at(&self, index: u32) -> Option<&Iface> {
        self.ifaces.iter().find(|i| i.index == index)
    }

    /// The interface holding `ip` as one of its addresses.
    fn holder_of(&self, ip: Ipv4Addr) -> Option<&Iface> {
        self.ifaces
            .iter()
            .find(|i| i.ipv4.iter().any(|(addr, _)| *addr == ip))
    }

    /// The LAN side as a refusal lists it, by name: every LAN-side
    /// interface but a bridge's ports, which its bridge stands for.
    pub fn lan_views(&self) -> Vec<LanView> {
        let mut views: Vec<LanView> = self
            .ifaces
            .iter()
            .filter(|i| i.lan_side() && !i.enslaved)
            .map(|i| LanView {
                dev: i.name.clone(),
                ipv4: i.ipv4.iter().map(|(a, p)| format!("{a}/{p}")).collect(),
            })
            .collect();
        views.sort_by(|a, b| a.dev.cmp(&b.dev));
        views
    }

    /// "LAN side: br-lan 192.0.2.1/24; eth1 holds no address."
    fn lan_words(&self) -> String {
        let parts: Vec<String> = self
            .lan_views()
            .into_iter()
            .map(|v| {
                if v.ipv4.is_empty() {
                    format!("{} holds no address", v.dev)
                } else {
                    format!("{} {}", v.dev, v.ipv4.join(", "))
                }
            })
            .collect();
        if parts.is_empty() {
            "LAN side: none.".to_string()
        } else {
            format!("LAN side: {}.", parts.join("; "))
        }
    }

    /// The main-table route a socket bound to `iface` would take to `dst`:
    /// the longest prefix through it, the lowest metric among equals.
    fn route_through(&self, iface: &Iface, dst: Ipv4Addr) -> Option<&Route> {
        self.routes
            .iter()
            .filter(|r| {
                r.oif == Some(iface.index)
                    && r.kind == netlink::RTN_UNICAST
                    && r.dst
                        .map_or(r.dst_len == 0, |net| contains(net, r.dst_len, dst))
            })
            .max_by_key(|r| (r.dst_len, Reverse(r.priority)))
    }
}

// ─── judging ────────────────────────────────────────────────────────────

/// The kernel's answer to a route lookup, or why it had none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    Route(KernelRoute),
    /// No route: `ENETUNREACH`, or an unreachable, prohibit or blackhole
    /// route the kernel answers with an error.
    NoRoute,
}

/// Where the guard lets a request go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Approved {
    /// The address to send to, the only one.
    pub address: Ipv4Addr,
    /// The interface the socket is bound to.
    pub dev: String,
    /// The source address the socket is bound to.
    pub src: Ipv4Addr,
    /// The destination is the unit's own: nothing leaves it.
    pub own: bool,
    /// What the guard read while it approved: a refusal after the approval,
    /// a socket that cannot be bound, shows it.
    pub seen: Seen,
}

/// Why a bound TCP connection did not open.
#[derive(Debug)]
pub enum Connect {
    /// The socket could not be bound: nothing was sent.
    Bind(io::Error),
    /// The connection itself failed.
    Connect(io::Error),
}

impl Approved {
    /// A TCP connection to `port`, bound to the approved interface and
    /// source address before the first packet.
    pub async fn connect_tcp(&self, port: u16) -> Result<TcpStream, Connect> {
        let socket = TcpSocket::new_v4().map_err(Connect::Bind)?;
        source::bind_to_device(&socket, &self.dev).map_err(Connect::Bind)?;
        socket
            .bind(SocketAddr::V4(SocketAddrV4::new(self.src, 0)))
            .map_err(Connect::Bind)?;
        socket
            .connect(SocketAddr::V4(SocketAddrV4::new(self.address, port)))
            .await
            .map_err(Connect::Connect)
    }

    /// A UDP socket bound to the approved interface and source address and
    /// connected to `port`, before anything is sent.
    pub async fn udp(&self, port: u16) -> io::Result<UdpSocket> {
        let socket = UdpSocket::bind(SocketAddrV4::new(self.src, 0)).await?;
        source::bind_to_device(&socket, &self.dev)?;
        socket
            .connect(SocketAddrV4::new(self.address, port))
            .await?;
        Ok(socket)
    }

    /// A refusal of this request after the guard approved it: it shows what
    /// the guard read.
    pub fn refusal(&self, code: Code, reason: String) -> Box<Refusal> {
        Box::new(Refusal::new(code, reason, self.address.to_string()).with_seen(&self.seen))
    }

    /// A refusal for a socket that could not be bound.
    pub fn bind_refusal(&self, error: &io::Error) -> Box<Refusal> {
        self.refusal(
            Code::GuardFailed,
            format!("cannot bind to {} from {}: {error}", self.dev, self.src),
        )
    }
}

/// The view of the kernel's route a refusal shows.
fn route_view(net: &Network, answer: &Answer) -> Option<RouteView> {
    let Answer::Route(route) = answer else {
        return None;
    };
    let iface = net.iface_at(route.oif);
    Some(RouteView {
        dev: route.dev.clone(),
        role: iface.and_then(Iface::view_role),
        role_by: iface.filter(|i| !i.loopback()).map(|i| i.role_by),
        gateway: route.via,
        kind: route.kind.as_str(),
    })
}

/// Judge one destination: where it may go, or why not. Pure: the network
/// and the kernel's answer are read before, the ARP request comes after.
pub fn judge(
    net: &Network,
    address: Ipv4Addr,
    answer: &Answer,
    dev: Option<&str>,
    expect_mac: bool,
) -> Result<Approved, Box<Refusal>> {
    let seen = Seen {
        dev: dev.map(str::to_string),
        route: route_view(net, answer),
        lan: Some(net.lan_views()),
    };
    let refuse = |code: Code, reason: String| {
        Box::new(Refusal::new(code, reason, address.to_string()).with_seen(&seen))
    };
    let kind = match answer {
        Answer::Route(route) => Some(route.kind),
        Answer::NoRoute => None,
    };
    if address.is_broadcast()
        || address.is_multicast()
        || matches!(
            kind,
            Some(RouteKind::Broadcast | RouteKind::Multicast | RouteKind::Anycast)
        )
    {
        let what = if address.is_multicast() || kind == Some(RouteKind::Multicast) {
            "multicast"
        } else if kind == Some(RouteKind::Anycast) {
            "anycast"
        } else {
            "broadcast"
        };
        return Err(refuse(
            Code::NotUnicast,
            format!("{address} is a {what} address: the tools speak to one device."),
        ));
    }
    let own = kind == Some(RouteKind::Local);
    if own && expect_mac {
        return Err(refuse(
            Code::OwnAddress,
            format!(
                "{address} is this unit's own address: no device answers for it to prove a MAC."
            ),
        ));
    }
    if let Some(name) = dev {
        let (dev, src) = judge_dev(net, address, name, refuse)?;
        return Ok(Approved {
            address,
            dev,
            src,
            own,
            seen,
        });
    }
    let lan = net.lan_words();
    let not_on_lan = |how: String| {
        refuse(
            Code::NotOnLan,
            format!("{address} is not on a network this unit holds on its LAN side: {how}. {lan}"),
        )
    };
    let route = match answer {
        Answer::NoRoute => return Err(not_on_lan("the kernel has no route to it".into())),
        Answer::Route(route) => route,
    };
    match route.kind {
        RouteKind::Local => {
            // Bound to the interface that holds the address, so the
            // kernel's local route still matches; else (127/8) the route's.
            let (dev, src) = net.holder_of(address).map_or_else(
                || (route.dev.clone(), route.src.unwrap_or(Ipv4Addr::LOCALHOST)),
                |holder| (holder.name.clone(), address),
            );
            return Ok(Approved {
                address,
                dev,
                src,
                own: true,
                seen,
            });
        }
        RouteKind::Unicast => {}
        other => {
            return Err(not_on_lan(format!(
                "the kernel would drop it (a {} route)",
                other.as_str()
            )))
        }
    }
    let iface = net.iface_at(route.oif);
    let words = iface.map_or_else(|| "unknown".to_string(), Iface::role_words);
    if route.via.is_some() || !iface.is_some_and(Iface::lan_side) {
        let gateway = if route.via.is_some() {
            " through a gateway"
        } else {
            ""
        };
        return Err(not_on_lan(format!(
            "the kernel would send it by {} ({words}){gateway}",
            route.dev
        )));
    }
    let Some(iface) = iface.filter(|i| !i.ipv4.is_empty()) else {
        return Err(refuse(
            Code::NoAddress,
            format!(
                "{address} would leave by {}, which holds no IPv4 address. {lan}",
                route.dev
            ),
        ));
    };
    let Some(src) = route.src.or_else(|| iface.source_for(address)) else {
        return Err(refuse(
            Code::NoAddress,
            format!("{} holds no IPv4 address. {lan}", route.dev),
        ));
    };
    Ok(Approved {
        address,
        dev: iface.name.clone(),
        src,
        own: false,
        seen,
    })
}

/// `--dev`: the named interface, judged from the routes. The interface and
/// the source address to bind to.
fn judge_dev(
    net: &Network,
    address: Ipv4Addr,
    name: &str,
    refuse: impl Fn(Code, String) -> Box<Refusal>,
) -> Result<(String, Ipv4Addr), Box<Refusal>> {
    let lan = net.lan_words();
    let Some(iface) = net.iface(name) else {
        return Err(refuse(
            Code::NotOnLan,
            format!("this unit has no interface {name}. {lan}"),
        ));
    };
    if let Some(bridge) = &iface.port_of {
        return Err(refuse(
            Code::NoAddress,
            format!(
                "{name} is a port of {bridge}, which holds its addresses: name {bridge}. {lan}"
            ),
        ));
    }
    if iface.loopback() {
        return Err(refuse(
            Code::NotOnLan,
            format!("{name} is the loopback, not the LAN side. {lan}"),
        ));
    }
    if !iface.up() {
        return Err(refuse(Code::NotOnLan, format!("{name} is down. {lan}")));
    }
    if iface.role != Role::Lan {
        return Err(refuse(
            Code::NotOnLan,
            format!(
                "{name} is not on the LAN side: it is {}. {lan}",
                iface.role_words()
            ),
        ));
    }
    if iface.ipv4.is_empty() {
        return Err(refuse(
            Code::NoAddress,
            format!("{name} holds no IPv4 address. {lan}"),
        ));
    }
    let Some(route) = net.route_through(iface, address) else {
        return Err(refuse(
            Code::NotOnLan,
            format!("{address} lies under no route through {name}. {lan}"),
        ));
    };
    if route.gateway.is_some() {
        return Err(refuse(
            Code::NotOnLan,
            format!("the route through {name} to {address} goes through a gateway. {lan}"),
        ));
    }
    let src = if iface.ipv4.iter().any(|(a, _)| *a == address) {
        address
    } else {
        match route
            .prefsrc
            .filter(|s| net.holder_of(*s).is_some())
            .or_else(|| iface.source_for(address))
        {
            Some(src) => src,
            None => {
                return Err(refuse(
                    Code::NoAddress,
                    format!("{name} holds no IPv4 address. {lan}"),
                ))
            }
        }
    };
    Ok((name.to_string(), src))
}

// ─── the ARP request ────────────────────────────────────────────────────

const ARP_LEN: usize = 28;
const ARPOP_REQUEST: u16 = 1;
const ARPOP_REPLY: u16 = 2;

/// An ARP request (RFC 826) for `target` from `sha`/`spa`, Ethernet and
/// IPv4.
fn arp_request(sha: [u8; 6], spa: Ipv4Addr, target: Ipv4Addr) -> [u8; ARP_LEN] {
    let mut packet = [0u8; ARP_LEN];
    packet[0..2].copy_from_slice(&netlink::ARPHRD_ETHER.to_be_bytes());
    packet[2..4].copy_from_slice(&0x0800u16.to_be_bytes());
    packet[4] = 6;
    packet[5] = 4;
    packet[6..8].copy_from_slice(&ARPOP_REQUEST.to_be_bytes());
    packet[8..14].copy_from_slice(&sha);
    packet[14..18].copy_from_slice(&spa.octets());
    // tha stays zero.
    packet[24..28].copy_from_slice(&target.octets());
    packet
}

/// The MAC an ARP reply says holds `target`, if `packet` is one.
fn arp_reply_for(packet: &[u8], target: Ipv4Addr) -> Option<[u8; 6]> {
    if packet.len() < ARP_LEN
        || packet[0..2] != netlink::ARPHRD_ETHER.to_be_bytes()
        || packet[2..4] != 0x0800u16.to_be_bytes()
        || packet[4] != 6
        || packet[5] != 4
        || packet[6..8] != ARPOP_REPLY.to_be_bytes()
        || packet[14..18] != target.octets()
    {
        return None;
    }
    packet[8..14].try_into().ok()
}

/// How long the ARP request waits, within a request's timeout.
pub fn arp_wait(timeout: Duration) -> Duration {
    (timeout / 3).clamp(ARP_WAIT_MIN, ARP_WAIT_MAX).min(timeout)
}

/// Judge the answers to the ARP request and the kernel's neighbour entry.
fn judge_macs(
    expected: Mac,
    address: Ipv4Addr,
    dev: &str,
    waited: Duration,
    answers: &[[u8; 6]],
    neighbour: Option<([u8; 6], &str)>,
) -> Result<(), (Code, String)> {
    match answers {
        [] => Err((
            Code::NoAnswer,
            format!(
                "nothing answered an ARP request for {address} on {dev} within {} ms.",
                waited.as_millis()
            ),
        )),
        [one] if Mac(*one) != expected => Err((
            Code::MacMismatch,
            format!("{address} answered with {}, not {expected}.", Mac(*one)),
        )),
        [_] => match neighbour {
            Some((entry, state)) if Mac(entry) != expected => Err((
                Code::MacMismatch,
                format!(
                    "the kernel's neighbour entry for {address} names {} ({state}), not {expected}: it would send there.",
                    Mac(entry)
                ),
            )),
            _ => Ok(()),
        },
        many => Err((
            Code::MacMismatch,
            format!(
                "{} MACs answered for {address}: {}.",
                many.len(),
                many.iter()
                    .map(|m| Mac(*m).to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )),
    }
}

/// `sockaddr_ll` for ARP on interface `index`, to `dest` when sending.
fn link_address(index: u32, dest: Option<[u8; 6]>) -> io::Result<libc::sockaddr_ll> {
    // SAFETY: sockaddr_ll is plain data; all zero is a valid value.
    let mut addr: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
    addr.sll_family = libc::AF_PACKET as libc::c_ushort;
    addr.sll_protocol = (libc::ETH_P_ARP as u16).to_be();
    addr.sll_ifindex = i32::try_from(index)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "interface index too large"))?;
    if let Some(dest) = dest {
        addr.sll_halen = 6;
        addr.sll_addr[..6].copy_from_slice(&dest);
    }
    Ok(addr)
}

/// A packet socket for ARP on interface `index`.
fn arp_socket(index: u32) -> io::Result<OwnedFd> {
    let protocol = libc::c_int::from((libc::ETH_P_ARP as u16).to_be());
    // SAFETY: socket(2) with plain integer arguments; the result is checked.
    let raw = unsafe {
        libc::socket(
            libc::AF_PACKET,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            protocol,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: raw is a descriptor this call just created and nothing else owns.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let addr = link_address(index, None)?;
    // SAFETY: addr is a sockaddr_ll and its exact size is passed with it.
    let rc = unsafe {
        libc::bind(
            fd.as_raw_fd(),
            (&raw const addr).cast(),
            std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// Read one ARP packet without blocking: its length and its packet type
/// (`PACKET_OUTGOING` for one this host sent).
fn recv_arp(fd: RawFd, buf: &mut [u8]) -> io::Result<(usize, u8)> {
    // SAFETY: sockaddr_ll is plain data; all zero is a valid value.
    let mut from: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t;
    // SAFETY: buf and from are live and their sizes go with them.
    let n = unsafe {
        libc::recvfrom(
            fd,
            buf.as_mut_ptr().cast(),
            buf.len(),
            libc::MSG_DONTWAIT,
            (&raw mut from).cast(),
            &raw mut len,
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((n.unsigned_abs(), from.sll_pkttype))
}

/// Send one ARP request for `target` on `iface` and gather the MACs that
/// answer within `wait`: after the first, a second is waited for at most
/// [`ARP_SETTLE`] more.
async fn ask_wire(
    iface: &Iface,
    mac: [u8; 6],
    src: Ipv4Addr,
    target: Ipv4Addr,
    wait: Duration,
) -> io::Result<Vec<[u8; 6]>> {
    let fd = AsyncFd::new(arp_socket(iface.index)?)?;
    let to = link_address(iface.index, Some([0xff; 6]))?;
    let packet = arp_request(mac, src, target);
    // SAFETY: packet and to are live and their sizes go with them.
    let sent = unsafe {
        libc::sendto(
            fd.as_raw_fd(),
            packet.as_ptr().cast(),
            packet.len(),
            0,
            (&raw const to).cast(),
            std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t,
        )
    };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    let deadline = Instant::now() + wait;
    let mut settle_by: Option<Instant> = None;
    let mut answers: Vec<[u8; 6]> = Vec::new();
    let mut buf = [0u8; 256];
    loop {
        let until = settle_by.map_or(deadline, |s| s.min(deadline));
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let Ok(ready) = tokio::time::timeout(left, fd.readable()).await else {
            break;
        };
        let mut ready = ready?;
        match ready.try_io(|fd| recv_arp(fd.as_raw_fd(), &mut buf)) {
            Ok(Ok((n, kind))) => {
                if kind == libc::PACKET_OUTGOING {
                    continue;
                }
                if let Some(sha) = arp_reply_for(&buf[..n], target) {
                    if !answers.contains(&sha) {
                        answers.push(sha);
                    }
                    settle_by.get_or_insert_with(|| Instant::now() + ARP_SETTLE);
                }
            }
            Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => {}
            Ok(Err(e)) => return Err(e),
            Err(_would_block) => {}
        }
    }
    Ok(answers)
}

/// The MAC the kernel's neighbour entry for `address` on interface `index`
/// names, with the entry's state, if it holds a usable one.
async fn neighbour_of(
    index: u32,
    address: Ipv4Addr,
) -> io::Result<Option<([u8; 6], &'static str)>> {
    let mut socket = NlSocket::open(0)?;
    let mut buf = vec![0; netlink::RECV_BUF];
    let (request, reply, family) = netlink::neighbour_dump_request();
    let payloads = socket.dump(request, reply, &family, &mut buf).await?;
    Ok(payloads
        .iter()
        .filter_map(|p| netlink::parse_neighbour(p, Order::NATIVE))
        .filter(|n| n.index == index && n.dst == address)
        .find_map(|n| n.usable_lladdr().map(|mac| (mac, n.state_word()))))
}

// ─── the guard ──────────────────────────────────────────────────────────

/// The agent's own LTE bearer, for the Infra checks it runs in process.
static AGENT_LTE: OnceLock<Lte> = OnceLock::new();

/// Called once by the agent at start: the Infra checks' guard reads the
/// LTE bearer its config names.
pub fn set_agent_lte(lte: Lte) {
    let _ = AGENT_LTE.set(lte);
}

/// One request's guard: what names the LTE bearer, and the request's
/// `--dev` and `--expect-mac`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Guard {
    pub lte: Lte,
    pub dev: Option<String>,
    pub expect_mac: Option<Mac>,
    /// How long the ARP request waits ([`arp_wait`]).
    pub arp_wait: Duration,
}

impl Guard {
    /// The guard of a check the agent runs itself: its own LTE bearer, no
    /// `--dev`, no MAC.
    pub fn for_agent() -> Self {
        Self {
            lte: AGENT_LTE.get().cloned().unwrap_or_default(),
            ..Self::default()
        }
    }

    /// Look `host` up once: an IPv4 literal, or a name's first IPv4
    /// address. An IPv6 destination is refused with `--dev` as given and
    /// the LAN side, read now; it has no route, since the kernel is asked
    /// only about IPv4 ones.
    pub async fn resolve(&self, host: &str) -> Result<(Ipv4Addr, Option<String>), Stop> {
        match resolve(host).await {
            Err(Stop::Refused(refusal)) => {
                let seen = Seen {
                    dev: self.dev.clone(),
                    route: None,
                    lan: Network::read(&self.lte)
                        .await
                        .ok()
                        .map(|net| net.lan_views()),
                };
                Err(Stop::Refused(Box::new((*refusal).with_seen(&seen))))
            }
            other => other,
        }
    }

    /// Resolve `host` once and approve its first IPv4 address.
    pub async fn approve_host(&self, host: &str) -> Result<Approved, Stop> {
        let (address, name) = self.resolve(host).await?;
        self.approve_resolved(address, name).await
    }

    /// Approve `address`, the answer a lookup of `name` gave when the host
    /// was a name: a refusal names it. The address judged is the one the
    /// caller looked up, never a second lookup's.
    pub async fn approve_resolved(
        &self,
        address: Ipv4Addr,
        name: Option<String>,
    ) -> Result<Approved, Stop> {
        self.approve(address).await.map_err(|mut refusal| {
            refusal.name = name;
            Stop::Refused(refusal)
        })
    }

    /// Judge `address` against the unit's routes now and, with
    /// `--expect-mac`, ask the wire who holds it.
    pub async fn approve(&self, address: Ipv4Addr) -> Result<Approved, Box<Refusal>> {
        // What the guard could not read is left out: the LAN side when the
        // network could not be read, the route when it was not answered.
        let failed = |what: &str, e: &io::Error, lan: Option<Vec<LanView>>| {
            let seen = Seen {
                dev: self.dev.clone(),
                route: None,
                lan,
            };
            Box::new(
                Refusal::new(
                    Code::GuardFailed,
                    format!("cannot {what}: {e}"),
                    address.to_string(),
                )
                .with_seen(&seen),
            )
        };
        let net = Network::read(&self.lte)
            .await
            .map_err(|e| failed("read this unit's network", &e, None))?;
        let answer = match source::route_get(address).await {
            Ok(route) => Answer::Route(route),
            Err(e) if is_no_route(&e) => Answer::NoRoute,
            Err(e) => {
                return Err(failed(
                    "ask the kernel for the route",
                    &e,
                    Some(net.lan_views()),
                ))
            }
        };
        let approved = judge(
            &net,
            address,
            &answer,
            self.dev.as_deref(),
            self.expect_mac.is_some(),
        )?;
        let Some(expected) = self.expect_mac else {
            return Ok(approved);
        };
        let refuse = |code: Code, reason: String, macs: Option<Vec<Mac>>, entry: Option<Mac>| {
            let mut refusal = approved.refusal(code, reason);
            refusal.macs = macs;
            refusal.neighbour = entry;
            refusal
        };
        let Some((iface, mac)) = net
            .iface(&approved.dev)
            .filter(|i| i.kind == netlink::ARPHRD_ETHER)
            .and_then(|i| i.mac.map(|mac| (i, mac)))
        else {
            return Err(refuse(
                Code::GuardFailed,
                format!(
                    "cannot ask the wire on {}: it is not an Ethernet link",
                    approved.dev
                ),
                None,
                None,
            ));
        };
        let wait = if self.arp_wait.is_zero() {
            ARP_WAIT_MAX
        } else {
            self.arp_wait
        };
        let answers = ask_wire(iface, mac, approved.src, address, wait)
            .await
            .map_err(|e| {
                refuse(
                    Code::GuardFailed,
                    format!("cannot ask the wire on {}: {e}", approved.dev),
                    None,
                    None,
                )
            })?;
        let macs = Some(answers.iter().copied().map(Mac).collect::<Vec<_>>());
        let entry = neighbour_of(iface.index, address).await.map_err(|e| {
            refuse(
                Code::GuardFailed,
                format!("cannot read the kernel's neighbour table: {e}"),
                macs.clone(),
                None,
            )
        })?;
        judge_macs(expected, address, &approved.dev, wait, &answers, entry).map_err(
            |(code, reason)| refuse(code, reason, macs.clone(), entry.map(|(mac, _)| Mac(mac))),
        )?;
        Ok(approved)
    }
}

/// Errors a route lookup answers with when no unicast route leads there:
/// none (`ENETUNREACH`), unreachable (`EHOSTUNREACH`), prohibit (`EACCES`),
/// blackhole (`EINVAL`).
fn is_no_route(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::ENETUNREACH | libc::EHOSTUNREACH | libc::EACCES | libc::EINVAL)
    )
}

/// The address to judge for `host`: an IPv4 literal, or a name's first IPv4
/// address, looked up once; with the name when it was one.
async fn resolve(host: &str) -> Result<(Ipv4Addr, Option<String>), Stop> {
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    match bare.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => return Ok((ip, None)),
        Ok(IpAddr::V6(ip)) => {
            return Err(Stop::Refused(Box::new(Refusal::new(
                Code::Ipv6,
                format!("{ip} is an IPv6 address: the tools send to IPv4 destinations only."),
                ip.to_string(),
            ))))
        }
        Err(_) => {}
    }
    #[cfg(test)]
    if let Some(ip) = steer::take(host) {
        return Ok((ip, Some(host.to_string())));
    }
    let found: Vec<SocketAddr> = tokio::net::lookup_host((host, 0))
        .await
        .map_err(|e| Stop::Failed(format!("cannot resolve {host}: {e}")))?
        .collect();
    if let Some(ip) = found.iter().find_map(|a| match a.ip() {
        IpAddr::V4(ip) => Some(ip),
        IpAddr::V6(_) => None,
    }) {
        return Ok((ip, Some(host.to_string())));
    }
    match found.first() {
        Some(v6) => {
            let mut refusal = Refusal::new(
                Code::Ipv6,
                format!(
                    "{host} has only IPv6 addresses: the tools send to IPv4 destinations only."
                ),
                v6.ip().to_string(),
            );
            refusal.name = Some(host.to_string());
            Err(Stop::Refused(Box::new(refusal)))
        }
        None => Err(Stop::Failed(format!("cannot resolve {host}: no address"))),
    }
}

/// Answers a test gives names on its own thread, one per lookup, in order:
/// a test can count a name's lookups and change its answer between two.
#[cfg(test)]
pub(crate) mod steer {
    use std::cell::RefCell;
    use std::net::Ipv4Addr;

    thread_local! {
        static ANSWERS: RefCell<Vec<(String, Ipv4Addr)>> = const { RefCell::new(Vec::new()) };
    }

    /// Answer `name` with `answers`, one per lookup, on this thread.
    pub(crate) fn set(name: &str, answers: &[Ipv4Addr]) {
        ANSWERS.with(|all| {
            all.borrow_mut()
                .extend(answers.iter().map(|ip| (name.to_string(), *ip)));
        });
    }

    /// The next answer for `name`, taken by a lookup.
    pub(super) fn take(name: &str) -> Option<Ipv4Addr> {
        ANSWERS.with(|all| {
            let mut all = all.borrow_mut();
            let at = all.iter().position(|(n, _)| n == name)?;
            Some(all.remove(at).1)
        })
    }

    /// How many answers for `name` no lookup has taken.
    pub(crate) fn left(name: &str) -> usize {
        ANSWERS.with(|all| all.borrow().iter().filter(|(n, _)| n == name).count())
    }

    /// Forget every answer not taken.
    pub(crate) fn clear() {
        ANSWERS.with(|all| all.borrow_mut().clear());
    }
}

// ─── the agent's config, for a tool ─────────────────────────────────────

/// The config a tool reads: `--config`, else `SCTL_AGENT_CONFIG` (`env`),
/// else none. Never `./sctl.toml`: a tool runs in `/tmp` on the units.
pub fn config_path(flag: Option<&str>, env: Option<OsString>) -> Option<PathBuf> {
    flag.map(PathBuf::from)
        .or_else(|| env.filter(|v| !v.is_empty()).map(PathBuf::from))
}

/// Read the config at `path` for a tool, or the refusal that it could not
/// be read or parsed: it names the path and quotes nothing from the file.
pub fn read_config(path: &Path, host: &str) -> Result<Config, Box<Refusal>> {
    let refuse = |why: String| {
        Box::new(Refusal::new(
            Code::ConfigUnreadable,
            format!("the agent's config {} {why}", path.display()),
            host,
        ))
    };
    let mut text = String::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(MAX_CONFIG_BYTES + 1).read_to_string(&mut text))
        .map_err(|e| refuse(format!("cannot be read ({}).", e.kind())))?;
    if text.len() as u64 > MAX_CONFIG_BYTES {
        return Err(refuse("is larger than any config the agent reads.".into()));
    }
    Config::from_toml(&text).ok_or_else(|| refuse("is not a config the agent reads.".into()))
}

/// The tool's guard: the LTE bearer from the config it reads, with its
/// `--dev`, `--expect-mac` and timeout. A config it cannot read refuses.
pub fn for_tool(
    config_flag: Option<&str>,
    dev: Option<String>,
    expect_mac: Option<Mac>,
    timeout: Duration,
    host: &str,
) -> Result<(Guard, Option<Config>), Box<Refusal>> {
    let config = match config_path(config_flag, std::env::var_os(AGENT_CONFIG_ENV)) {
        Some(path) => Some(read_config(&path, host)?),
        None => None,
    };
    let guard = Guard {
        lte: Lte::from_config(config.as_ref()),
        dev,
        expect_mac,
        arp_wait: arp_wait(timeout),
    };
    Ok((guard, config))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::netwatch::netlink::{IFF_LOOPBACK, IFF_NOARP, IFF_POINTOPOINT, IFF_UP};

    const ETHER_UP: u32 = IFF_UP | 0x1000 | 0x40; // UP, MULTICAST, RUNNING
    const MAC_A: [u8; 6] = [0x02, 0, 0, 0, 0, 0x0a];
    const MAC_B: [u8; 6] = [0x02, 0, 0, 0, 0, 0x0b];

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

    fn addr(index: u32, local: [u8; 4], prefix: u8, secondary: bool) -> Addr {
        Addr {
            index,
            prefix,
            secondary,
            local: Ipv4Addr::from(local),
        }
    }

    fn route(dst: Option<([u8; 4], u8)>, via: Option<[u8; 4]>, oif: u32) -> Route {
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
            priority: 0,
            prefsrc: None,
        }
    }

    /// The kernel's answer for a route through `dev` (index `oif`).
    fn kernel(dev: &str, oif: u32, via: Option<[u8; 4]>, src: [u8; 4], kind: RouteKind) -> Answer {
        Answer::Route(KernelRoute {
            dev: dev.into(),
            oif,
            via: via.map(Ipv4Addr::from),
            src: Some(Ipv4Addr::from(src)),
            kind,
        })
    }

    /// A probe unit on a site: br-lan (port eth0) holding 192.0.2.1/24, a
    /// wired WAN port eth1 that lost its lease, the LTE bearer wwan0
    /// (framed like Ethernet, as on the XE300) with the default route, and
    /// the mesh wg0.
    fn site(lte: &Lte) -> Network {
        let mut eth0 = link(2, "eth0", ETHER_UP);
        eth0.master = Some(4);
        let links = vec![
            link(1, "lo", IFF_UP | IFF_LOOPBACK | 0x40),
            eth0,
            link(3, "eth1", ETHER_UP),
            link(4, "br-lan", ETHER_UP),
            link(5, "wwan0", ETHER_UP),
            Link {
                kind: 65534, // ARPHRD_NONE
                address: None,
                ..link(6, "wg0", IFF_UP | IFF_POINTOPOINT | IFF_NOARP | 0x40)
            },
        ];
        let addrs = vec![
            addr(1, [127, 0, 0, 1], 8, false),
            addr(4, [192, 0, 2, 1], 24, false),
            addr(5, [10, 180, 41, 231], 30, false),
            addr(6, [10, 99, 0, 2], 24, false),
        ];
        let routes = vec![
            route(None, Some([10, 180, 41, 232]), 5),
            route(Some(([192, 0, 2, 0], 24)), None, 4),
            route(Some(([10, 99, 0, 0], 24)), None, 6),
        ];
        Network::from_dump(&links, &addrs, &routes, lte)
    }

    fn ip(a: [u8; 4]) -> Ipv4Addr {
        Ipv4Addr::from(a)
    }

    #[test]
    fn macs_parse_with_colons_or_hyphens_in_either_case() {
        let want = Mac([0x00, 0x1b, 0xff, 0xa0, 0x0c, 0xde]);
        for text in [
            "00:1b:ff:a0:0c:de",
            "00:1B:FF:A0:0C:DE",
            "00-1b-ff-a0-0c-de",
            "00-1B-ff-A0-0c-DE",
        ] {
            assert_eq!(Mac::parse(text), Some(want), "{text}");
        }
        for bad in [
            "",
            "00:1b:ff:a0:0c",
            "00:1b:ff:a0:0c:de:01",
            "001b.ffa0.0cde",
            "0:1b:ff:a0:0c:de",
            "00:1b:ff:a0:0c:dg",
            "00:1b:ff:a0:0c:de ",
            "+0:1b:ff:a0:0c:de",
            "001bffa00cde",
        ] {
            assert_eq!(Mac::parse(bad), None, "{bad:?}");
        }
        assert_eq!(want.to_string(), "00:1b:ff:a0:0c:de");
        assert_eq!(
            Mac::parse("00-1B-FF-A0-0C-DE"),
            Mac::parse("00:1b:ff:a0:0c:de"),
            "the same MAC whatever the writing"
        );
    }

    #[test]
    fn roles_come_from_the_config_the_bearer_names_and_the_flags() {
        let by_config = site(&Lte::named("wwan0"));
        let wwan = by_config.iface("wwan0").unwrap();
        assert_eq!((wwan.role, wwan.role_by), (Role::Lte, RoleBy::Config));
        assert!(!wwan.lan_side(), "framed like Ethernet, but the bearer");
        let wg = by_config.iface("wg0").unwrap();
        assert_eq!((wg.role, wg.role_by), (Role::Tunnel, RoleBy::Flags));
        assert!(
            by_config.iface("eth1").unwrap().lan_side(),
            "a wired WAN port"
        );
        assert!(by_config.iface("br-lan").unwrap().lan_side());
        assert!(!by_config.iface("lo").unwrap().lan_side());

        // No config, or one naming an interface the unit lacks: the name.
        for lte in [Lte::default(), Lte::named("usb0")] {
            let net = site(&lte);
            let wwan = net.iface("wwan0").unwrap();
            assert_eq!(
                (wwan.role, wwan.role_by),
                (Role::Lte, RoleBy::Name),
                "{lte:?}"
            );
        }

        // A configured bearer the unit has: names alone decide nothing.
        let links = vec![
            link(2, "usb0", ETHER_UP),
            link(3, "qmimux0", IFF_UP | IFF_NOARP | IFF_POINTOPOINT),
            link(4, "wwan0", ETHER_UP),
        ];
        let net = Network::from_dump(&links, &[], &[], &Lte::named("usb0"));
        let roles: Vec<(&str, Role, RoleBy)> = net
            .ifaces
            .iter()
            .map(|i| (i.name.as_str(), i.role, i.role_by))
            .collect();
        assert_eq!(
            roles,
            [
                ("usb0", Role::Lte, RoleBy::Config),
                ("qmimux0", Role::Tunnel, RoleBy::Flags),
                ("wwan0", Role::Lan, RoleBy::Flags),
            ]
        );
        let net = Network::from_dump(&links, &[], &[], &Lte::named("qmimux0"));
        assert_eq!(net.iface("qmimux0").unwrap().role_by, RoleBy::Config);
        assert_eq!(net.iface("usb0").unwrap().role, Role::Lan);
        // Down is not the LAN side, whatever the role.
        let net = Network::from_dump(&[link(2, "eth1", 0x1000)], &[], &[], &Lte::default());
        assert_eq!(net.iface("eth1").unwrap().role, Role::Lan);
        assert!(!net.iface("eth1").unwrap().lan_side());
    }

    #[test]
    fn on_the_link_is_sent_bound_to_the_interface_and_its_address() {
        let net = site(&Lte::named("wwan0"));
        let answer = kernel("br-lan", 4, None, [192, 0, 2, 1], RouteKind::Unicast);
        let approved = judge(&net, ip([192, 0, 2, 50]), &answer, None, false).unwrap();
        assert_eq!(
            approved,
            Approved {
                address: ip([192, 0, 2, 50]),
                dev: "br-lan".into(),
                src: ip([192, 0, 2, 1]),
                own: false,
                // Kept for a refusal after the approval: a failed bind.
                seen: Seen {
                    dev: None,
                    route: Some(RouteView {
                        dev: "br-lan".into(),
                        role: Some(Role::Lan),
                        role_by: Some(RoleBy::Flags),
                        gateway: None,
                        kind: "unicast",
                    }),
                    lan: Some(vec![
                        LanView {
                            dev: "br-lan".into(),
                            ipv4: vec!["192.0.2.1/24".into()],
                        },
                        LanView {
                            dev: "eth1".into(),
                            ipv4: vec![],
                        },
                    ]),
                },
            }
        );
    }

    /// ADR-012 decision 5: what the 2026-10-16 set would have read.
    #[test]
    fn through_a_gateway_by_the_lte_bearer_is_refused_with_the_adr_s_words() {
        let net = site(&Lte::named("wwan0"));
        let answer = kernel(
            "wwan0",
            5,
            Some([10, 180, 41, 232]),
            [10, 180, 41, 231],
            RouteKind::Unicast,
        );
        let refusal = judge(&net, ip([198, 51, 100, 50]), &answer, None, false).unwrap_err();
        assert_eq!(refusal.refused, Code::NotOnLan);
        assert_eq!(
            refusal.reason,
            "198.51.100.50 is not on a network this unit holds on its LAN side: the kernel would \
             send it by wwan0 (lte) through a gateway. LAN side: br-lan 192.0.2.1/24; eth1 holds \
             no address."
        );
        assert_eq!(refusal.host, "198.51.100.50");
        let json = refusal.to_json();
        assert_eq!(json["ok"], false);
        assert_eq!(json["refused"], "not_on_lan");
        assert_eq!(
            json["route"],
            json!({"dev": "wwan0", "role": "lte", "role_by": "config", "gateway": "10.180.41.232", "type": "unicast"})
        );
        assert_eq!(
            json["lan"],
            json!([{"dev": "br-lan", "ipv4": ["192.0.2.1/24"]}, {"dev": "eth1", "ipv4": []}])
        );
        assert!(json.get("macs").is_none() && json.get("name").is_none());

        // With no config, the bearer's name decides, and the answer says so.
        let net = site(&Lte::default());
        let refusal = judge(&net, ip([198, 51, 100, 50]), &answer, None, false).unwrap_err();
        assert!(
            refusal
                .reason
                .contains("by wwan0 (lte, by its name) through a gateway"),
            "{}",
            refusal.reason
        );
        assert_eq!(refusal.to_json()["route"]["role_by"], "name");
    }

    #[test]
    fn a_gateway_on_the_lan_side_is_refused_too() {
        let mut net = site(&Lte::named("wwan0"));
        net.ifaces[2].ipv4 = vec![(ip([192, 168, 1, 20]), 24)];
        let answer = kernel(
            "eth1",
            3,
            Some([192, 168, 1, 1]),
            [192, 168, 1, 20],
            RouteKind::Unicast,
        );
        let refusal = judge(&net, ip([203, 0, 113, 9]), &answer, None, false).unwrap_err();
        assert_eq!(refusal.refused, Code::NotOnLan);
        assert!(
            refusal.reason.contains("by eth1 (lan) through a gateway"),
            "{}",
            refusal.reason
        );
    }

    #[test]
    fn the_lte_bearer_wireguard_and_point_to_point_links_are_refused_without_a_gateway() {
        let net = site(&Lte::named("wwan0"));
        // A gatewayless route on the configured bearer.
        let answer = kernel("wwan0", 5, None, [10, 180, 41, 231], RouteKind::Unicast);
        let refusal = judge(&net, ip([10, 180, 41, 232]), &answer, None, false).unwrap_err();
        assert_eq!(refusal.refused, Code::NotOnLan);
        assert!(
            refusal.reason.contains("by wwan0 (lte)."),
            "{}",
            refusal.reason
        );
        // The mesh: no ARP, point-to-point.
        let answer = kernel("wg0", 6, None, [10, 99, 0, 2], RouteKind::Unicast);
        let refusal = judge(&net, ip([10, 99, 0, 7]), &answer, None, false).unwrap_err();
        assert_eq!(refusal.refused, Code::NotOnLan);
        assert!(
            refusal.reason.contains("by wg0 (tunnel)."),
            "{}",
            refusal.reason
        );
        assert_eq!(refusal.to_json()["route"]["role"], "tunnel");
        // A PPP link: point-to-point only.
        let links = vec![link(7, "ppp0", IFF_UP | IFF_POINTOPOINT)];
        let addrs = vec![addr(7, [10, 64, 0, 9], 32, false)];
        let net = Network::from_dump(&links, &addrs, &[], &Lte::default());
        let answer = kernel("ppp0", 7, None, [10, 64, 0, 9], RouteKind::Unicast);
        let refusal = judge(&net, ip([10, 64, 0, 1]), &answer, None, false).unwrap_err();
        assert!(refusal.reason.contains("(tunnel)"), "{}", refusal.reason);
    }

    #[test]
    fn broadcast_multicast_and_no_route_are_refused() {
        let net = site(&Lte::named("wwan0"));
        let directed = kernel("br-lan", 4, None, [192, 0, 2, 1], RouteKind::Broadcast);
        let refusal = judge(&net, ip([192, 0, 2, 255]), &directed, None, false).unwrap_err();
        assert_eq!(refusal.refused, Code::NotUnicast);
        assert!(refusal.reason.contains("broadcast"), "{}", refusal.reason);
        for (dst, word) in [
            ([255, 255, 255, 255], "broadcast"),
            ([224, 0, 0, 1], "multicast"),
        ] {
            let refusal = judge(&net, ip(dst), &Answer::NoRoute, None, false).unwrap_err();
            assert_eq!(refusal.refused, Code::NotUnicast, "{dst:?}");
            assert!(refusal.reason.contains(word), "{}", refusal.reason);
        }
        let refusal = judge(&net, ip([203, 0, 113, 1]), &Answer::NoRoute, None, false).unwrap_err();
        assert_eq!(refusal.refused, Code::NotOnLan);
        assert!(
            refusal.reason.contains("the kernel has no route to it"),
            "{}",
            refusal.reason
        );
        assert!(refusal.route.is_none());
        let blackhole = kernel("lo", 1, None, [127, 0, 0, 1], RouteKind::Other(6));
        let refusal = judge(&net, ip([203, 0, 113, 1]), &blackhole, None, false).unwrap_err();
        assert!(
            refusal.reason.contains("would drop it (a blackhole route)"),
            "{}",
            refusal.reason
        );
    }

    #[test]
    fn an_interface_without_an_address_is_no_address() {
        let mut net = site(&Lte::named("wwan0"));
        // A runbook's host route through eth1, which lost its lease.
        net.routes
            .push(route(Some(([192, 168, 1, 50], 32)), None, 3));
        let answer = Answer::Route(KernelRoute {
            dev: "eth1".into(),
            oif: 3,
            via: None,
            src: Some(ip([192, 0, 2, 1])),
            kind: RouteKind::Unicast,
        });
        let refusal = judge(&net, ip([192, 168, 1, 50]), &answer, None, false).unwrap_err();
        assert_eq!(refusal.refused, Code::NoAddress);
        assert!(
            refusal
                .reason
                .starts_with("192.168.1.50 would leave by eth1, which holds no IPv4 address."),
            "{}",
            refusal.reason
        );
    }

    #[test]
    fn the_unit_s_own_address_passes_bound_to_its_holder_but_not_with_a_mac() {
        let net = site(&Lte::named("wwan0"));
        let local = kernel("lo", 1, None, [192, 0, 2, 1], RouteKind::Local);
        let approved = judge(&net, ip([192, 0, 2, 1]), &local, None, false).unwrap();
        assert_eq!(
            (approved.dev.as_str(), approved.src, approved.own),
            ("br-lan", ip([192, 0, 2, 1]), true)
        );
        let loopback = kernel("lo", 1, None, [127, 0, 0, 1], RouteKind::Local);
        let approved = judge(&net, ip([127, 0, 0, 1]), &loopback, None, false).unwrap();
        assert_eq!((approved.dev.as_str(), approved.own), ("lo", true));
        // 127.0.0.5 is local through lo/8 but held by no one exactly.
        let approved = judge(&net, ip([127, 0, 0, 5]), &loopback, None, false).unwrap();
        assert_eq!(
            (approved.dev.as_str(), approved.src),
            ("lo", Ipv4Addr::LOCALHOST)
        );
        let refusal = judge(&net, ip([192, 0, 2, 1]), &local, None, true).unwrap_err();
        assert_eq!(refusal.refused, Code::OwnAddress);
        let refusal = judge(&net, ip([192, 0, 2, 1]), &local, Some("br-lan"), true).unwrap_err();
        assert_eq!(refusal.refused, Code::OwnAddress);
    }

    #[test]
    fn dev_passes_a_host_route_and_a_slash_32_address_from_the_routes() {
        let mut net = site(&Lte::named("wwan0"));
        // A runbook's temporary /32 on eth1 and a host route to the device,
        // as reached by hand on 2026-10-16.
        net.ifaces[2].ipv4 = vec![(ip([192, 168, 1, 250]), 32)];
        net.routes
            .push(route(Some(([192, 168, 1, 50], 32)), None, 3));
        // The kernel's own lookup is not what decides.
        let answer = kernel(
            "wwan0",
            5,
            Some([10, 180, 41, 232]),
            [10, 180, 41, 231],
            RouteKind::Unicast,
        );
        let approved = judge(&net, ip([192, 168, 1, 50]), &answer, Some("eth1"), false).unwrap();
        assert_eq!(
            (
                approved.address,
                approved.dev.as_str(),
                approved.src,
                approved.own
            ),
            (ip([192, 168, 1, 50]), "eth1", ip([192, 168, 1, 250]), false)
        );
        assert_eq!(approved.seen.dev.as_deref(), Some("eth1"));
        // An address under no route through eth1: refused, whatever the
        // kernel would answer for a lookup tied to it.
        let refusal = judge(&net, ip([192, 168, 1, 51]), &answer, Some("eth1"), false).unwrap_err();
        assert_eq!(refusal.refused, Code::NotOnLan);
        assert_eq!(refusal.dev.as_deref(), Some("eth1"));
        assert!(
            refusal
                .reason
                .starts_with("192.168.1.51 lies under no route through eth1."),
            "{}",
            refusal.reason
        );
        let refusal = judge(&net, ip([203, 0, 113, 8]), &answer, Some("eth1"), false).unwrap_err();
        assert_eq!(refusal.refused, Code::NotOnLan, "an internet address");
    }

    /// ADR-012 decision 5: a refusal after the approval, a bind that fails
    /// because the runbook's address went first, shows what the guard read:
    /// `--dev` as given, the kernel's route and the LAN side.
    #[test]
    fn a_bind_refused_after_the_approval_shows_dev_the_route_and_the_lan_side() {
        let mut net = site(&Lte::named("wwan0"));
        net.ifaces[2].ipv4 = vec![(ip([192, 168, 1, 250]), 32)];
        net.routes
            .push(route(Some(([192, 168, 1, 50], 32)), None, 3));
        let answer = kernel(
            "wwan0",
            5,
            Some([10, 180, 41, 232]),
            [10, 180, 41, 231],
            RouteKind::Unicast,
        );
        let approved = judge(&net, ip([192, 168, 1, 50]), &answer, Some("eth1"), false).unwrap();
        let refusal = approved.bind_refusal(&io::Error::from_raw_os_error(libc::EADDRNOTAVAIL));
        assert_eq!(refusal.refused, Code::GuardFailed);
        assert!(
            refusal
                .reason
                .starts_with("cannot bind to eth1 from 192.168.1.250: "),
            "{}",
            refusal.reason
        );
        let json = refusal.to_json();
        assert_eq!(json["host"], "192.168.1.50");
        assert_eq!(json["dev"], "eth1");
        assert_eq!(
            json["route"],
            json!({"dev": "wwan0", "role": "lte", "role_by": "config", "gateway": "10.180.41.232", "type": "unicast"})
        );
        assert_eq!(
            json["lan"],
            json!([{"dev": "br-lan", "ipv4": ["192.0.2.1/24"]}, {"dev": "eth1", "ipv4": ["192.168.1.250/32"]}])
        );
    }

    #[test]
    fn dev_takes_the_route_a_bound_socket_would_take() {
        let mut net = site(&Lte::named("wwan0"));
        // A longer prefix through br-lan via a gateway beats the /24.
        net.routes
            .push(route(Some(([192, 0, 2, 128], 25)), Some([192, 0, 2, 5]), 4));
        let answer = Answer::NoRoute;
        let refusal =
            judge(&net, ip([192, 0, 2, 200]), &answer, Some("br-lan"), false).unwrap_err();
        assert_eq!(refusal.refused, Code::NotOnLan);
        assert!(
            refusal.reason.contains("goes through a gateway"),
            "{}",
            refusal.reason
        );
        let approved = judge(&net, ip([192, 0, 2, 20]), &answer, Some("br-lan"), false).unwrap();
        assert_eq!(approved.src, ip([192, 0, 2, 1]));
        // A route's preferred source is the one the kernel would take.
        let mut with_src = route(Some(([198, 51, 100, 0], 24)), None, 4);
        with_src.prefsrc = Some(ip([192, 0, 2, 1]));
        net.routes.push(with_src);
        let approved = judge(&net, ip([198, 51, 100, 7]), &answer, Some("br-lan"), false).unwrap();
        assert_eq!(approved.src, ip([192, 0, 2, 1]));
    }

    #[test]
    fn dev_must_name_a_lan_side_interface_holding_an_address() {
        let net = site(&Lte::named("wwan0"));
        let answer = Answer::NoRoute;
        let refused = |dev: &str| {
            let r = judge(&net, ip([192, 0, 2, 50]), &answer, Some(dev), false).unwrap_err();
            (r.refused, r.reason)
        };
        let (code, reason) = refused("eth0");
        assert_eq!(code, Code::NoAddress);
        assert!(reason.starts_with("eth0 is a port of br-lan"), "{reason}");
        let (code, reason) = refused("wwan0");
        assert_eq!(code, Code::NotOnLan);
        assert!(
            reason.starts_with("wwan0 is not on the LAN side: it is lte."),
            "{reason}"
        );
        let (code, reason) = refused("wg0");
        assert_eq!(code, Code::NotOnLan);
        assert!(reason.contains("it is tunnel"), "{reason}");
        let (code, _) = refused("lo");
        assert_eq!(code, Code::NotOnLan);
        let (code, reason) = refused("eth9");
        assert_eq!(code, Code::NotOnLan);
        assert!(
            reason.starts_with("this unit has no interface eth9."),
            "{reason}"
        );
        let (code, reason) = refused("eth1");
        assert_eq!(code, Code::NoAddress);
        assert!(
            reason.starts_with("eth1 holds no IPv4 address."),
            "{reason}"
        );
    }

    #[test]
    fn arp_requests_and_replies_are_rfc_826() {
        let request = arp_request(MAC_A, ip([192, 0, 2, 1]), ip([192, 0, 2, 50]));
        assert_eq!(
            request.to_vec(),
            [
                vec![0, 1, 8, 0, 6, 4, 0, 1],
                MAC_A.to_vec(),
                vec![192, 0, 2, 1],
                vec![0; 6],
                vec![192, 0, 2, 50],
            ]
            .concat()
        );
        // Our own request is not an answer.
        assert_eq!(arp_reply_for(&request, ip([192, 0, 2, 50])), None);
        let mut reply = request;
        reply[6..8].copy_from_slice(&ARPOP_REPLY.to_be_bytes());
        reply[8..14].copy_from_slice(&MAC_B);
        reply[14..18].copy_from_slice(&[192, 0, 2, 50]);
        reply[18..24].copy_from_slice(&MAC_A);
        reply[24..28].copy_from_slice(&[192, 0, 2, 1]);
        assert_eq!(arp_reply_for(&reply, ip([192, 0, 2, 50])), Some(MAC_B));
        assert_eq!(
            arp_reply_for(&reply, ip([192, 0, 2, 51])),
            None,
            "another address"
        );
        assert_eq!(arp_reply_for(&reply[..27], ip([192, 0, 2, 50])), None);
        // Ethernet padding after the packet is ignored.
        let mut padded = reply.to_vec();
        padded.extend([0; 18]);
        assert_eq!(arp_reply_for(&padded, ip([192, 0, 2, 50])), Some(MAC_B));
    }

    #[test]
    fn the_wait_stays_within_the_timeout() {
        assert_eq!(arp_wait(Duration::from_secs(3)), Duration::from_secs(1));
        assert_eq!(arp_wait(Duration::from_secs(10)), Duration::from_secs(1));
        assert_eq!(
            arp_wait(Duration::from_millis(600)),
            Duration::from_millis(200)
        );
        assert_eq!(
            arp_wait(Duration::from_millis(60)),
            Duration::from_millis(60)
        );
    }

    #[test]
    fn every_answer_and_the_neighbour_entry_must_name_the_expected_mac() {
        let a = Mac(MAC_A);
        let at = ip([192, 0, 2, 50]);
        let wait = Duration::from_secs(1);
        let judged = |answers: &[[u8; 6]], entry: Option<[u8; 6]>| {
            judge_macs(a, at, "br-lan", wait, answers, entry.map(|m| (m, "stale")))
        };
        assert_eq!(judged(&[MAC_A], None), Ok(()));
        assert_eq!(judged(&[MAC_A], Some(MAC_A)), Ok(()));
        let (code, reason) = judged(&[], None).unwrap_err();
        assert_eq!(code, Code::NoAnswer);
        assert_eq!(
            reason,
            "nothing answered an ARP request for 192.0.2.50 on br-lan within 1000 ms."
        );
        let (code, reason) = judged(&[MAC_B], None).unwrap_err();
        assert_eq!(code, Code::MacMismatch);
        assert_eq!(
            reason,
            "192.0.2.50 answered with 02:00:00:00:00:0b, not 02:00:00:00:00:0a."
        );
        let (code, reason) = judged(&[MAC_A, MAC_B], None).unwrap_err();
        assert_eq!(code, Code::MacMismatch);
        assert!(
            reason.starts_with("2 MACs answered for 192.0.2.50"),
            "{reason}"
        );
        // A STALE entry still naming the device that held the address.
        let (code, reason) = judged(&[MAC_A], Some(MAC_B)).unwrap_err();
        assert_eq!(code, Code::MacMismatch);
        assert_eq!(
            reason,
            "the kernel's neighbour entry for 192.0.2.50 names 02:00:00:00:00:0b (stale), not \
             02:00:00:00:00:0a: it would send there."
        );
    }

    #[test]
    fn a_tool_reads_its_config_from_the_flag_then_the_variable_never_the_working_directory() {
        let flag = config_path(Some("/a.toml"), Some(OsString::from("/b.toml")));
        assert_eq!(flag, Some(PathBuf::from("/a.toml")), "--config wins");
        let env = config_path(None, Some(OsString::from("/b.toml")));
        assert_eq!(env, Some(PathBuf::from("/b.toml")));
        assert_eq!(config_path(None, None), None, "never ./sctl.toml");
        assert_eq!(config_path(None, Some(OsString::new())), None);
    }

    fn scratch_file(tag: &str, text: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("sctl-guard-{tag}-{}.toml", std::process::id()));
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn the_config_names_the_bearer_or_leaves_it_to_the_names() {
        let path = scratch_file("lte", "[lte]\ninterface = \"usb0\"\n");
        let config = read_config(&path, "192.0.2.50").unwrap();
        assert_eq!(Lte::from_config(Some(&config)), Lte::named("usb0"));
        let path2 = scratch_file("nolte", "[server]\nlisten = \"127.0.0.1:1\"\n");
        let config = read_config(&path2, "192.0.2.50").unwrap();
        assert_eq!(Lte::from_config(Some(&config)), Lte::default());
        // `[lte]` without an interface: the agent's default, wwan0.
        let path3 = scratch_file("ltedefault", "[lte]\npoll_interval_secs = 60\n");
        let config = read_config(&path3, "192.0.2.50").unwrap();
        assert_eq!(Lte::from_config(Some(&config)), Lte::named("wwan0"));
        for p in [path, path2, path3] {
            std::fs::remove_file(p).ok();
        }
    }

    #[test]
    fn an_unreadable_config_is_refused_naming_the_path_and_quoting_nothing() {
        // The MCP server's devices file given as the config.
        let json = scratch_file(
            "json",
            "{\"devices\": {\"bench\": {\"url\": \"http://192.0.2.9:1337\", \"api_key\": \"k3y-n0t-quoted\"}}}",
        );
        let refusal = read_config(&json, "192.0.2.50").unwrap_err();
        assert_eq!(refusal.refused, Code::ConfigUnreadable);
        assert!(refusal.reason.contains(&json.display().to_string()));
        assert!(!refusal.reason.contains("k3y"), "{}", refusal.reason);
        assert!(!refusal.reason.contains("devices"), "{}", refusal.reason);
        let broken = scratch_file("broken", "[lte\ninterface = \"s3cr3t-line\"\n");
        let refusal = read_config(&broken, "192.0.2.50").unwrap_err();
        assert_eq!(refusal.refused, Code::ConfigUnreadable);
        assert!(!refusal.reason.contains("s3cr3t"), "{}", refusal.reason);
        let missing = std::env::temp_dir().join("sctl-guard-no-such-config.toml");
        let refusal = read_config(&missing, "192.0.2.50").unwrap_err();
        assert_eq!(refusal.refused, Code::ConfigUnreadable);
        assert!(
            refusal.reason.contains("cannot be read"),
            "{}",
            refusal.reason
        );
        assert_eq!(refusal.to_json()["host"], "192.0.2.50");
        std::fs::remove_file(json).ok();
        std::fs::remove_file(broken).ok();
    }

    #[tokio::test]
    async fn ipv6_destinations_are_refused_and_ipv4_literals_need_no_lookup() {
        for host in ["::1", "[fe80::1]", "2001:db8::7"] {
            match resolve(host).await {
                Err(Stop::Refused(r)) => {
                    assert_eq!(r.refused, Code::Ipv6, "{host}");
                    assert!(r.reason.contains("IPv6"), "{}", r.reason);
                }
                other => panic!("{host}: {other:?}"),
            }
        }
        assert_eq!(resolve("192.0.2.50").await, Ok((ip([192, 0, 2, 50]), None)));
        assert_eq!(
            resolve("localhost").await.unwrap().1.as_deref(),
            Some("localhost")
        );
    }

    #[tokio::test]
    async fn loopback_is_the_unit_s_own_and_broadcast_never_leaves() {
        let guard = Guard::default();
        let approved = guard.approve(Ipv4Addr::LOCALHOST).await.unwrap();
        assert_eq!(
            (
                approved.address,
                approved.dev.as_str(),
                approved.src,
                approved.own
            ),
            (Ipv4Addr::LOCALHOST, "lo", Ipv4Addr::LOCALHOST, true)
        );
        let refusal = guard.approve(Ipv4Addr::BROADCAST).await.unwrap_err();
        assert_eq!(refusal.refused, Code::NotUnicast);
        assert!(refusal.lan.is_some(), "the LAN side was read");
        // With a MAC to prove, the unit's own address is refused before any
        // packet socket is opened.
        let guard = Guard {
            expect_mac: Some(Mac(MAC_A)),
            ..Guard::default()
        };
        let refusal = guard.approve(Ipv4Addr::LOCALHOST).await.unwrap_err();
        assert_eq!(refusal.refused, Code::OwnAddress);
    }

    #[tokio::test]
    async fn a_bound_connection_to_loopback_opens() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let approved = Guard::default().approve(Ipv4Addr::LOCALHOST).await.unwrap();
        let stream = approved.connect_tcp(port).await;
        assert!(stream.is_ok(), "{stream:?}");
        let udp = approved.udp(port).await.unwrap();
        assert_eq!(
            udp.peer_addr().unwrap(),
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
        );
        // An interface the unit lacks cannot be bound: nothing is sent, and
        // the refusal shows the route and the LAN side the guard read.
        let gone = Approved {
            dev: "sctl-none0".into(),
            ..approved
        };
        let Err(Connect::Bind(error)) = gone.connect_tcp(port).await else {
            panic!("the bind fails");
        };
        assert!(gone.udp(port).await.is_err());
        let refusal = gone.bind_refusal(&error);
        assert_eq!(refusal.refused, Code::GuardFailed);
        assert_eq!(
            refusal.route,
            Some(RouteView {
                dev: "lo".into(),
                role: None,
                role_by: None,
                gateway: None,
                kind: "local",
            })
        );
        assert!(refusal.lan.is_some(), "the LAN side was read");
    }

    /// An IPv6 destination has no route the guard asks for, but its refusal
    /// shows `--dev` as given and the LAN side.
    #[tokio::test]
    async fn an_ipv6_refusal_shows_dev_and_the_lan_side() {
        let guard = Guard {
            dev: Some("eth9".into()),
            ..Guard::default()
        };
        let Err(Stop::Refused(refusal)) = guard.approve_host("2001:db8::7").await else {
            panic!("refused");
        };
        assert_eq!(refusal.refused, Code::Ipv6);
        assert_eq!(refusal.dev.as_deref(), Some("eth9"));
        assert!(refusal.lan.is_some(), "the LAN side was read");
        assert!(refusal.route.is_none(), "{refusal:?}");
    }

    #[tokio::test]
    async fn the_live_network_reads_and_lists_no_loopback_on_the_lan_side() {
        let net = Network::read(&Lte::default()).await.unwrap();
        let lo = net.iface("lo").expect("every kernel has lo");
        assert!(lo.loopback() && !lo.lan_side());
        assert!(lo.ipv4.contains(&(Ipv4Addr::LOCALHOST, 8)));
        assert!(net.lan_views().iter().all(|v| v.dev != "lo"));
    }
}
