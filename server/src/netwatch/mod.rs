//! Kernel network events: what the device's links, IPv4 addresses and routes
//! are, republished whenever they change.
//!
//! One `NETLINK_ROUTE` socket listens to link, IPv4 address and IPv4 route
//! events. Any message only means "the network changed": after a quiet
//! second the links, addresses and main-table routes are dumped on a
//! second socket, and the resulting [`NetState`] is published on a watch
//! channel when it differs from the last one. Nothing polls: with no events
//! the watcher does no work at all. When the kernel drops events because the
//! socket's buffer overflowed (`ENOBUFS`), the answer is the same full dump.
//!
//! When the socket cannot be opened the watcher logs once and publishes
//! nothing, so its readers behave as if the network never changed.

pub mod mwan3;
mod netlink;
pub mod owner;
pub mod route;
pub mod source;
pub mod wg;

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::sync::watch;
use tracing::{debug, info, warn};

use netlink::{Addr, Link, NlSocket, Order, Route};

/// Quiet period between the first event of a burst and the dump. A link
/// flap, a DHCP renewal or a route swap arrives as several messages; one dump
/// after the burst sees where it ended.
const DEBOUNCE: Duration = Duration::from_secs(1);
/// Wait before retrying a failed dump or reopening a failed event socket.
/// Only a broken socket ever waits on this.
const RETRY: Duration = Duration::from_secs(5);
const GROUPS: u32 = netlink::RTMGRP_LINK | netlink::RTMGRP_IPV4_IFADDR | netlink::RTMGRP_IPV4_ROUTE;

/// The latest network state: None until the first dump, and forever when the
/// kernel socket is unavailable.
pub type NetWatch = watch::Receiver<Option<Arc<NetState>>>;
/// The publishing side of [`NetWatch`].
pub type NetPublisher = watch::Sender<Option<Arc<NetState>>>;

/// A new, empty channel.
pub fn channel() -> (NetPublisher, NetWatch) {
    watch::channel(None)
}

/// An IPv4 address with its prefix length, shown as `a.b.c.d/p`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ipv4Cidr {
    pub addr: Ipv4Addr,
    pub prefix: u8,
}

impl fmt::Display for Ipv4Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

impl Serialize for Ipv4Cidr {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// One network interface other than loopback.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Interface {
    pub name: String,
    /// Kernel interface index, for route writes. It changes when an interface
    /// is recreated under the same name, which counts as a change.
    #[serde(skip)]
    pub index: u32,
    /// `IFLA_OPERSTATE` as sysfs spells it: `up`, `down`, `lowerlayerdown`,
    /// `dormant`, `testing`, `notpresent` or `unknown`.
    pub operstate: &'static str,
    /// `IFLA_CARRIER`; None on kernels before 3.4, which do not send it.
    pub carrier: Option<bool>,
    /// The bridge (or bond) this interface is a port of, by name; None when
    /// it is not enslaved or its master is not in the same dump. Enslaving
    /// a port counts as a change.
    pub master: Option<String>,
    /// The first (primary) IPv4 address.
    pub ipv4: Option<Ipv4Cidr>,
    /// Lowest metric among the default routes through this interface.
    pub default_metric: Option<u32>,
    /// What the unit's own failover engine (mwan3) holds about this uplink,
    /// when it tracks one (ADR-005). A flip counts as a change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<mwan3::Verdict>,
}

/// A default route in the main table.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DefaultRoute {
    pub dev: String,
    pub gw: Option<Ipv4Addr>,
    pub metric: u32,
    /// The gateway is declared on the link (`onlink`), as it must be when it
    /// lies outside the interface's prefix. A route through it must say so
    /// too, or the kernel refuses it.
    #[serde(skip)]
    pub onlink: bool,
}

/// A `/32` route in the main table.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HostRoute {
    pub dst: Ipv4Addr,
    pub dev: String,
    pub via: Option<Ipv4Addr>,
    pub metric: u32,
    /// `rtm_protocol`: 2 kernel, 3 boot (`ip route add`), 4 static (netifd),
    /// 16 dhcp, [`route::RTPROT_SCTL`] for the agent's own.
    pub protocol: u8,
}

/// The device's network as the kernel sees it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct NetState {
    /// When this agent started, unix ms. A new value means `seq` restarted.
    pub boot: u64,
    /// +1 for every distinct state published since `boot`, from 1.
    pub seq: u64,
    /// Sorted by name.
    pub interfaces: Vec<Interface>,
    /// Sorted by metric, then interface.
    pub default_routes: Vec<DefaultRoute>,
    /// Sorted by destination, then metric.
    pub host_routes: Vec<HostRoute>,
}

/// `IF_OPER_*` (RFC 2863) to the words sysfs `operstate` uses.
fn operstate_name(value: u8) -> &'static str {
    match value {
        1 => "notpresent",
        2 => "down",
        3 => "lowerlayerdown",
        4 => "testing",
        5 => "dormant",
        6 => "up",
        _ => "unknown",
    }
}

impl NetState {
    /// Build a state from the three dumps. `boot` and `seq` are left 0 for
    /// the publisher to stamp.
    pub(crate) fn from_dump(links: &[Link], addrs: &[Addr], routes: &[Route]) -> Self {
        // Loopback is not listed but still names routes through it.
        let names: HashMap<u32, &str> = links.iter().map(|l| (l.index, l.name.as_str())).collect();

        let mut default_routes = Vec::new();
        let mut host_routes = Vec::new();
        for route in routes {
            if route.table != u32::from(netlink::RT_TABLE_MAIN)
                || route.kind != netlink::RTN_UNICAST
            {
                continue;
            }
            // A route through a link the link dump did not list yet belongs
            // to a change still arriving; its own event brings another dump.
            let Some(dev) = route.oif.and_then(|oif| names.get(&oif)) else {
                continue;
            };
            match (route.dst_len, route.dst) {
                (0, _) => default_routes.push(DefaultRoute {
                    dev: (*dev).to_string(),
                    gw: route.gateway,
                    metric: route.priority,
                    onlink: route.flags & netlink::RTNH_F_ONLINK != 0,
                }),
                (32, Some(dst)) => host_routes.push(HostRoute {
                    dst,
                    dev: (*dev).to_string(),
                    via: route.gateway,
                    metric: route.priority,
                    protocol: route.protocol,
                }),
                _ => {}
            }
        }
        default_routes.sort_by(|a, b| (a.metric, &a.dev, a.gw).cmp(&(b.metric, &b.dev, b.gw)));
        host_routes.sort_by(|a, b| {
            (a.dst, a.metric, &a.dev, a.via, a.protocol)
                .cmp(&(b.dst, b.metric, &b.dev, b.via, b.protocol))
        });

        // The primary address wins; a secondary stands in only when alone.
        let mut first_addr: HashMap<u32, &Addr> = HashMap::new();
        for addr in addrs {
            let slot = first_addr.entry(addr.index).or_insert(addr);
            if slot.secondary && !addr.secondary {
                *slot = addr;
            }
        }

        let mut interfaces: Vec<Interface> = links
            .iter()
            .filter(|l| l.flags & netlink::IFF_LOOPBACK == 0)
            .map(|l| Interface {
                name: l.name.clone(),
                index: l.index,
                operstate: operstate_name(l.operstate),
                carrier: l.carrier,
                master: l
                    .master
                    .and_then(|m| names.get(&m))
                    .map(|name| (*name).to_string()),
                ipv4: first_addr.get(&l.index).map(|a| Ipv4Cidr {
                    addr: a.local,
                    prefix: a.prefix,
                }),
                default_metric: default_routes
                    .iter()
                    .filter(|d| d.dev == l.name)
                    .map(|d| d.metric)
                    .min(),
                verdict: None,
            })
            .collect();
        interfaces.sort_by(|a, b| a.name.cmp(&b.name).then(a.index.cmp(&b.index)));

        Self {
            boot: 0,
            seq: 0,
            interfaces,
            default_routes,
            host_routes,
        }
    }

    /// Whether two states describe the same network, ignoring `boot` and `seq`.
    pub fn same_network(&self, other: &Self) -> bool {
        self.interfaces == other.interfaces
            && self.default_routes == other.default_routes
            && self.host_routes == other.host_routes
    }

    /// The default route with the lowest metric, the one the kernel uses.
    /// Ties go to the interface that sorts first, so the answer never depends
    /// on dump order.
    pub fn lowest_default(&self) -> Option<&DefaultRoute> {
        self.default_routes
            .iter()
            .min_by(|a, b| (a.metric, &a.dev).cmp(&(b.metric, &b.dev)))
    }

    pub fn interface(&self, name: &str) -> Option<&Interface> {
        self.interfaces.iter().find(|i| i.name == name)
    }

    /// Every main-table `/32` route to `dst`, lowest metric first.
    pub fn host_routes_to(&self, dst: Ipv4Addr) -> impl Iterator<Item = &HostRoute> {
        self.host_routes.iter().filter(move |r| r.dst == dst)
    }
}

/// The state to publish after `current`, or None when `fresh` describes the
/// same network.
fn next_published(current: Option<&NetState>, mut fresh: NetState, boot: u64) -> Option<NetState> {
    if current.is_some_and(|c| c.same_network(&fresh)) {
        return None;
    }
    fresh.boot = boot;
    fresh.seq = current.map_or(1, |c| c.seq + 1);
    Some(fresh)
}

/// Publish `fresh` if it differs from what readers hold. True when it did.
fn publish(tx: &NetPublisher, fresh: NetState, boot: u64) -> bool {
    tx.send_if_modified(
        |current| match next_published(current.as_deref(), fresh, boot) {
            Some(next) => {
                *current = Some(Arc::new(next));
                true
            }
            None => false,
        },
    )
}

/// Wait for the next published state. Pends forever when there is no watch
/// (relay mode) or the watcher has stopped, so a `select!` arm on it never
/// fires for nothing.
pub async fn next_state(rx: Option<&mut NetWatch>) -> Arc<NetState> {
    if let Some(rx) = rx {
        while rx.changed().await.is_ok() {
            if let Some(state) = rx.borrow_and_update().clone() {
                return state;
            }
        }
    }
    std::future::pending().await
}

/// Dump the network now, unstamped (`boot` and `seq` are 0), with the
/// failover engine's verdicts when the unit has one.
pub async fn dump() -> io::Result<NetState> {
    let mut socket = NlSocket::open(0)?;
    dump_with(&mut socket).await
}

async fn dump_with(socket: &mut NlSocket) -> io::Result<NetState> {
    let mut state = dump_kernel(socket).await?;
    mwan3::annotate(&mut state).await;
    Ok(state)
}

async fn dump_kernel(socket: &mut NlSocket) -> io::Result<NetState> {
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
    Ok(NetState::from_dump(&links, &addrs, &routes))
}

/// Run the watcher, publishing on `tx`. `boot` is the agent's start time in
/// unix ms. Returns only if cancelled; a kernel without the socket leaves it
/// parked with `tx` held, so readers never see the channel close.
pub async fn run(tx: Arc<NetPublisher>, boot: u64) {
    let mut opened_before = false;
    let mut verdicts = mwan3::Watcher::new();
    loop {
        let events = match NlSocket::open(GROUPS) {
            Ok(socket) => socket,
            Err(e) if !opened_before => {
                warn!(
                    "netwatch: no kernel route events ({e}); network changes will not be watched"
                );
                return std::future::pending().await;
            }
            Err(e) => {
                warn!("netwatch: reopening the event socket failed ({e}); retrying in {RETRY:?}");
                tokio::time::sleep(RETRY).await;
                continue;
            }
        };
        if !opened_before {
            info!("netwatch: watching link, address and route events");
        }
        opened_before = true;
        let e = watch_events(&events, &tx, boot, &mut verdicts).await;
        warn!("netwatch: event socket failed ({e}); reopening in {RETRY:?}");
        tokio::time::sleep(RETRY).await;
    }
}

/// Dump, publish, wait for the next burst of events (from the kernel or the
/// failover engine's verdict directory), repeat. Returns only when the event
/// socket fails.
async fn watch_events(
    events: &NlSocket,
    tx: &NetPublisher,
    boot: u64,
    verdicts: &mut mwan3::Watcher,
) -> io::Error {
    let mut buf = vec![0; netlink::RECV_BUF];
    let mut requests: Option<NlSocket> = None;
    let mut failing = false;
    loop {
        // Drain before dumping: an event that lands during the dump then
        // wakes the next round instead of being swallowed with the burst.
        drain(events, &mut buf);
        verdicts.arm();
        let dumped = match requests.as_mut() {
            Some(socket) => dump_with(socket).await,
            None => match NlSocket::open(0) {
                Ok(socket) => dump_with(requests.insert(socket)).await,
                Err(e) => Err(e),
            },
        };
        match dumped {
            Ok(state) => {
                if failing {
                    info!("netwatch: dumps work again");
                    failing = false;
                }
                if publish(tx, state, boot) {
                    debug!(
                        seq = tx.borrow().as_ref().map_or(0, |s| s.seq),
                        "netwatch: network changed"
                    );
                }
            }
            Err(e) => {
                // One warning per run of failures, not one every RETRY.
                if failing {
                    debug!("netwatch: dump failed again ({e})");
                } else {
                    warn!("netwatch: dump failed ({e}); retrying every {RETRY:?}");
                }
                failing = true;
                requests = None;
                tokio::time::sleep(RETRY).await;
                continue;
            }
        }
        tokio::select! {
            waited = wait_for_event(events, &mut buf) => {
                if let Err(e) = waited {
                    return e;
                }
            }
            () = verdicts.changed() => {}
        }
        tokio::time::sleep(DEBOUNCE).await;
    }
}

/// Wait until the kernel says something changed. `ENOBUFS` counts: events
/// were lost, and only a full dump recovers what they said.
async fn wait_for_event(events: &NlSocket, buf: &mut [u8]) -> io::Result<()> {
    loop {
        match events.recv(buf).await {
            Ok(_) => return Ok(()),
            Err(e) if e.raw_os_error() == Some(libc::ENOBUFS) => {
                debug!("netwatch: event queue overflowed; dumping");
                return Ok(());
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

/// Discard every queued event: the dump that follows covers them.
fn drain(events: &NlSocket, buf: &mut [u8]) {
    loop {
        match events.try_recv(buf) {
            Ok(_) => {}
            Err(e) if e.raw_os_error() == Some(libc::ENOBUFS) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::netlink::tests::{
        addr_message, enslaved, link_message, route_message, with_rtm_flags,
    };
    use super::netlink::{Messages, IFA_F_SECONDARY};
    use super::*;

    /// A travel router: wired WAN at metric 10, LTE at 40, a bridge with one
    /// port, and netifd's relay pin on LTE, as three dumps in the given byte
    /// order.
    fn fixture_dumps(order: Order) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let mut links = link_message(order, 1, 0x49, "lo", 0, Some(1));
        links.extend(link_message(order, 3, 0x1_1043, "eth1", 6, Some(1)));
        // The port is dumped before its bridge (index 5): resolving the
        // master's name never depends on dump order.
        links.extend(enslaved(
            order,
            link_message(order, 4, 0x1_1043, "eth0", 6, Some(1)),
            5,
        ));
        links.extend(link_message(order, 5, 0x1_1043, "br-lan", 6, Some(1)));
        links.extend(link_message(order, 7, 0x1_10d1, "wwan0", 0, None));
        let mut addrs = addr_message(order, 1, [127, 0, 0, 1], 8, 0);
        addrs.extend(addr_message(order, 3, [10, 42, 0, 99], 24, IFA_F_SECONDARY));
        addrs.extend(addr_message(order, 3, [10, 42, 0, 7], 24, 0));
        addrs.extend(addr_message(order, 5, [192, 168, 8, 1], 24, 0));
        addrs.extend(addr_message(order, 7, [10, 180, 41, 231], 30, 0));
        let mut routes = route_message(order, None, Some([10, 180, 41, 232]), 7, 40, 4, 254);
        routes.extend(route_message(
            order,
            None,
            Some([10, 42, 0, 1]),
            3,
            10,
            4,
            254,
        ));
        routes.extend(route_message(
            order,
            Some(([174, 138, 114, 209], 32)),
            Some([10, 180, 41, 232]),
            7,
            40,
            4,
            254,
        ));
        routes.extend(route_message(
            order,
            Some(([192, 168, 8, 0], 24)),
            None,
            5,
            0,
            2,
            254,
        ));
        // A policy table's default route (mwan3) is not the main table's.
        routes.extend(route_message(order, None, Some([10, 42, 0, 1]), 3, 0, 4, 1));
        (links, addrs, routes)
    }

    fn state_from(order: Order, dumps: &(Vec<u8>, Vec<u8>, Vec<u8>)) -> NetState {
        let links: Vec<Link> = Messages::new(&dumps.0, order)
            .filter_map(|m| netlink::parse_link(m.payload, order))
            .collect();
        let addrs: Vec<Addr> = Messages::new(&dumps.1, order)
            .filter_map(|m| netlink::parse_addr(m.payload, order))
            .collect();
        let routes: Vec<Route> = Messages::new(&dumps.2, order)
            .filter_map(|m| netlink::parse_route(m.payload, order))
            .collect();
        NetState::from_dump(&links, &addrs, &routes)
    }

    #[test]
    fn dumps_build_the_same_state_in_both_byte_orders() {
        let little = state_from(Order::Little, &fixture_dumps(Order::Little));
        let big = state_from(Order::Big, &fixture_dumps(Order::Big));
        assert_eq!(little, big);

        let names: Vec<&str> = little.interfaces.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(
            names,
            ["br-lan", "eth0", "eth1", "wwan0"],
            "loopback is left out, names sorted"
        );

        let eth0 = little.interface("eth0").unwrap();
        assert_eq!(
            eth0.master.as_deref(),
            Some("br-lan"),
            "the port names its bridge, resolved from the same dump"
        );
        assert_eq!(eth0.default_metric, None);
        assert!(
            little
                .interfaces
                .iter()
                .all(|i| i.name == "eth0" || i.master.is_none()),
            "only the port is enslaved"
        );

        let eth1 = little.interface("eth1").unwrap();
        assert_eq!(eth1.index, 3);
        assert_eq!(eth1.operstate, "up");
        assert_eq!(eth1.carrier, Some(true));
        assert_eq!(eth1.master, None);
        assert_eq!(
            eth1.ipv4.unwrap().to_string(),
            "10.42.0.7/24",
            "primary beats secondary"
        );
        assert_eq!(eth1.default_metric, Some(10));

        let wwan = little.interface("wwan0").unwrap();
        assert_eq!(wwan.operstate, "unknown");
        assert_eq!(wwan.carrier, None);
        assert_eq!(wwan.default_metric, Some(40));
        assert_eq!(little.interface("br-lan").unwrap().default_metric, None);

        assert_eq!(
            little.default_routes,
            vec![
                DefaultRoute {
                    dev: "eth1".into(),
                    gw: Some(Ipv4Addr::new(10, 42, 0, 1)),
                    metric: 10,
                    onlink: false,
                },
                DefaultRoute {
                    dev: "wwan0".into(),
                    gw: Some(Ipv4Addr::new(10, 180, 41, 232)),
                    metric: 40,
                    onlink: false,
                },
            ],
            "main table only, sorted by metric"
        );
        assert_eq!(
            little.host_routes,
            vec![HostRoute {
                dst: Ipv4Addr::new(174, 138, 114, 209),
                dev: "wwan0".into(),
                via: Some(Ipv4Addr::new(10, 180, 41, 232)),
                metric: 40,
                protocol: 4,
            }]
        );
    }

    #[test]
    fn an_onlink_default_route_is_marked_so() {
        for order in [Order::Little, Order::Big] {
            let (links, addrs, _) = fixture_dumps(order);
            // eth1's gateway lies outside 10.42.0.0/24: `onlink`.
            let mut routes = with_rtm_flags(
                order,
                route_message(order, None, Some([10, 9, 9, 1]), 3, 10, 4, 254),
                netlink::RTNH_F_ONLINK,
            );
            routes.extend(route_message(
                order,
                None,
                Some([10, 180, 41, 232]),
                7,
                40,
                4,
                254,
            ));
            let state = state_from(order, &(links, addrs, routes));
            let onlink: Vec<(&str, bool)> = state
                .default_routes
                .iter()
                .map(|r| (r.dev.as_str(), r.onlink))
                .collect();
            assert_eq!(onlink, [("eth1", true), ("wwan0", false)], "{order:?}");
            let mut plain = state.clone();
            plain.default_routes[0].onlink = false;
            assert!(
                !state.same_network(&plain),
                "the flag is part of the network"
            );
        }
        let fixture = state_from(Order::NATIVE, &fixture_dumps(Order::NATIVE));
        assert!(fixture.default_routes.iter().all(|r| !r.onlink));
    }

    #[test]
    fn lowest_default_picks_the_lowest_metric_then_the_first_name() {
        let state = state_from(Order::NATIVE, &fixture_dumps(Order::NATIVE));
        assert_eq!(state.lowest_default().unwrap().dev, "eth1");

        let mut tied = state.clone();
        tied.default_routes = vec![
            DefaultRoute {
                dev: "wwan0".into(),
                gw: None,
                metric: 5,
                onlink: false,
            },
            DefaultRoute {
                dev: "eth1".into(),
                gw: None,
                metric: 5,
                onlink: false,
            },
            DefaultRoute {
                dev: "eth0".into(),
                gw: None,
                metric: 7,
                onlink: false,
            },
        ];
        assert_eq!(tied.lowest_default().unwrap().dev, "eth1");

        assert!(NetState::default().lowest_default().is_none());
    }

    #[test]
    fn host_routes_to_finds_every_route_to_one_destination() {
        let mut state = state_from(Order::NATIVE, &fixture_dumps(Order::NATIVE));
        let relay = Ipv4Addr::new(174, 138, 114, 209);
        state.host_routes.insert(
            0,
            HostRoute {
                dst: relay,
                dev: "eth1".into(),
                via: None,
                metric: 0,
                protocol: route::RTPROT_SCTL,
            },
        );
        let protocols: Vec<u8> = state.host_routes_to(relay).map(|r| r.protocol).collect();
        assert_eq!(protocols, [route::RTPROT_SCTL, 4]);
        assert_eq!(state.host_routes_to(Ipv4Addr::new(1, 1, 1, 1)).count(), 0);
    }

    #[test]
    fn same_network_ignores_the_stamps_but_not_the_content() {
        let a = state_from(Order::NATIVE, &fixture_dumps(Order::NATIVE));
        let mut b = a.clone();
        b.boot = 99;
        b.seq = 42;
        assert!(a.same_network(&b));

        let mut moved = a.clone();
        moved.default_routes[0].metric = 50;
        assert!(!a.same_network(&moved));

        let mut recreated = a.clone();
        recreated.interfaces[1].index = 12;
        assert!(
            !a.same_network(&recreated),
            "a recreated interface has a new index"
        );

        let mut carrier_lost = a.clone();
        carrier_lost.interfaces[1].carrier = Some(false);
        assert!(!a.same_network(&carrier_lost));

        let mut released = a.clone();
        assert_eq!(released.interfaces[1].name, "eth0");
        released.interfaces[1].master = None;
        assert!(
            !a.same_network(&released),
            "a port leaving its bridge is a change"
        );
    }

    #[test]
    fn a_master_missing_from_the_dump_is_unknown() {
        for order in [Order::Little, Order::Big] {
            // A port whose bridge the dump does not list: the port is kept,
            // its master unresolved rather than invented.
            let (_, addrs, routes) = fixture_dumps(order);
            let mut links = link_message(order, 3, 0x1_1043, "eth1", 6, Some(1));
            links.extend(enslaved(
                order,
                link_message(order, 4, 0x1_1043, "eth0", 6, Some(1)),
                99,
            ));
            let state = state_from(order, &(links, addrs, routes));
            let eth0 = state.interface("eth0").unwrap();
            assert_eq!(eth0.master, None, "{order:?}");
        }
    }

    #[test]
    fn publishing_stamps_boot_and_counts_only_distinct_states() {
        let (tx, mut rx) = channel();
        let state = state_from(Order::NATIVE, &fixture_dumps(Order::NATIVE));

        assert!(publish(&tx, state.clone(), 1_700_000_000_000));
        let first = rx.borrow_and_update().clone().unwrap();
        assert_eq!((first.boot, first.seq), (1_700_000_000_000, 1));

        assert!(
            !publish(&tx, state.clone(), 1_700_000_000_000),
            "same network, nothing sent"
        );
        assert!(!rx.has_changed().unwrap());

        let mut changed = state;
        changed.default_routes.remove(0);
        assert!(publish(&tx, changed, 1_700_000_000_000));
        let second = rx.borrow_and_update().clone().unwrap();
        assert_eq!((second.boot, second.seq), (1_700_000_000_000, 2));
    }

    #[test]
    fn operstate_words_match_sysfs() {
        let words: Vec<&str> = (0..=7).map(operstate_name).collect();
        assert_eq!(
            words,
            [
                "unknown",
                "notpresent",
                "down",
                "lowerlayerdown",
                "testing",
                "dormant",
                "up",
                "unknown"
            ]
        );
    }

    #[test]
    fn serialized_state_names_addresses_and_skips_the_index() {
        let state = state_from(Order::NATIVE, &fixture_dumps(Order::NATIVE));
        let json = serde_json::to_value(&state).unwrap();
        let eth0 = &json["interfaces"][1];
        assert_eq!(eth0["name"], "eth0");
        assert_eq!(eth0["master"], "br-lan");
        let eth1 = &json["interfaces"][2];
        assert_eq!(eth1["name"], "eth1");
        assert_eq!(eth1["ipv4"], "10.42.0.7/24");
        assert_eq!(eth1["master"], serde_json::Value::Null);
        assert!(eth1.get("index").is_none());
        assert_eq!(json["host_routes"][0]["via"], "10.180.41.232");
    }

    #[tokio::test]
    async fn next_state_pends_without_a_watch_or_after_the_watcher_stops() {
        let quiet = tokio::time::timeout(Duration::from_millis(50), next_state(None)).await;
        assert!(quiet.is_err());

        let (tx, mut rx) = channel();
        drop(tx);
        let closed =
            tokio::time::timeout(Duration::from_millis(50), next_state(Some(&mut rx))).await;
        assert!(closed.is_err());

        let (tx, mut rx) = channel();
        publish(&tx, NetState::default(), 5);
        let got = tokio::time::timeout(Duration::from_millis(50), next_state(Some(&mut rx)))
            .await
            .unwrap();
        assert_eq!(got.seq, 1);
    }

    /// Default and /32 routes of the main table as /proc/net/route lists
    /// them, as (dev, mask, gateway, metric).
    fn proc_net_route() -> Option<Vec<(String, u32, Ipv4Addr, u32)>> {
        let text = std::fs::read_to_string("/proc/net/route").ok()?;
        let hex = |s: &str| u32::from_str_radix(s, 16).ok();
        let mut rows = Vec::new();
        for line in text.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            let (Some(gw), Some(mask), Some(metric)) = (
                f.get(2).and_then(|s| hex(s)),
                f.get(7).and_then(|s| hex(s)),
                f.get(6).and_then(|s| s.parse::<u32>().ok()),
            ) else {
                continue;
            };
            if mask == 0 || mask == u32::MAX {
                // Each address is printed as its raw network-order word.
                rows.push((
                    f[0].to_string(),
                    mask,
                    Ipv4Addr::from(gw.to_ne_bytes()),
                    metric,
                ));
            }
        }
        Some(rows)
    }

    #[tokio::test]
    async fn live_dump_matches_proc_net_route() {
        let state = match dump().await {
            Ok(state) => state,
            Err(e) => {
                eprintln!("skipped: no netlink route socket here ({e})");
                return;
            }
        };
        for iface in &state.interfaces {
            assert!(!iface.name.is_empty());
            assert_ne!(iface.name, "lo");
            // sysfs shows the same enslavement: `master` is a symlink to
            // the bridge's own directory.
            let sysfs = std::fs::read_link(format!("/sys/class/net/{}/master", iface.name))
                .ok()
                .and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()));
            assert_eq!(iface.master, sysfs, "{} master", iface.name);
        }
        for route in &state.default_routes {
            assert!(
                state.interface(&route.dev).is_some(),
                "default route through unlisted {}",
                route.dev
            );
        }
        let Some(proc_rows) = proc_net_route() else {
            return;
        };
        let mut from_proc: Vec<(String, Ipv4Addr, u32)> = proc_rows
            .iter()
            .filter(|(_, mask, _, _)| *mask == 0)
            .map(|(dev, _, gw, metric)| (dev.clone(), *gw, *metric))
            .collect();
        let mut from_netlink: Vec<(String, Ipv4Addr, u32)> = state
            .default_routes
            .iter()
            .map(|r| {
                (
                    r.dev.clone(),
                    r.gw.unwrap_or(Ipv4Addr::UNSPECIFIED),
                    r.metric,
                )
            })
            .collect();
        from_proc.sort();
        from_netlink.sort();
        assert_eq!(from_netlink, from_proc);
        let host_count = proc_rows
            .iter()
            .filter(|(_, mask, _, _)| *mask == u32::MAX)
            .count();
        assert_eq!(state.host_routes.len(), host_count);
    }

    /// Needs CAP_NET_ADMIN and skips without it. Run it for real in a
    /// throwaway namespace: `unshare -rn sh -c 'ip link set lo up && <test binary> live_route'`.
    #[tokio::test]
    async fn live_route_write_is_seen_by_the_watcher_and_removed() {
        let lo = {
            let name = std::ffi::CString::new("lo").unwrap();
            // SAFETY: name is a valid NUL-terminated string.
            unsafe { libc::if_nametoindex(name.as_ptr()) }
        };
        let owned = route::OwnedRoute {
            dst: Ipv4Addr::new(192, 0, 2, 77),
            via: None,
            oif: lo,
            metric: 7,
            onlink: false,
        };
        let (tx, mut rx) = channel();
        let watcher = tokio::spawn(run(Arc::new(tx), 1));
        // The first state is published before any write.
        tokio::time::timeout(Duration::from_secs(5), next_state(Some(&mut rx)))
            .await
            .expect("initial state");

        match route::replace(&owned).await {
            Ok(()) => {}
            Err(e) if matches!(e.raw_os_error(), Some(libc::EPERM | libc::EACCES)) => {
                eprintln!("skipped: route writes need CAP_NET_ADMIN ({e})");
                watcher.abort();
                return;
            }
            Err(e) => panic!("replace failed: {e}"),
        }
        let seen = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let state = next_state(Some(&mut rx)).await;
                let found = state.host_routes_to(owned.dst).next().cloned();
                if let Some(route) = found {
                    return (route, state.seq);
                }
            }
        })
        .await
        .expect("the watcher publishes the new route");
        assert_eq!(seen.0.protocol, route::RTPROT_SCTL);
        assert_eq!((seen.0.dev.as_str(), seen.0.metric), ("lo", 7));
        assert!(seen.1 >= 2);

        // Replacing with the same metric updates in place, never duplicates.
        route::replace(&owned).await.unwrap();
        let again = dump().await.unwrap();
        assert_eq!(again.host_routes_to(owned.dst).count(), 1);

        route::delete(&owned).await.unwrap();
        let err = route::delete(&owned).await.unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ESRCH));
        assert_eq!(dump().await.unwrap().host_routes_to(owned.dst).count(), 0);
        watcher.abort();
    }
}
