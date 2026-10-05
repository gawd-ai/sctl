//! `net.state`: the device's network, pushed to the relay when it changes.
//!
//! The device offers the feature in `tunnel.register` and sends the message
//! only when the relay's `tunnel.register.ack` advertises it too: once right
//! after the ack, then after every network change netwatch publishes whose
//! content differs from the last message sent on this connection, at most
//! one a second. `GET /api/net` answers with the same message built from a
//! fresh dump.
//!
//! ```json
//! {"type": "net.state", "v": 1, "boot": 1790000000000, "seq": 4,
//!  "ts": "2026-09-26T12:00:00Z",
//!  "interfaces": [{"name": "eth1", "operstate": "up", "carrier": true,
//!                  "metric": 10, "ip": "10.42.0.7/24", "verdict": "offline"},
//!                 {"name": "lan1", "operstate": "up", "carrier": true,
//!                  "metric": null, "ip": null, "master": "br-lan"}],
//!  "default_routes": [{"dev": "eth1", "via": "10.42.0.1", "metric": 10}],
//!  "relay_route": {"ip": "174.138.114.209", "dev": "eth1", "via": "10.42.0.1", "src": "10.42.0.7"},
//!  "tunnel": {"dev": "eth1", "local": "10.42.0.7", "remote": "174.138.114.209"},
//!  "wg_routes": [{"ip": "174.138.114.209", "dev": "eth1", "via": "10.42.0.1", "src": "10.42.0.7"}],
//!  "truncated": false}
//! ```
//!
//! An interface that is a bridge (or bond) port names its master in
//! `master`; the key is absent otherwise. `relay_route` and each of
//! `wg_routes` is the kernel's route to that address, what `ip route get`
//! says. The WireGuard endpoints are read from the kernel over generic
//! netlink, as `wg show wg0 endpoints` reads them.
//! Caps keep a message far below the 16 KiB the relay accepts: 32 interfaces
//! (those with a default route first, then those with an address), 16
//! default routes, 8 WireGuard routes and 15-byte names, with `truncated`
//! set when anything was left out.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{debug, warn};

use crate::netwatch::mwan3::Verdict;
use crate::netwatch::{self, source, wg, Interface, Ipv4Cidr, NetState, NetWatch};
use crate::state::TunnelPath;

/// The feature name in `tunnel.register` and `tunnel.register.ack`.
pub const FEATURE: &str = "net.state";
/// The message version, `v`.
pub const VERSION: u32 = 1;
/// The largest message the relay accepts.
pub const MAX_BYTES: usize = 16 * 1024;
const MAX_INTERFACES: usize = 32;
const MAX_DEFAULT_ROUTES: usize = 16;
const MAX_WG_ROUTES: usize = 8;
/// IFNAMSIZ less its NUL.
const MAX_NAME_BYTES: usize = 15;
/// At most one message per this interval on a connection.
const MIN_INTERVAL: Duration = Duration::from_secs(1);
/// The WireGuard interface whose peer endpoints are routed.
const WG_INTERFACE: &str = "wg0";

/// One `net.state` message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NetStateMessage {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub v: u32,
    /// When this agent started, unix ms.
    pub boot: u64,
    /// The netwatch sequence number of the state; 0 for a fresh dump.
    pub seq: u64,
    /// The device's clock when the message was built, for information.
    pub ts: String,
    #[serde(flatten)]
    pub body: Body,
}

/// What a message says about the network: the part compared to decide
/// whether a new message is worth sending.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Body {
    /// Every interface but loopback, sorted by name.
    pub interfaces: Vec<InterfaceEntry>,
    /// Main-table default routes, lowest metric first.
    pub default_routes: Vec<DefaultRouteEntry>,
    /// The kernel's route to the relay the tunnel is connected to.
    pub relay_route: Option<RouteEntry>,
    /// The tunnel connection's two ends.
    pub tunnel: Option<TunnelEntry>,
    /// The kernel's route to each WireGuard peer endpoint on wg0.
    pub wg_routes: Vec<RouteEntry>,
    /// Whether a cap left something out.
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InterfaceEntry {
    pub name: String,
    pub operstate: &'static str,
    pub carrier: Option<bool>,
    /// The lowest metric among the main-table default routes on this interface.
    pub metric: Option<u32>,
    pub ip: Option<Ipv4Cidr>,
    /// The bridge (or bond) this interface is a port of; absent when it is
    /// not enslaved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub master: Option<String>,
    /// The failover engine's verdict on this uplink (`online`, `offline`);
    /// absent when the unit has none or it does not track this interface.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<Verdict>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DefaultRouteEntry {
    pub dev: String,
    pub via: Option<Ipv4Addr>,
    pub metric: u32,
}

/// The kernel's route to `ip`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RouteEntry {
    pub ip: Ipv4Addr,
    pub dev: String,
    pub via: Option<Ipv4Addr>,
    pub src: Option<Ipv4Addr>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TunnelEntry {
    pub dev: Option<String>,
    pub local: IpAddr,
    pub remote: IpAddr,
}

/// What a message says beyond the netwatch state, looked up in the kernel
/// each time one is built.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Lookups {
    pub relay_route: Option<RouteEntry>,
    pub tunnel: Option<TunnelPath>,
    pub wg_routes: Vec<RouteEntry>,
    /// More WireGuard endpoints existed than the cap looks up.
    pub wg_truncated: bool,
}

/// Whether the relay's `tunnel.register.ack` advertises `net.state`.
pub fn advertised(ack: &Value) -> bool {
    ack["features"]
        .as_array()
        .is_some_and(|features| features.iter().any(|f| f.as_str() == Some(FEATURE)))
}

/// Look up the routes a message reports: to the relay at the far end of
/// `tunnel`, and to each WireGuard peer endpoint when `net` has a wg0.
pub async fn look_up(net: &NetState, tunnel: Option<TunnelPath>) -> Lookups {
    let relay = tunnel.as_ref().and_then(|path| match path.remote.ip() {
        IpAddr::V4(ip) => Some(ip),
        IpAddr::V6(_) => None,
    });
    let relay_route = match relay {
        Some(ip) => kernel_route(ip).await,
        None => None,
    };
    let (wg_routes, wg_truncated) = if net.interface(WG_INTERFACE).is_some() {
        wg_routes().await
    } else {
        (Vec::new(), false)
    };
    Lookups {
        relay_route,
        tunnel,
        wg_routes,
        wg_truncated,
    }
}

async fn kernel_route(ip: Ipv4Addr) -> Option<RouteEntry> {
    match source::route_get(ip).await {
        Ok(route) => Some(RouteEntry {
            ip,
            dev: route.dev,
            via: route.via,
            src: route.src,
        }),
        Err(e) => {
            debug!("net.state: no route to {ip} ({e})");
            None
        }
    }
}

/// Routes to the first [`MAX_WG_ROUTES`] IPv4 endpoints on wg0, by address,
/// and whether more existed.
async fn wg_routes() -> (Vec<RouteEntry>, bool) {
    let endpoints = match wg::endpoints(WG_INTERFACE).await {
        Ok(endpoints) => endpoints,
        Err(e) => {
            debug!("net.state: {WG_INTERFACE} endpoints unavailable ({e})");
            return (Vec::new(), false);
        }
    };
    let mut ips: Vec<Ipv4Addr> = endpoints
        .iter()
        .filter_map(|endpoint| match endpoint.ip() {
            IpAddr::V4(ip) => Some(ip),
            IpAddr::V6(_) => None,
        })
        .collect();
    ips.sort_unstable();
    ips.dedup();
    let truncated = ips.len() > MAX_WG_ROUTES;
    let mut routes = Vec::new();
    for ip in ips.into_iter().take(MAX_WG_ROUTES) {
        routes.extend(kernel_route(ip).await);
    }
    (routes, truncated)
}

/// `name` cut to [`MAX_NAME_BYTES`] on a character boundary; a cut sets
/// `truncated`.
fn capped(name: &str, truncated: &mut bool) -> String {
    if name.len() <= MAX_NAME_BYTES {
        return name.to_string();
    }
    *truncated = true;
    let mut end = MAX_NAME_BYTES;
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    name[..end].to_string()
}

fn capped_route(mut route: RouteEntry, truncated: &mut bool) -> RouteEntry {
    route.dev = capped(&route.dev, truncated);
    route
}

/// Build the message for `net` and `lookups`, applying the caps.
pub fn build(net: &NetState, lookups: Lookups, ts: String) -> NetStateMessage {
    let mut truncated = lookups.wg_truncated;

    let mut kept: Vec<&Interface> = net.interfaces.iter().collect();
    if kept.len() > MAX_INTERFACES {
        truncated = true;
        // A stable sort: within each rank the name order holds.
        kept.sort_by_key(|i| (i.default_metric.is_none(), i.ipv4.is_none()));
        kept.truncate(MAX_INTERFACES);
    }
    let mut interfaces: Vec<InterfaceEntry> = kept
        .into_iter()
        .map(|i| InterfaceEntry {
            name: capped(&i.name, &mut truncated),
            operstate: i.operstate,
            carrier: i.carrier,
            metric: i.default_metric,
            ip: i.ipv4,
            master: i.master.as_deref().map(|m| capped(m, &mut truncated)),
            verdict: i.verdict,
        })
        .collect();
    interfaces.sort_by(|a, b| a.name.cmp(&b.name));

    if net.default_routes.len() > MAX_DEFAULT_ROUTES {
        truncated = true;
    }
    let default_routes = net
        .default_routes
        .iter()
        .take(MAX_DEFAULT_ROUTES)
        .map(|r| DefaultRouteEntry {
            dev: capped(&r.dev, &mut truncated),
            via: r.gw,
            metric: r.metric,
        })
        .collect();

    let relay_route = lookups.relay_route.map(|r| capped_route(r, &mut truncated));
    let tunnel = lookups.tunnel.map(|path| TunnelEntry {
        dev: path.dev.map(|dev| capped(&dev, &mut truncated)),
        local: path.local.ip(),
        remote: path.remote.ip(),
    });
    let mut wg = lookups.wg_routes;
    if wg.len() > MAX_WG_ROUTES {
        truncated = true;
        wg.truncate(MAX_WG_ROUTES);
    }
    let wg_routes = wg
        .into_iter()
        .map(|r| capped_route(r, &mut truncated))
        .collect();

    NetStateMessage {
        kind: FEATURE,
        v: VERSION,
        boot: net.boot,
        seq: net.seq,
        ts,
        body: Body {
            interfaces,
            default_routes,
            relay_route,
            tunnel,
            wg_routes,
            truncated,
        },
    }
}

/// Start this connection's `net.state` forwarder, unless the relay did not
/// advertise the feature or the network is not watched. `look` gathers the
/// [`Lookups`] for a state (the tunnel client passes [`look_up`] with its
/// path). The caller aborts the task when the connection ends.
pub fn spawn_forwarder<F, Fut>(
    relay_takes_it: bool,
    net: Option<NetWatch>,
    tx: mpsc::Sender<WsMessage>,
    look: F,
) -> Option<JoinHandle<()>>
where
    F: FnMut(Arc<NetState>) -> Fut + Send + 'static,
    Fut: Future<Output = Lookups> + Send + 'static,
{
    if !relay_takes_it {
        return None;
    }
    Some(tokio::spawn(forward(net?, tx, look, MIN_INTERVAL)))
}

/// Send the latest state now (or once there is one), then after every
/// published change whose message body differs from the last one sent, at
/// most one per `min_interval`. A send waits for room in the writer's queue:
/// a state is never dropped, only superseded by a newer one. Returns when the
/// writer is gone.
async fn forward<F, Fut>(
    mut net: NetWatch,
    tx: mpsc::Sender<WsMessage>,
    mut look: F,
    min_interval: Duration,
) where
    F: FnMut(Arc<NetState>) -> Fut,
    Fut: Future<Output = Lookups>,
{
    let mut sent: Option<Body> = None;
    let mut sent_at: Option<Instant> = None;
    let mut ready = net.borrow_and_update().clone();
    loop {
        let state = match ready.take() {
            Some(state) => state,
            None => netwatch::next_state(Some(&mut net)).await,
        };
        if let Some(at) = sent_at {
            tokio::time::sleep_until(at + min_interval).await;
        }
        // A state published during the wait supersedes this one.
        let state = net.borrow_and_update().clone().unwrap_or(state);
        let message = build(&state, look(state.clone()).await, crate::infra::now_iso());
        if sent.as_ref() == Some(&message.body) {
            continue;
        }
        let text = match serde_json::to_string(&message) {
            Ok(text) => text,
            Err(e) => {
                warn!("net.state: serialize failed: {e}");
                continue;
            }
        };
        if text.len() > MAX_BYTES {
            warn!(
                bytes = text.len(),
                "net.state: message over the relay's limit despite the caps; not sent"
            );
            continue;
        }
        if tx.send(WsMessage::Text(text.into())).await.is_err() {
            return;
        }
        debug!(seq = message.seq, "net.state: sent");
        sent = Some(message.body);
        sent_at = Some(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use std::net::{SocketAddr, SocketAddrV4};

    use serde_json::json;

    use super::*;
    use crate::netwatch::DefaultRoute;

    fn iface(name: &str, ip: Option<[u8; 4]>, metric: Option<u32>) -> Interface {
        Interface {
            name: name.to_string(),
            index: 0,
            operstate: "up",
            carrier: Some(true),
            master: None,
            ipv4: ip.map(|octets| Ipv4Cidr {
                addr: Ipv4Addr::from(octets),
                prefix: 24,
            }),
            default_metric: metric,
            verdict: None,
        }
    }

    fn travel_router(seq: u64, eth1: [u8; 4]) -> NetState {
        NetState {
            boot: 1_790_000_000_000,
            seq,
            interfaces: vec![
                iface("br-lan", Some([192, 168, 8, 1]), None),
                iface("eth1", Some(eth1), Some(10)),
                Interface {
                    master: Some("br-lan".into()),
                    ..iface("lan1", None, None)
                },
                Interface {
                    operstate: "unknown",
                    carrier: None,
                    ..iface("wwan0", Some([10, 180, 41, 231]), Some(40))
                },
            ],
            default_routes: vec![
                DefaultRoute {
                    dev: "eth1".into(),
                    gw: Some(Ipv4Addr::new(10, 42, 0, 1)),
                    metric: 10,
                    onlink: false,
                },
                DefaultRoute {
                    dev: "wwan0".into(),
                    gw: None,
                    metric: 40,
                    onlink: false,
                },
            ],
            host_routes: Vec::new(),
        }
    }

    const RELAY: Ipv4Addr = Ipv4Addr::new(174, 138, 114, 209);

    fn relay_route(dev: &str) -> RouteEntry {
        RouteEntry {
            ip: RELAY,
            dev: dev.to_string(),
            via: Some(Ipv4Addr::new(10, 42, 0, 1)),
            src: Some(Ipv4Addr::new(10, 42, 0, 7)),
        }
    }

    fn path() -> TunnelPath {
        TunnelPath {
            local: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(10, 42, 0, 7), 51838)),
            remote: SocketAddr::V4(SocketAddrV4::new(RELAY, 443)),
            dev: Some("eth1".into()),
        }
    }

    #[test]
    fn the_message_has_the_wire_shape() {
        let lookups = Lookups {
            relay_route: Some(relay_route("eth1")),
            tunnel: Some(path()),
            wg_routes: vec![relay_route("eth1")],
            wg_truncated: false,
        };
        let message = build(
            &travel_router(4, [10, 42, 0, 7]),
            lookups,
            "2026-09-26T12:00:00Z".into(),
        );
        let route =
            json!({"ip": "174.138.114.209", "dev": "eth1", "via": "10.42.0.1", "src": "10.42.0.7"});
        assert_eq!(
            serde_json::to_value(&message).unwrap(),
            json!({
                "type": "net.state",
                "v": 1,
                "boot": 1_790_000_000_000_u64,
                "seq": 4,
                "ts": "2026-09-26T12:00:00Z",
                "interfaces": [
                    {"name": "br-lan", "operstate": "up", "carrier": true, "metric": null, "ip": "192.168.8.1/24"},
                    {"name": "eth1", "operstate": "up", "carrier": true, "metric": 10, "ip": "10.42.0.7/24"},
                    {"name": "lan1", "operstate": "up", "carrier": true, "metric": null, "ip": null, "master": "br-lan"},
                    {"name": "wwan0", "operstate": "unknown", "carrier": null, "metric": 40, "ip": "10.180.41.231/24"},
                ],
                "default_routes": [
                    {"dev": "eth1", "via": "10.42.0.1", "metric": 10},
                    {"dev": "wwan0", "via": null, "metric": 40},
                ],
                "relay_route": route,
                "tunnel": {"dev": "eth1", "local": "10.42.0.7", "remote": "174.138.114.209"},
                "wg_routes": [route],
                "truncated": false,
            })
        );
    }

    #[test]
    fn nothing_known_about_the_paths_is_null_and_empty() {
        let message = build(&NetState::default(), Lookups::default(), String::new());
        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(value["relay_route"], Value::Null);
        assert_eq!(value["tunnel"], Value::Null);
        assert_eq!(value["wg_routes"], json!([]));
        assert_eq!(value["interfaces"], json!([]));
        assert_eq!(value["truncated"], false);
    }

    /// 40 interfaces: 5 with a default route, 30 with only an address, 5
    /// with neither, interleaved by name.
    fn crowded() -> NetState {
        let mut interfaces = Vec::new();
        for n in 0..40u8 {
            let (ip, metric) = match n % 8 {
                0 => (Some([10, 0, n, 1]), Some(u32::from(n))),
                7 => (None, None),
                _ => (Some([10, 1, n, 1]), None),
            };
            interfaces.push(iface(&format!("if{n:02}"), ip, metric));
        }
        let default_routes = (0..20u32)
            .map(|metric| DefaultRoute {
                dev: format!("if{metric:02}"),
                gw: None,
                metric,
                onlink: false,
            })
            .collect();
        NetState {
            interfaces,
            default_routes,
            ..NetState::default()
        }
    }

    #[test]
    fn caps_keep_the_uplinks_then_the_addressed_and_say_so() {
        let wg_routes = (0..10u8)
            .map(|n| RouteEntry {
                ip: Ipv4Addr::new(198, 51, 100, n),
                ..relay_route("eth1")
            })
            .collect();
        let lookups = Lookups {
            wg_routes,
            ..Lookups::default()
        };
        let message = build(&crowded(), lookups, String::new());
        let body = &message.body;
        assert!(body.truncated);
        assert_eq!(body.interfaces.len(), 32);
        let names: Vec<&str> = body.interfaces.iter().map(|i| i.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "still sorted by name");
        for n in (0..40).step_by(8) {
            assert!(
                names.contains(&format!("if{n:02}").as_str()),
                "uplink if{n:02} kept"
            );
        }
        assert!(
            body.interfaces.iter().all(|i| i.ip.is_some()),
            "the 30 addressed fill the rest before any without an address"
        );
        assert_eq!(body.default_routes.len(), 16);
        assert_eq!(
            body.default_routes.last().unwrap().metric,
            15,
            "lowest metrics kept"
        );
        assert_eq!(body.wg_routes.len(), 8);
        assert_eq!(body.wg_routes[7].ip, Ipv4Addr::new(198, 51, 100, 7));
    }

    #[test]
    fn under_the_caps_nothing_is_truncated() {
        let mut net = crowded();
        net.interfaces.truncate(32);
        net.default_routes.truncate(16);
        let message = build(&net, Lookups::default(), String::new());
        assert!(!message.body.truncated);
        assert_eq!(message.body.interfaces.len(), 32);
    }

    #[test]
    fn long_names_are_cut_on_a_character_boundary() {
        let mut net = NetState::default();
        net.interfaces
            .push(iface("a-very-long-interface-name", None, None));
        net.interfaces.push(iface("é-é-é-é-é-é-é-é", None, None));
        let message = build(&net, Lookups::default(), String::new());
        assert!(message.body.truncated);
        assert_eq!(message.body.interfaces[0].name, "a-very-long-int");
        assert!(message.body.interfaces[1].name.len() <= MAX_NAME_BYTES);
        assert!(message.body.interfaces[1].name.starts_with("é-é-é"));

        let mut port = iface("lan1", None, None);
        port.master = Some("a-very-long-bridge-name".into());
        let mut net = NetState::default();
        net.interfaces.push(port);
        let message = build(&net, Lookups::default(), String::new());
        assert!(message.body.truncated, "a master name is capped too");
        assert_eq!(
            message.body.interfaces[0].master.as_deref(),
            Some("a-very-long-bri")
        );

        let mut truncated = false;
        assert_eq!(capped("eth0", &mut truncated), "eth0");
        assert!(!truncated, "a name that fits is not a cut");
        let lookups = Lookups {
            relay_route: Some(relay_route("a-very-long-tunnel-dev")),
            ..Lookups::default()
        };
        let message = build(&NetState::default(), lookups, String::new());
        assert!(message.body.truncated);
        assert_eq!(message.body.relay_route.unwrap().dev, "a-very-long-tun");
    }

    #[test]
    fn a_message_at_every_cap_stays_under_the_relay_limit() {
        // Control characters escape to six bytes each in JSON.
        let name = "\u{1}".repeat(MAX_NAME_BYTES);
        let mut net = NetState {
            boot: u64::MAX,
            seq: u64::MAX,
            ..NetState::default()
        };
        for n in 0..MAX_INTERFACES {
            net.interfaces.push(Interface {
                name: format!("{name}{n}"),
                operstate: "lowerlayerdown",
                master: Some(name.clone()),
                ..iface("", Some([255, 255, 255, 255]), Some(u32::MAX))
            });
        }
        for _ in 0..MAX_DEFAULT_ROUTES {
            net.default_routes.push(DefaultRoute {
                dev: name.clone(),
                gw: Some(Ipv4Addr::BROADCAST),
                metric: u32::MAX,
                onlink: false,
            });
        }
        let route = RouteEntry {
            ip: Ipv4Addr::BROADCAST,
            dev: name.clone(),
            via: Some(Ipv4Addr::BROADCAST),
            src: Some(Ipv4Addr::BROADCAST),
        };
        let lookups = Lookups {
            relay_route: Some(route.clone()),
            tunnel: Some(TunnelPath {
                dev: Some(name.clone()),
                ..path()
            }),
            wg_routes: vec![route; MAX_WG_ROUTES],
            wg_truncated: false,
        };
        let message = build(&net, lookups, "9999-12-31T23:59:59Z".into());
        let bytes = serde_json::to_string(&message).unwrap().len();
        assert!(bytes < MAX_BYTES, "{bytes} bytes");
    }

    #[test]
    fn only_an_ack_that_names_the_feature_advertises_it() {
        assert!(advertised(
            &json!({"type": "tunnel.register.ack", "features": ["net.state"]})
        ));
        assert!(advertised(&json!({"features": ["x", "net.state"]})));
        assert!(!advertised(
            &json!({"type": "tunnel.register.ack", "serial": "D1"})
        ));
        assert!(!advertised(&json!({"features": []})));
        assert!(!advertised(&json!({"features": "net.state"})));
        assert!(!advertised(&json!({"features": ["net.states"]})));
    }

    fn no_lookups(_: Arc<NetState>) -> std::future::Ready<Lookups> {
        std::future::ready(Lookups::default())
    }

    /// The forwarder's interval in these tests, and how long one waits to
    /// call a forwarder silent.
    const INTERVAL: Duration = Duration::from_millis(100);
    const QUIET: Duration = Duration::from_millis(400);

    fn start(net: NetWatch, tx: mpsc::Sender<WsMessage>) -> JoinHandle<()> {
        tokio::spawn(forward(net, tx, no_lookups, INTERVAL))
    }

    async fn next_message(rx: &mut mpsc::Receiver<WsMessage>) -> Value {
        let message = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("a message")
            .expect("the forwarder is running");
        match message {
            WsMessage::Text(text) => serde_json::from_str(&text).unwrap(),
            other => panic!("not text: {other:?}"),
        }
    }

    async fn silent(rx: &mut mpsc::Receiver<WsMessage>) -> bool {
        tokio::time::timeout(QUIET, rx.recv()).await.is_err()
    }

    #[tokio::test]
    async fn the_forwarder_sends_at_start_and_on_change_only() {
        let (publisher, watch) = netwatch::channel();
        publisher.send_replace(Some(Arc::new(travel_router(1, [10, 42, 0, 7]))));
        let (tx, mut rx) = mpsc::channel(8);
        let task = start(watch, tx);

        let first = next_message(&mut rx).await;
        assert_eq!(first["type"], "net.state");
        assert_eq!(first["seq"], 1);

        // A new sequence number over the same network says nothing new.
        publisher.send_replace(Some(Arc::new(travel_router(2, [10, 42, 0, 7]))));
        assert!(silent(&mut rx).await);

        publisher.send_replace(Some(Arc::new(travel_router(3, [10, 42, 0, 8]))));
        let changed = next_message(&mut rx).await;
        assert_eq!(changed["seq"], 3);
        assert_eq!(changed["interfaces"][1]["ip"], "10.42.0.8/24");
        task.abort();
    }

    #[tokio::test]
    async fn changes_inside_the_interval_send_only_the_latest_after_it() {
        let (publisher, watch) = netwatch::channel();
        publisher.send_replace(Some(Arc::new(travel_router(1, [10, 42, 0, 7]))));
        let (tx, mut rx) = mpsc::channel(8);
        let task = start(watch, tx);
        next_message(&mut rx).await;
        let first_at = Instant::now();

        publisher.send_replace(Some(Arc::new(travel_router(2, [10, 42, 0, 8]))));
        tokio::task::yield_now().await;
        publisher.send_replace(Some(Arc::new(travel_router(3, [10, 42, 0, 9]))));
        let latest = next_message(&mut rx).await;
        assert_eq!(
            latest["seq"], 3,
            "the newer state supersedes the one waiting"
        );
        assert!(first_at.elapsed() >= INTERVAL);
        assert!(silent(&mut rx).await);
        task.abort();
    }

    #[tokio::test]
    async fn a_watch_that_has_not_published_sends_its_first_state() {
        let (publisher, watch) = netwatch::channel();
        let (tx, mut rx) = mpsc::channel(8);
        let task = start(watch, tx);
        assert!(silent(&mut rx).await);
        publisher.send_replace(Some(Arc::new(travel_router(1, [10, 42, 0, 7]))));
        assert_eq!(next_message(&mut rx).await["seq"], 1);
        task.abort();
    }

    #[tokio::test]
    async fn the_lookups_ride_along() {
        let (publisher, watch) = netwatch::channel();
        publisher.send_replace(Some(Arc::new(travel_router(1, [10, 42, 0, 7]))));
        let (tx, mut rx) = mpsc::channel(8);
        let look = |_: Arc<NetState>| async {
            Lookups {
                relay_route: Some(relay_route("wwan0")),
                tunnel: Some(path()),
                ..Lookups::default()
            }
        };
        let task = spawn_forwarder(true, Some(watch), tx, look).unwrap();
        let message = next_message(&mut rx).await;
        assert_eq!(message["relay_route"]["dev"], "wwan0");
        assert_eq!(message["tunnel"]["remote"], "174.138.114.209");
        task.abort();
    }

    #[tokio::test]
    async fn without_the_ack_feature_or_a_watch_nothing_is_sent() {
        let (publisher, watch) = netwatch::channel();
        publisher.send_replace(Some(Arc::new(travel_router(1, [10, 42, 0, 7]))));
        let (tx, mut rx) = mpsc::channel(8);
        let old_relay = json!({"type": "tunnel.register.ack", "serial": "D1"});
        assert!(
            spawn_forwarder(advertised(&old_relay), Some(watch), tx.clone(), no_lookups).is_none()
        );
        assert!(spawn_forwarder(true, None, tx.clone(), no_lookups).is_none());
        // `tx` stays open, so silence is a wait that times out.
        assert!(silent(&mut rx).await);
        drop(tx);
    }

    #[tokio::test]
    async fn the_kernel_answers_for_a_relay_on_loopback() {
        // The production path end to end: a fresh dump, the kernel's route
        // to the relay, and the message built from both.
        let net = netwatch::dump().await.unwrap();
        let tunnel = TunnelPath {
            local: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 40022)),
            remote: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 443)),
            dev: Some("lo".into()),
        };
        let lookups = look_up(&net, Some(tunnel)).await;
        let message = build(&net, lookups, crate::infra::now_iso());
        assert!(serde_json::to_string(&message).unwrap().len() < MAX_BYTES);
        assert!(message.body.interfaces.iter().all(|i| i.name != "lo"));
        let relay_route = message.body.relay_route.expect("a route to loopback");
        assert_eq!(relay_route.ip, Ipv4Addr::LOCALHOST);
        assert_eq!(relay_route.dev, "lo");
        assert_eq!(relay_route.src, Some(Ipv4Addr::LOCALHOST));
    }

    #[tokio::test]
    async fn the_forwarder_ends_with_the_writer() {
        let (publisher, watch) = netwatch::channel();
        publisher.send_replace(Some(Arc::new(travel_router(1, [10, 42, 0, 7]))));
        let (tx, rx) = mpsc::channel(8);
        drop(rx);
        let task = spawn_forwarder(true, Some(watch), tx, no_lookups).unwrap();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("ends")
            .unwrap();
    }
}
