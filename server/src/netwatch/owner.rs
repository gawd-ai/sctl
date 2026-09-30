//! One owner for the host route to the relay (`[tunnel] relay_route`).
//!
//! With `relay_route = "follow_default"` or `"prefer"` the agent keeps one
//! route, `relay/32 via <gw> dev <uplink> metric 0 proto 83`, where `relay` is the
//! address the tunnel connected to. At metric 0 it outranks the pin netifd
//! installs for the WireGuard endpoint (the same address) without touching
//! it. It stays in place when the agent exits, since WireGuard uses it too.
//!
//! The uplink is the lowest-metric default route whose interface is not
//! suspect ([`choose`]): netifd's metrics already rank the wire above LTE.
//! Where they do not (the WE826 and the BPI put LTE at the lower metric),
//! `relay_route = "prefer"` names the uplinks to take first, in order,
//! whatever their metrics; the unlisted ones follow by metric. The uplink
//! is chosen again on every network change netwatch publishes and after every
//! registration. The route only moves to an uplink that answers the relay,
//! unless the tunnel is registered over it already. Asking is a [`Probe`]
//! bound to that interface (`SO_BINDTODEVICE`): the tunnel's own TLS
//! handshake with the relay, so an uplink that passes TCP but breaks TLS does
//! not answer. An uplink that does not answer is suspect.
//!
//! A link that is up while its internet is dead sends no kernel event. The
//! tunnel's own failures are the signal: after two in a row on the uplink the
//! route uses (a dial nothing answered, a handshake that broke or stalled, a
//! pong timeout, a read or write error), that uplink and every other one
//! with a default route are asked in the same round. The route moves only
//! when its own uplink does not answer while another does, to the first of
//! those in that same order. The failing uplink, and any ranked above the one
//! that answered, become suspect. When every uplink fails, the relay itself
//! is down: nothing moves and nothing becomes suspect.
//!
//! A suspect uplink is asked again at once when netwatch reports a change on
//! it (link, address or default route: a replug, a new lease), or no longer
//! suspect once the tunnel registers over it. Otherwise it is probed on its
//! own schedule, which exists only while it is suspect: after 2, 5 and 10
//! minutes, then every 15. It gets the route back after two good probes in a
//! row. An uplink that fails again within 10 minutes of getting the route
//! back is flapping: its waits double each time (never above an hour), until
//! it keeps the route for an hour. In steady state the owner runs no timer at
//! all.
//!
//! Only routes carrying [`RTPROT_SCTL`] are written or deleted. A route
//! someone else installed at the same destination and metric would be
//! overwritten by a replace, so the owner leaves the relay to it and says so.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::{join, join_all};
use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpSocket, TcpStream};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{debug, info, warn};

use super::route::{self, OwnedRoute, RTPROT_SCTL};
use super::{next_state, source, DefaultRoute, HostRoute, NetState, NetWatch};
use crate::config::{RelayRouteMode, TunnelConfig};
use crate::state::{TunnelEventType, TunnelStats};
use crate::tunnel::client;

/// Tunnel failures in a row on the route's uplink before the others are asked.
const FAILURES_TO_FAIL_OVER: u8 = 2;
/// Good probes in a row before a suspect uplink gets the route back.
const PROBES_TO_RETURN: u8 = 2;
/// Tunnel signals queued for the owner; newer ones are dropped past this.
const SIGNAL_QUEUE: usize = 16;
/// An uplink that fails again this soon after the route came back to it is
/// flapping: its waits before it is asked again double.
const RELAPSE_WINDOW: Duration = Duration::from_mins(10);
/// A flapping uplink's waits stop growing here.
const MAX_RETURN_WAIT: Duration = Duration::from_hours(1);
/// An uplink that keeps the route this long is forgiven its flapping.
const FORGIVE_AFTER: Duration = Duration::from_hours(1);
/// The doubling stops here: 2 minutes times 32 is past the hour already.
const MAX_STRETCH: u32 = 32;

/// How the owner paces its probes. Tests shorten it.
#[derive(Clone, Debug)]
pub struct Timing {
    /// Waits between probe rounds while an uplink is suspect. The last one
    /// repeats.
    pub return_schedule: Vec<Duration>,
    /// How long one probe's TCP connect may take.
    pub probe_timeout: Duration,
    /// How long one probe's TLS handshake may take, after the connect.
    pub handshake_timeout: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            return_schedule: vec![
                Duration::from_mins(2),
                Duration::from_mins(5),
                Duration::from_mins(10),
                Duration::from_mins(15),
            ],
            // What the tunnel itself gives a TCP connect and a TLS handshake.
            probe_timeout: Duration::from_secs(10),
            handshake_timeout: Duration::from_secs(15),
        }
    }
}

/// How a probe asks an uplink whether it reaches the relay: the first steps
/// of the tunnel's own connection, bound to that uplink.
///
/// A bare TCP connect is not enough over `wss://`: an uplink that accepts
/// TCP but breaks TLS (SSL inspection, a captive portal) would pass it while
/// the tunnel fails on it, and the route would go back and forth. So the
/// probe of a `wss://` tunnel makes the tunnel's TLS handshake, with its
/// server name (SNI), its CA file and its certificate pin, and closes the
/// connection straight after: no WebSocket upgrade, and the tunnel key is
/// never sent.
#[derive(Clone)]
pub enum Probe {
    /// A TCP connect, all a plain `ws://` tunnel does before its upgrade.
    Connect,
    /// A TCP connect and the TLS handshake of the tunnel configured here.
    Tls(Arc<TunnelConfig>),
}

impl Probe {
    /// The probe that matches the tunnel: its TLS handshake for a `wss://`
    /// url, a connect otherwise.
    pub fn for_tunnel(config: &TunnelConfig) -> Self {
        if config
            .url
            .as_deref()
            .is_some_and(|url| url.starts_with("wss://"))
        {
            Self::Tls(Arc::new(config.clone()))
        } else {
            Self::Connect
        }
    }

    /// Ask the relay at `relay` over `dev` alone, from `src`. The error says
    /// why it did not answer.
    pub async fn ask(
        &self,
        relay: SocketAddrV4,
        dev: &str,
        src: Ipv4Addr,
        timing: &Timing,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let tcp = connect_from(relay, dev, src, timing.probe_timeout).await?;
        let Self::Tls(config) = self else {
            return Ok(());
        };
        let url = config.url.as_deref().unwrap_or_default();
        let mut tls = tokio::time::timeout(
            timing.handshake_timeout,
            client::tls_handshake(url, tcp, config),
        )
        .await
        .map_err(|_| "TLS handshake timed out")??;
        // A close_notify, so the relay sees the end of a TLS session rather
        // than a reset. Whether it gets there changes nothing.
        let _ = tokio::time::timeout(Duration::from_secs(1), tls.shutdown()).await;
        Ok(())
    }
}

/// What the tunnel client tells the owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TunnelSignal {
    /// Registered with the relay at `relay`, leaving from `local`.
    Registered {
        relay: SocketAddrV4,
        local: Ipv4Addr,
    },
    /// An attempt or a connection failed the way a dead path fails: a dial
    /// nothing answered, a handshake or registration that broke or stalled, a
    /// pong timeout, a read or write error. `from` is the local address it
    /// left from and `relay` the address it dialed, when known.
    Failed {
        from: Option<Ipv4Addr>,
        relay: Option<SocketAddrV4>,
    },
}

/// `tunnel.relay_route` in `/api/health`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Report {
    pub mode: RelayRouteMode,
    /// The uplinks preferred in order (`relay_route = "prefer"`); shown only
    /// when set.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub prefer: Vec<String>,
    /// The uplink sctl's route to the relay goes through; None while sctl
    /// holds no route.
    pub dev: Option<String>,
    pub via: Option<Ipv4Addr>,
    /// Uplinks that did not answer the relay, waiting to be asked again.
    pub suspect: Vec<String>,
    /// Uplinks the owner may probe whose reverse-path filtering is strict
    /// (`rp_filter` 1), read when the report is asked for. The answer to a
    /// probe arrives on the uplink it left by, which strict filtering drops
    /// while the relay route uses another one: such an uplink never answers,
    /// and once suspect it stays suspect.
    pub rp_filter_strict: Vec<String>,
    /// Every uplink with a default route: the ones the owner may probe.
    #[serde(skip)]
    uplinks: Vec<String>,
}

/// The uplinks among `uplinks` where reverse-path filtering is strict, with
/// `read` giving `net.ipv4.conf.<name>.rp_filter`. The kernel applies the
/// larger of `conf/all` and `conf/<dev>`, so an uplink is strict when that
/// larger value is 1. A value that cannot be read counts as 0.
pub(crate) fn strict_rp_filter(
    uplinks: &[String],
    read: impl Fn(&str) -> Option<u8>,
) -> Vec<String> {
    let all = read("all").unwrap_or(0);
    uplinks
        .iter()
        .filter(|dev| read(dev).unwrap_or(0).max(all) == 1)
        .cloned()
        .collect()
}

/// `/proc/sys/net/ipv4/conf/<name>/rp_filter`, as this network namespace
/// sees it.
fn read_rp_filter(name: &str) -> Option<u8> {
    std::fs::read_to_string(format!("/proc/sys/net/ipv4/conf/{name}/rp_filter"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// The owner's shared side: the tunnel's signals in, the report out. One per
/// tunnel client, whatever the mode.
pub struct RelayRoute {
    mode: RelayRouteMode,
    /// `[tunnel] relay_route_prefer`: empty unless the mode is `prefer`.
    prefer: Vec<String>,
    tx: mpsc::Sender<TunnelSignal>,
    /// Held by the running owner; a restarted one takes it back.
    rx: tokio::sync::Mutex<mpsc::Receiver<TunnelSignal>>,
    report: std::sync::Mutex<Report>,
}

impl RelayRoute {
    pub fn new(mode: RelayRouteMode, prefer: Vec<String>) -> Self {
        let (tx, rx) = mpsc::channel(SIGNAL_QUEUE);
        Self {
            mode,
            prefer: prefer.clone(),
            tx,
            rx: tokio::sync::Mutex::new(rx),
            report: std::sync::Mutex::new(Report {
                mode,
                prefer,
                dev: None,
                via: None,
                suspect: Vec::new(),
                rp_filter_strict: Vec::new(),
                uplinks: Vec::new(),
            }),
        }
    }

    pub fn mode(&self) -> RelayRouteMode {
        self.mode
    }

    /// The uplinks preferred in order; empty unless the mode is `prefer`.
    pub fn prefer(&self) -> &[String] {
        &self.prefer
    }

    /// Tell the owner how the tunnel is doing. Never waits: with the mode
    /// off, or the owner behind, the signal is dropped.
    pub fn signal(&self, signal: TunnelSignal) {
        if self.mode != RelayRouteMode::Off {
            let _ = self.tx.try_send(signal);
        }
    }

    /// The owner's latest report, with `rp_filter_strict` read now.
    pub fn report(&self) -> Report {
        let mut report = self
            .report
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if self.mode != RelayRouteMode::Off {
            report.rp_filter_strict = strict_rp_filter(&report.uplinks, read_rp_filter);
        }
        report
    }

    fn set_report(&self, report: Report) {
        *self
            .report
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = report;
    }
}

/// The uplink for the relay route. The first of `prefer` that holds a
/// default route and is not suspect wins, whatever its metric (its lowest
/// default route when it holds several); when none of them does, the
/// lowest-metric default route among the unlisted interfaces that are not
/// suspect. Ties go to the interface that sorts first, as in
/// [`NetState::lowest_default`]. A suspect uplink is never chosen, listed or
/// not. With `prefer` empty this is the lowest-metric rule alone.
pub fn choose<'a>(
    net: &'a NetState,
    prefer: &[String],
    suspect: impl Fn(&str) -> bool,
) -> Option<&'a DefaultRoute> {
    let lowest = |routes: &mut dyn Iterator<Item = &'a DefaultRoute>| {
        routes.min_by(|a, b| (a.metric, &a.dev).cmp(&(b.metric, &b.dev)))
    };
    prefer
        .iter()
        .filter(|dev| !suspect(dev))
        .find_map(|dev| lowest(&mut net.default_routes.iter().filter(|r| r.dev == **dev)))
        .or_else(|| {
            lowest(
                &mut net
                    .default_routes
                    .iter()
                    .filter(|r| !prefer.contains(&r.dev) && !suspect(&r.dev)),
            )
        })
}

/// Whether going from `old` to `new` changed interface `dev` itself: its
/// link, address or default routes. Host routes do not count, so the owner's
/// own writes never touch anything.
fn touched(old: &NetState, new: &NetState, dev: &str) -> bool {
    let defaults = |net: &NetState| {
        net.default_routes
            .iter()
            .filter(|r| r.dev == dev)
            .cloned()
            .collect::<Vec<_>>()
    };
    old.interface(dev) != new.interface(dev) || defaults(old) != defaults(new)
}

/// Every uplink but `except` with a default route and an address, in
/// [`choose`]'s order: the `prefer` list first, in its order, then the rest
/// lowest metric first.
fn candidates(net: &NetState, prefer: &[String], except: &str) -> Vec<(String, Ipv4Addr)> {
    let rank = |dev: &str| prefer.iter().position(|p| p == dev).unwrap_or(prefer.len());
    let mut routes: Vec<&DefaultRoute> = net.default_routes.iter().collect();
    routes.sort_by(|a, b| (rank(&a.dev), a.metric, &a.dev).cmp(&(rank(&b.dev), b.metric, &b.dev)));
    let mut out: Vec<(String, Ipv4Addr)> = Vec::new();
    for r in routes {
        if r.dev == except || out.iter().any(|(dev, _)| *dev == r.dev) {
            continue;
        }
        if let Some(src) = net.interface(&r.dev).and_then(|i| i.ipv4) {
            out.push((r.dev.clone(), src.addr));
        }
    }
    out
}

/// One change worth a tunnel event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Change {
    /// The route moved from one uplink to another; None is no route.
    Moved {
        from: Option<String>,
        to: Option<String>,
        reason: String,
    },
    /// A route sctl did not install holds the relay at metric 0.
    LeftAlone { relay: Ipv4Addr, route: HostRoute },
}

impl fmt::Display for Change {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Moved { from, to, reason } => write!(
                f,
                "{} -> {} ({reason})",
                from.as_deref().unwrap_or("none"),
                to.as_deref().unwrap_or("none")
            ),
            Self::LeftAlone { relay, route } => {
                write!(f, "left to {relay}/32")?;
                if let Some(via) = route.via {
                    write!(f, " via {via}")?;
                }
                write!(
                    f,
                    " dev {} proto {}, a route sctl did not install",
                    route.dev, route.protocol
                )
            }
        }
    }
}

/// What the owner does to the world, so tests can stand in for the kernel.
pub(crate) trait Uplinks {
    /// Whether the relay at `relay` answers a probe bound to `dev` and `src`.
    fn answers(
        &self,
        relay: SocketAddrV4,
        dev: &str,
        src: Ipv4Addr,
    ) -> impl Future<Output = bool> + Send;
    fn replace(&self, route: OwnedRoute) -> impl Future<Output = io::Result<()>> + Send;
    fn delete(&self, route: OwnedRoute) -> impl Future<Output = io::Result<()>> + Send;
    /// The interface the tunnel is registered over, if it is.
    fn tunnel_dev(&self) -> Option<String>;
}

/// Where sctl's route goes.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Held {
    dev: String,
    via: Option<Ipv4Addr>,
}

/// An uplink that did not answer the relay, waiting to be asked again.
#[derive(Clone, Copy, Debug)]
struct Suspect {
    /// Good probes in a row.
    good: u8,
    /// Probes since it became suspect: its place in the return schedule.
    probes: usize,
    /// When it is asked next.
    due: Instant,
}

/// What the owner remembers of an uplink that has been suspect, so that one
/// which keeps failing soon after it gets the route back waits longer each
/// time.
#[derive(Clone, Copy, Debug)]
struct Flaps {
    /// What its waits in the return schedule are multiplied by: 1, doubled
    /// by every quick relapse.
    stretch: u32,
    /// When the route last came back to it, until it fails again.
    returned: Option<Instant>,
}

/// The wait before probe number `probes` of a suspect whose waits are
/// multiplied by `stretch`: never above [`MAX_RETURN_WAIT`] once stretched.
fn return_wait(schedule: &[Duration], probes: usize, stretch: u32) -> Duration {
    let base = schedule
        .get(probes)
        .or(schedule.last())
        .copied()
        .unwrap_or(Duration::from_mins(15));
    base.saturating_mul(stretch).min(MAX_RETURN_WAIT).max(base)
}

/// The owner's state, fed one event at a time.
pub(crate) struct Owner<U> {
    uplinks: U,
    schedule: Vec<Duration>,
    /// The uplinks to take first, in order (`relay_route_prefer`).
    prefer: Vec<String>,
    relay: Option<SocketAddrV4>,
    net: Option<Arc<NetState>>,
    /// sctl's route as sctl last wrote it (or found it, once, at start).
    held: Option<Held>,
    adopted: bool,
    /// Uplinks that did not answer the relay.
    suspect: BTreeMap<String, Suspect>,
    /// Uplinks that have been suspect, and how they have fared since.
    flaps: BTreeMap<String, Flaps>,
    /// Tunnel failures in a row on the held uplink.
    failures: u8,
    /// The route someone else installed that the owner stands aside for.
    left_to: Option<HostRoute>,
    write_failing: bool,
}

impl<U: Uplinks> Owner<U> {
    pub(crate) fn new(uplinks: U, schedule: Vec<Duration>, prefer: Vec<String>) -> Self {
        Self {
            uplinks,
            schedule,
            prefer,
            relay: None,
            net: None,
            held: None,
            adopted: false,
            suspect: BTreeMap::new(),
            flaps: BTreeMap::new(),
            failures: 0,
            left_to: None,
            write_failing: false,
        }
    }

    pub(crate) fn report(&self, mode: RelayRouteMode) -> Report {
        let mut uplinks: Vec<String> = Vec::new();
        for route in self.net.iter().flat_map(|net| &net.default_routes) {
            if !uplinks.contains(&route.dev) {
                uplinks.push(route.dev.clone());
            }
        }
        Report {
            mode,
            prefer: self.prefer.clone(),
            dev: self.held.as_ref().map(|h| h.dev.clone()),
            via: self.held.as_ref().and_then(|h| h.via),
            suspect: self.suspect.keys().cloned().collect(),
            rp_filter_strict: Vec::new(),
            uplinks,
        }
    }

    /// When the next suspect is due to be asked; None outside the degraded
    /// state.
    pub(crate) fn next_probe(&self) -> Option<Instant> {
        self.suspect.values().map(|s| s.due).min()
    }

    /// Why the route goes where [`choose`] puts it when nothing else says.
    fn default_reason(&self) -> String {
        if self.prefer.is_empty() {
            "lowest-metric default route".to_string()
        } else {
            "preferred uplink".to_string()
        }
    }

    /// A new network state.
    pub(crate) async fn on_net(&mut self, net: Arc<NetState>) -> Vec<Change> {
        let mut reason = self.default_reason();
        if let Some(old) = self.net.take() {
            let back: Vec<String> = self
                .suspect
                .keys()
                .filter(|dev| touched(&old, &net, dev))
                .cloned()
                .collect();
            if !back.is_empty() {
                for dev in &back {
                    self.suspect.remove(dev);
                }
                reason = format!("{} changed", back.join(", "));
            }
        }
        self.net = Some(net);
        self.reconcile(&[], reason).await
    }

    /// A signal from the tunnel.
    pub(crate) async fn on_signal(&mut self, signal: TunnelSignal) -> Vec<Change> {
        match signal {
            TunnelSignal::Registered { relay, local } => {
                if let Some(old) = self.relay.filter(|old| old.ip() != relay.ip()) {
                    self.forget(*old.ip()).await;
                }
                self.relay = Some(relay);
                if self.on_held(local) {
                    self.failures = 0;
                }
                // A registration proves its uplink reaches the relay better
                // than any probe: it is no longer suspect.
                let over = self.uplink_with(local);
                let mut reason = self.default_reason();
                if let Some(dev) = over.as_ref().filter(|dev| self.suspect.contains_key(*dev)) {
                    self.suspect.remove(dev);
                    reason = format!("the tunnel reached the relay over {dev}");
                }
                self.reconcile(over.as_slice(), reason).await
            }
            TunnelSignal::Failed { from, relay } => {
                // A restarted owner hears no registration while the uplink
                // its earlier run chose is dead, but that run's route names
                // the relay: failing to reach it there counts.
                let mut changes = Vec::new();
                if let Some(relay) =
                    relay.filter(|r| self.relay.is_none() && self.has_own_route_to(*r.ip()))
                {
                    self.relay = Some(relay);
                    let reason = self.default_reason();
                    changes = self.reconcile(&[], reason).await;
                }
                if !from.is_some_and(|from| self.on_held(from)) {
                    return changes;
                }
                self.failures += 1;
                if self.failures < FAILURES_TO_FAIL_OVER {
                    return changes;
                }
                self.failures = 0;
                changes.extend(self.fail_over().await);
                changes
            }
        }
    }

    /// Whether sctl's own route to `ip` is in place, as an earlier run left it.
    fn has_own_route_to(&self, ip: Ipv4Addr) -> bool {
        self.net.as_ref().is_some_and(|net| {
            net.host_routes_to(ip)
                .any(|r| r.metric == 0 && r.protocol == RTPROT_SCTL)
        })
    }

    /// Ask every suspect uplink that is due; one that answered twice in a
    /// row gets its place back.
    pub(crate) async fn probe_suspects(&mut self) -> Vec<Change> {
        let now = Instant::now();
        let due: Vec<String> = self
            .suspect
            .iter()
            .filter(|(_, s)| s.due <= now)
            .map(|(dev, _)| dev.clone())
            .collect();
        let (Some(net), Some(relay)) = (self.net.clone(), self.relay) else {
            // Nothing to ask with yet: each waits its next turn.
            for dev in &due {
                self.count_probe(dev, false);
            }
            return Vec::new();
        };
        let mut back = Vec::new();
        for dev in due {
            let answered = self.asks(&net, relay, &dev).await;
            if self.count_probe(&dev, answered) {
                back.push(dev);
            }
        }
        for dev in &back {
            self.suspect.remove(dev);
        }
        if back.is_empty() {
            return Vec::new();
        }
        let reason = format!("{} answers the relay again", back.join(", "));
        self.reconcile(&back, reason).await
    }

    /// Count one probe of suspect `dev` and set when it is asked next. True
    /// once it has answered twice in a row.
    fn count_probe(&mut self, dev: &str, answered: bool) -> bool {
        let stretch = self.flaps.get(dev).map_or(1, |f| f.stretch);
        let Some(suspect) = self.suspect.get_mut(dev) else {
            return false;
        };
        suspect.good = if answered {
            suspect.good.saturating_add(1)
        } else {
            0
        };
        suspect.probes += 1;
        suspect.due = Instant::now() + return_wait(&self.schedule, suspect.probes, stretch);
        suspect.good >= PROBES_TO_RETURN
    }

    /// The uplink holding `addr`: one with a default route whose address it
    /// is.
    fn uplink_with(&self, addr: Ipv4Addr) -> Option<String> {
        let net = self.net.as_ref()?;
        net.default_routes
            .iter()
            .map(|r| r.dev.as_str())
            .find(|dev| {
                net.interface(dev)
                    .and_then(|i| i.ipv4)
                    .is_some_and(|cidr| cidr.addr == addr)
            })
            .map(ToString::to_string)
    }

    /// Whether `addr` is the held uplink's address.
    fn on_held(&self, addr: Ipv4Addr) -> bool {
        let (Some(held), Some(net)) = (&self.held, &self.net) else {
            return false;
        };
        net.interface(&held.dev)
            .and_then(|i| i.ipv4)
            .is_some_and(|cidr| cidr.addr == addr)
    }

    /// Probe `dev` from its own address.
    async fn asks(&self, net: &NetState, relay: SocketAddrV4, dev: &str) -> bool {
        match net.interface(dev).and_then(|i| i.ipv4) {
            Some(src) => self.uplinks.answers(relay, dev, src.addr).await,
            None => false,
        }
    }

    /// Two failures in a row on the held uplink. The held uplink and every
    /// other one are asked in the same round, all at once, so they all see
    /// the relay as it is at that moment. The route moves only when the held
    /// uplink does not answer while another does: to the first of those in
    /// metric order. When the held uplink answers, the failures were the
    /// relay's, not the path's; when nothing answers, the relay itself is
    /// most likely down. Either way nothing moves and nothing becomes
    /// suspect, so a relay outage never sends a unit to LTE. With
    /// `relay_route_prefer` set, the listed uplinks come first, in order.
    async fn fail_over(&mut self) -> Vec<Change> {
        let (Some(net), Some(relay), Some(held)) =
            (self.net.clone(), self.relay, self.held.clone())
        else {
            return Vec::new();
        };
        let others = candidates(&net, &self.prefer, &held.dev);
        if others.is_empty() {
            info!(
                "relay route: the tunnel keeps failing over {} and there is no other uplink; \
                 the route stays",
                held.dev
            );
            return Vec::new();
        }
        let (held_answers, answers) = join(
            self.asks(&net, relay, &held.dev),
            join_all(
                others
                    .iter()
                    .map(|(dev, src)| self.uplinks.answers(relay, dev, *src)),
            ),
        )
        .await;
        if held_answers {
            info!(
                "relay route: the tunnel failed twice over {} but it answers the relay; \
                 the route stays",
                held.dev
            );
            return Vec::new();
        }
        let Some(first) = answers.iter().position(|&answered| answered) else {
            info!(
                "relay route: no uplink answers the relay, {} included; the relay is likely \
                 down and the route stays",
                held.dev
            );
            return Vec::new();
        };
        let dev = others[first].0.clone();
        self.suspect.remove(&dev);
        self.mark_suspect(&held.dev);
        for (quiet, _) in &others[..first] {
            self.mark_suspect(quiet);
        }
        let reason = format!("{}: no answer from the relay", held.dev);
        self.reconcile(&[dev], reason).await
    }

    /// Put the route where it belongs now. `trusted` uplinks have just
    /// answered the relay.
    async fn reconcile(&mut self, trusted: &[String], reason: String) -> Vec<Change> {
        let (Some(net), Some(relay)) = (self.net.clone(), self.relay) else {
            return Vec::new();
        };
        let relay_ip = *relay.ip();
        let ours = net
            .host_routes_to(relay_ip)
            .find(|r| r.metric == 0 && r.protocol == RTPROT_SCTL)
            .cloned();
        // A route left by an earlier run is sctl's to keep.
        if !self.adopted {
            self.adopted = true;
            if let Some(o) = &ours {
                self.held = Some(Held {
                    dev: o.dev.clone(),
                    via: o.via,
                });
            }
        }

        // A replace would overwrite someone else's route at metric 0.
        if let Some(other) = net
            .host_routes_to(relay_ip)
            .find(|r| r.metric == 0 && r.protocol != RTPROT_SCTL)
        {
            self.held = None;
            if self.left_to.as_ref() == Some(other) {
                return Vec::new();
            }
            self.left_to = Some(other.clone());
            return vec![Change::LeftAlone {
                relay: relay_ip,
                route: other.clone(),
            }];
        }
        self.left_to = None;

        // The best uplink that is not suspect and answers.
        let want = loop {
            let Some(want) =
                choose(&net, &self.prefer, |dev| self.suspect.contains_key(dev)).cloned()
            else {
                break None;
            };
            let proven = self.held.as_ref().is_some_and(|h| h.dev == want.dev)
                || trusted.contains(&want.dev)
                || self.uplinks.tunnel_dev().as_deref() == Some(want.dev.as_str());
            if proven || self.asks(&net, relay, &want.dev).await {
                break Some(want);
            }
            info!(
                "relay route: {} does not answer the relay; not moving there",
                want.dev
            );
            self.mark_suspect(&want.dev);
        };

        let Some(want) = want else {
            if ours.is_some() {
                let any = OwnedRoute {
                    dst: relay_ip,
                    via: None,
                    oif: 0,
                    metric: 0,
                    onlink: false,
                };
                if !self.write(Write::Delete(any)).await {
                    return Vec::new();
                }
            }
            return match self.held.take() {
                Some(held) => vec![Change::Moved {
                    from: Some(held.dev),
                    to: None,
                    reason: "no uplink left to hold it".into(),
                }],
                None => Vec::new(),
            };
        };

        let target = Held {
            dev: want.dev.clone(),
            via: want.gw,
        };
        let installed = ours
            .as_ref()
            .is_some_and(|o| o.dev == target.dev && o.via == target.via);
        if !installed {
            let Some(oif) = net.interface(&want.dev).map(|i| i.index) else {
                return Vec::new();
            };
            // A gateway the default route declares on the link must be
            // declared so here too.
            let route = OwnedRoute {
                dst: relay_ip,
                via: want.gw,
                oif,
                metric: 0,
                onlink: want.onlink,
            };
            if !self.write(Write::Replace(route)).await {
                return Vec::new();
            }
        }
        let from = self.held.replace(target).map(|h| h.dev);
        if from.as_deref() == Some(want.dev.as_str()) {
            return Vec::new();
        }
        self.failures = 0;
        // The route is back on an uplink that had been suspect: failing again
        // soon counts against it.
        if let Some(flaps) = self.flaps.get_mut(&want.dev) {
            flaps.returned = Some(Instant::now());
        }
        vec![Change::Moved {
            from,
            to: Some(want.dev),
            reason,
        }]
    }

    /// The relay moved to another address: remove sctl's route to the old one.
    async fn forget(&mut self, old: Ipv4Addr) {
        if self.held.take().is_some() {
            let any = OwnedRoute {
                dst: old,
                via: None,
                oif: 0,
                metric: 0,
                onlink: false,
            };
            self.write(Write::Delete(any)).await;
            info!("relay route: the relay is no longer at {old}; its route is removed");
        }
        self.failures = 0;
    }

    /// Mark `dev` suspect, to be asked first after the schedule's first
    /// wait. An uplink that fails again within [`RELAPSE_WINDOW`] of getting
    /// the route back waits twice as long as it did last time; one that kept
    /// the route for [`FORGIVE_AFTER`] starts over.
    fn mark_suspect(&mut self, dev: &str) {
        if self.suspect.contains_key(dev) {
            return;
        }
        let now = Instant::now();
        let flaps = self.flaps.entry(dev.to_string()).or_insert(Flaps {
            stretch: 1,
            returned: None,
        });
        if let Some(returned) = flaps.returned.take() {
            let kept = now.saturating_duration_since(returned);
            if kept < RELAPSE_WINDOW {
                flaps.stretch = flaps.stretch.saturating_mul(2).min(MAX_STRETCH);
                info!(
                    "relay route: {dev} failed again {}s after it got the route back; \
                     its waits to be asked again are now {}x",
                    kept.as_secs(),
                    flaps.stretch
                );
            } else if kept >= FORGIVE_AFTER {
                flaps.stretch = 1;
            }
        }
        let stretch = flaps.stretch;
        self.suspect.insert(
            dev.to_string(),
            Suspect {
                good: 0,
                probes: 0,
                due: now + return_wait(&self.schedule, 0, stretch),
            },
        );
    }

    /// Write one route; false when the kernel refused. A delete that finds
    /// nothing is done.
    async fn write(&mut self, op: Write) -> bool {
        let result = match op {
            Write::Replace(route) => self.uplinks.replace(route).await,
            Write::Delete(route) => match self.uplinks.delete(route).await {
                Err(e) if e.raw_os_error() == Some(libc::ESRCH) => Ok(()),
                other => other,
            },
        };
        match result {
            Ok(()) => {
                if self.write_failing {
                    info!("relay route: route writes work again");
                }
                self.write_failing = false;
                true
            }
            Err(e) => {
                if self.write_failing {
                    debug!("relay route: writing the route failed again ({e})");
                } else {
                    warn!(
                        "relay route: writing the route failed ({e}); retrying on the next change"
                    );
                }
                self.write_failing = true;
                false
            }
        }
    }
}

enum Write {
    Replace(OwnedRoute),
    Delete(OwnedRoute),
}

/// A TCP connection to `relay` that can only leave through `dev`, from `src`.
pub async fn connect_from(
    relay: SocketAddrV4,
    dev: &str,
    src: Ipv4Addr,
    timeout: Duration,
) -> io::Result<TcpStream> {
    let socket = TcpSocket::new_v4()?;
    source::bind_to_device(&socket, dev)?;
    socket.bind(SocketAddr::V4(SocketAddrV4::new(src, 0)))?;
    tokio::time::timeout(timeout, socket.connect(SocketAddr::V4(relay)))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "connect timed out"))?
}

/// The real uplinks: kernel routes, bound probes and the tunnel's stats.
struct Kernel {
    stats: Arc<TunnelStats>,
    probe: Probe,
    timing: Timing,
}

impl Uplinks for Kernel {
    async fn answers(&self, relay: SocketAddrV4, dev: &str, src: Ipv4Addr) -> bool {
        match self.probe.ask(relay, dev, src, &self.timing).await {
            Ok(()) => {
                debug!("relay route: {relay} answers over {dev}");
                true
            }
            Err(e) => {
                debug!("relay route: no answer from {relay} over {dev} ({e})");
                false
            }
        }
    }

    async fn replace(&self, route: OwnedRoute) -> io::Result<()> {
        route::replace(&route).await
    }

    async fn delete(&self, route: OwnedRoute) -> io::Result<()> {
        route::delete(&route).await
    }

    fn tunnel_dev(&self) -> Option<String> {
        if !self.stats.connected.load(Ordering::Relaxed) {
            return None;
        }
        self.stats.path().and_then(|p| p.dev)
    }
}

/// Keep the relay route until cancelled, recording each move in the tunnel
/// events. `probe` is how an uplink is asked whether it reaches the relay.
/// Parks when the mode is off. Cancelling leaves the route in place.
pub async fn run(
    handle: Arc<RelayRoute>,
    mut net: NetWatch,
    stats: Arc<TunnelStats>,
    timing: Timing,
    probe: Probe,
) {
    if handle.mode() == RelayRouteMode::Off {
        return std::future::pending().await;
    }
    // A restarted owner takes the queue back from the one that panicked.
    let mut signals = handle.rx.lock().await;
    let schedule = timing.return_schedule.clone();
    let mut owner = Owner::new(
        Kernel {
            stats: stats.clone(),
            probe,
            timing,
        },
        schedule,
        handle.prefer().to_vec(),
    );
    if handle.prefer().is_empty() {
        info!("relay route: keeping the relay's route on the best uplink that answers it");
    } else {
        info!(
            "relay route: keeping the relay's route on the first of {} that answers it, \
             then the best of the rest",
            handle.prefer().join(", ")
        );
    }
    // Uplinks with strict reverse-path filtering, as last logged.
    let mut strict: Vec<String> = Vec::new();
    loop {
        let wake = owner.next_probe();
        let changes = tokio::select! {
            latest = next_state(Some(&mut net)) => owner.on_net(latest).await,
            Some(signal) = signals.recv() => owner.on_signal(signal).await,
            () = tokio::time::sleep_until(wake.unwrap_or_else(Instant::now)), if wake.is_some() => {
                owner.probe_suspects().await
            }
        };
        for change in changes {
            info!("relay route: {change}");
            stats
                .push_event(TunnelEventType::RelayRoute, change.to_string())
                .await;
        }
        handle.set_report(owner.report(handle.mode()));
        let now = handle.report().rp_filter_strict;
        if now != strict {
            if now.is_empty() {
                info!("relay route: reverse-path filtering is no longer strict on any uplink");
            } else {
                warn!(
                    "relay route: rp_filter is 1 (strict) on {}: the answer to a probe over an \
                     uplink the route does not use is dropped there, so it can never be proven; \
                     set net.ipv4.conf.all.rp_filter = 2",
                    now.join(", ")
                );
            }
            strict = now;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Mutex;

    use super::super::{Interface, Ipv4Cidr};
    use super::*;

    const RELAY: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(174, 138, 114, 209), 443);
    const ETH1: Ipv4Addr = Ipv4Addr::new(10, 42, 0, 7);
    const ETH1_GW: Ipv4Addr = Ipv4Addr::new(10, 42, 0, 1);
    const ETH2: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 50);
    const LTE: Ipv4Addr = Ipv4Addr::new(10, 180, 41, 231);
    const LTE_GW: Ipv4Addr = Ipv4Addr::new(10, 180, 41, 232);

    fn iface(name: &str, index: u32, addr: Ipv4Addr, metric: Option<u32>) -> Interface {
        Interface {
            name: name.into(),
            index,
            operstate: "up",
            carrier: Some(true),
            ipv4: Some(Ipv4Cidr { addr, prefix: 24 }),
            default_metric: metric,
            master: None,
        }
    }

    fn default(dev: &str, gw: Ipv4Addr, metric: u32) -> DefaultRoute {
        DefaultRoute {
            dev: dev.into(),
            gw: Some(gw),
            metric,
            onlink: false,
        }
    }

    fn host(dev: &str, via: Ipv4Addr, metric: u32, protocol: u8) -> HostRoute {
        HostRoute {
            dst: *RELAY.ip(),
            dev: dev.into(),
            via: Some(via),
            metric,
            protocol,
        }
    }

    /// A travel router: the wire at metric 10, LTE at 40, and netifd's pin
    /// for the relay on LTE.
    fn travel_router() -> NetState {
        NetState {
            boot: 1,
            seq: 1,
            interfaces: vec![
                iface("eth1", 3, ETH1, Some(10)),
                iface("wwan0", 7, LTE, Some(40)),
            ],
            default_routes: vec![default("eth1", ETH1_GW, 10), default("wwan0", LTE_GW, 40)],
            host_routes: vec![host("wwan0", LTE_GW, 40, 4)],
        }
    }

    /// The kernel and the relay as a test sees them: the owner's writes land
    /// in `net`, and every uplink answers unless it is `dead`.
    struct Fake {
        net: Mutex<NetState>,
        dead: Mutex<BTreeSet<String>>,
        probes: Mutex<Vec<String>>,
        writes: Mutex<Vec<String>>,
        tunnel: Mutex<Option<String>>,
    }

    impl Fake {
        fn new(net: NetState) -> Arc<Self> {
            Arc::new(Self {
                net: Mutex::new(net),
                dead: Mutex::new(BTreeSet::new()),
                probes: Mutex::new(Vec::new()),
                writes: Mutex::new(Vec::new()),
                tunnel: Mutex::new(None),
            })
        }
        fn state(&self) -> Arc<NetState> {
            Arc::new(self.net.lock().unwrap().clone())
        }
        fn edit(&self, f: impl FnOnce(&mut NetState)) {
            f(&mut self.net.lock().unwrap());
        }
        fn kill(&self, dev: &str) {
            self.dead.lock().unwrap().insert(dev.into());
        }
        fn revive(&self, dev: &str) {
            self.dead.lock().unwrap().remove(dev);
        }
        fn tunnel_on(&self, dev: Option<&str>) {
            *self.tunnel.lock().unwrap() = dev.map(Into::into);
        }
        fn probes(&self) -> Vec<String> {
            std::mem::take(&mut self.probes.lock().unwrap())
        }
        fn writes(&self) -> Vec<String> {
            std::mem::take(&mut self.writes.lock().unwrap())
        }
        fn ours(&self) -> Vec<HostRoute> {
            self.net
                .lock()
                .unwrap()
                .host_routes
                .iter()
                .filter(|r| r.protocol == RTPROT_SCTL)
                .cloned()
                .collect()
        }
    }

    impl Uplinks for Arc<Fake> {
        async fn answers(&self, relay: SocketAddrV4, dev: &str, src: Ipv4Addr) -> bool {
            assert_eq!(relay, RELAY);
            let net = self.net.lock().unwrap().clone();
            assert_eq!(
                net.interface(dev).and_then(|i| i.ipv4).map(|c| c.addr),
                Some(src),
                "a probe leaves from the uplink's own address"
            );
            self.probes.lock().unwrap().push(dev.into());
            !self.dead.lock().unwrap().contains(dev)
        }

        async fn replace(&self, route: OwnedRoute) -> io::Result<()> {
            let mut net = self.net.lock().unwrap();
            let dev = net
                .interfaces
                .iter()
                .find(|i| i.index == route.oif)
                .map(|i| i.name.clone())
                .expect("a known interface");
            assert!(
                net.host_routes.iter().all(|r| r.dst != route.dst
                    || r.metric != route.metric
                    || r.protocol == RTPROT_SCTL),
                "never replaces a route sctl did not install"
            );
            net.host_routes
                .retain(|r| !(r.dst == route.dst && r.metric == route.metric));
            net.host_routes.push(HostRoute {
                dst: route.dst,
                dev: dev.clone(),
                via: route.via,
                metric: route.metric,
                protocol: RTPROT_SCTL,
            });
            self.writes.lock().unwrap().push(format!(
                "replace {} via {} dev {dev} metric {}{}",
                route.dst,
                route.via.map_or("-".into(), |v| v.to_string()),
                route.metric,
                if route.onlink { " onlink" } else { "" }
            ));
            Ok(())
        }

        async fn delete(&self, route: OwnedRoute) -> io::Result<()> {
            let mut net = self.net.lock().unwrap();
            let before = net.host_routes.len();
            net.host_routes
                .retain(|r| !(r.dst == route.dst && r.protocol == RTPROT_SCTL));
            self.writes
                .lock()
                .unwrap()
                .push(format!("delete {}", route.dst));
            if net.host_routes.len() == before {
                return Err(io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        }

        fn tunnel_dev(&self) -> Option<String> {
            self.tunnel.lock().unwrap().clone()
        }
    }

    fn schedule() -> Vec<Duration> {
        Timing::default().return_schedule
    }

    fn registered(local: Ipv4Addr) -> TunnelSignal {
        TunnelSignal::Registered {
            relay: RELAY,
            local,
        }
    }

    fn failed(from: Ipv4Addr) -> TunnelSignal {
        TunnelSignal::Failed {
            from: Some(from),
            relay: Some(RELAY),
        }
    }

    fn moved(from: Option<&str>, to: Option<&str>, reason: &str) -> Vec<Change> {
        vec![Change::Moved {
            from: from.map(Into::into),
            to: to.map(Into::into),
            reason: reason.into(),
        }]
    }

    /// An owner that has installed its route on eth1, with the tunnel there.
    async fn settled(fake: &Arc<Fake>) -> Owner<Arc<Fake>> {
        let mut owner = Owner::new(fake.clone(), schedule(), Vec::new());
        fake.tunnel_on(Some("eth1"));
        owner.on_net(fake.state()).await;
        owner.on_signal(registered(ETH1)).await;
        owner.on_net(fake.state()).await;
        assert_eq!(
            owner.report(RelayRouteMode::FollowDefault).dev.as_deref(),
            Some("eth1")
        );
        fake.probes();
        fake.writes();
        owner
    }

    /// Run the next probe round once it is due, on the paused clock.
    async fn next_round(owner: &mut Owner<Arc<Fake>>) -> Vec<Change> {
        tokio::time::sleep_until(owner.next_probe().expect("a round is due")).await;
        owner.probe_suspects().await
    }

    /// Minutes until the next probe round, rounded up.
    fn minutes_to_next_round(owner: &Owner<Arc<Fake>>) -> u64 {
        let due = owner.next_probe().expect("a round is due") - Instant::now();
        due.as_secs().div_ceil(60)
    }

    /// An owner that failed over from eth1 to wwan0, with the tunnel there.
    async fn failed_over(fake: &Arc<Fake>) -> Owner<Arc<Fake>> {
        let mut owner = settled(fake).await;
        fake.kill("eth1");
        fake.tunnel_on(None);
        owner.on_signal(failed(ETH1)).await;
        owner.on_signal(failed(ETH1)).await;
        owner.on_net(fake.state()).await;
        fake.tunnel_on(Some("wwan0"));
        owner.on_signal(registered(LTE)).await;
        assert_eq!(
            owner.report(RelayRouteMode::FollowDefault).dev.as_deref(),
            Some("wwan0")
        );
        fake.probes();
        fake.writes();
        owner
    }

    #[test]
    fn choose_takes_the_lowest_metric_uplink_that_is_not_suspect() {
        let net = travel_router();
        assert_eq!(choose(&net, &[], |_| false).unwrap().dev, "eth1");
        assert_eq!(choose(&net, &[], |d| d == "eth1").unwrap().dev, "wwan0");
        assert!(choose(&net, &[], |_| true).is_none());
        assert!(choose(&NetState::default(), &[], |_| false).is_none());
    }

    #[test]
    fn choose_breaks_ties_by_name_whatever_the_order() {
        let mut net = travel_router();
        net.default_routes = vec![
            default("wwan0", LTE_GW, 10),
            default("eth1", ETH1_GW, 10),
            default("eth0", ETH1_GW, 20),
        ];
        assert_eq!(choose(&net, &[], |_| false).unwrap().dev, "eth1");
        assert_eq!(choose(&net, &[], |d| d == "eth1").unwrap().dev, "wwan0");
        assert_eq!(choose(&net, &[], |d| d != "eth0").unwrap().dev, "eth0");
    }

    /// A WE826: LTE at metric 0 (quectel-CM's route) and 5, the wire at 3.
    fn we826() -> NetState {
        NetState {
            boot: 1,
            seq: 1,
            interfaces: vec![
                iface("eth0", 2, ETH1, Some(3)),
                iface("usb0", 5, LTE, Some(0)),
                iface("eth1", 3, ETH2, Some(50)),
            ],
            default_routes: vec![
                default("usb0", LTE_GW, 0),
                default("eth0", ETH1_GW, 3),
                default("usb0", LTE_GW, 5),
                default("eth1", ETH1_GW, 50),
            ],
            host_routes: vec![],
        }
    }

    fn prefer(devs: &[&str]) -> Vec<String> {
        devs.iter().map(|d| (*d).to_string()).collect()
    }

    #[test]
    fn choose_takes_the_first_listed_uplink_whatever_its_metric() {
        let net = we826();
        let chosen = choose(&net, &prefer(&["eth0", "usb0"]), |_| false).unwrap();
        assert_eq!((chosen.dev.as_str(), chosen.metric), ("eth0", 3));
        // A listed uplink with several default routes: its lowest.
        let chosen = choose(&net, &prefer(&["usb0", "eth0"]), |_| false).unwrap();
        assert_eq!((chosen.dev.as_str(), chosen.metric), ("usb0", 0));
        // Listed, but without a default route: the next listed one.
        let chosen = choose(&net, &prefer(&["wlan0", "eth1", "eth0"]), |_| false).unwrap();
        assert_eq!(chosen.dev, "eth1");
    }

    #[test]
    fn choose_skips_a_listed_suspect_for_the_next_listed() {
        let net = we826();
        let list = prefer(&["eth0", "usb0"]);
        assert_eq!(choose(&net, &list, |d| d == "eth0").unwrap().dev, "usb0");
        // Every listed one suspect: the unlisted rest by metric, never a
        // listed suspect.
        assert_eq!(
            choose(&net, &list, |d| d == "eth0" || d == "usb0")
                .unwrap()
                .dev,
            "eth1"
        );
        assert!(choose(&net, &list, |_| true).is_none());
    }

    #[test]
    fn choose_falls_back_to_the_unlisted_by_metric() {
        let mut net = we826();
        net.default_routes.retain(|r| r.dev != "eth0");
        let list = prefer(&["eth0"]);
        assert_eq!(choose(&net, &list, |_| false).unwrap().dev, "usb0");
        assert_eq!(choose(&net, &list, |d| d == "usb0").unwrap().dev, "eth1");
        assert!(choose(&NetState::default(), &list, |_| false).is_none());
    }

    #[test]
    fn choose_with_an_empty_list_is_the_lowest_metric_rule() {
        let net = we826();
        for (suspect, want) in [
            (Vec::new(), Some("usb0")),
            (vec!["usb0"], Some("eth0")),
            (vec!["usb0", "eth0"], Some("eth1")),
            (vec!["usb0", "eth0", "eth1"], None),
        ] {
            let by_metric = choose(&net, &[], |d| suspect.contains(&d)).map(|r| r.dev.as_str());
            assert_eq!(by_metric, want, "{suspect:?}");
        }
        let travel = travel_router();
        assert_eq!(choose(&travel, &[], |_| false).unwrap().dev, "eth1");
    }

    #[test]
    fn candidates_follow_the_list_then_the_metric() {
        let net = we826();
        let names = |list: &[&str], except: &str| -> Vec<String> {
            candidates(&net, &prefer(list), except)
                .into_iter()
                .map(|(dev, _)| dev)
                .collect()
        };
        assert_eq!(names(&[], "none"), ["usb0", "eth0", "eth1"]);
        assert_eq!(names(&["eth0", "usb0"], "none"), ["eth0", "usb0", "eth1"]);
        assert_eq!(names(&["eth1"], "eth1"), ["usb0", "eth0"]);
        assert_eq!(names(&["eth0", "usb0"], "eth0"), ["usb0", "eth1"]);
    }

    /// An owner on a WE826 that prefers the wire: the route lands there,
    /// LTE's metric 0 notwithstanding, and the tunnel follows it.
    #[tokio::test]
    async fn a_preferred_wire_holds_the_route_over_a_lower_metric_lte() {
        let fake = Fake::new(we826());
        let mut owner = Owner::new(fake.clone(), schedule(), prefer(&["eth0", "usb0"]));
        fake.tunnel_on(Some("usb0"));
        owner.on_net(fake.state()).await;
        assert_eq!(
            owner.on_signal(registered(LTE)).await,
            moved(None, Some("eth0"), "preferred uplink")
        );
        assert_eq!(
            fake.probes(),
            ["eth0"],
            "the wire is asked before the route moves"
        );
        assert_eq!(
            fake.writes(),
            [format!(
                "replace {} via {ETH1_GW} dev eth0 metric 0",
                RELAY.ip()
            )]
        );
        let report = owner.report(RelayRouteMode::Prefer);
        assert_eq!(report.prefer, ["eth0", "usb0"]);
        assert_eq!(report.dev.as_deref(), Some("eth0"));
        assert_eq!(
            serde_json::to_value(&report).unwrap()["prefer"],
            serde_json::json!(["eth0", "usb0"])
        );

        // The wire's internet dies: LTE, listed next, takes over; the wire
        // returns when it answers again.
        fake.kill("eth0");
        fake.tunnel_on(None);
        owner.on_signal(failed(ETH1)).await;
        assert_eq!(
            owner.on_signal(failed(ETH1)).await,
            moved(Some("eth0"), Some("usb0"), "eth0: no answer from the relay")
        );
        assert_eq!(owner.report(RelayRouteMode::Prefer).suspect, ["eth0"]);
        fake.revive("eth0");
        fake.tunnel_on(Some("usb0"));
        next_round(&mut owner).await;
        assert_eq!(
            next_round(&mut owner).await,
            moved(Some("usb0"), Some("eth0"), "eth0 answers the relay again")
        );
    }

    #[test]
    fn a_touch_is_a_link_address_or_default_route_change_never_a_host_route() {
        let old = travel_router();
        let touches = |edit: &dyn Fn(&mut NetState)| {
            let mut new = old.clone();
            edit(&mut new);
            touched(&old, &new, "eth1")
        };
        assert!(!touches(&|_| {}));
        assert!(touches(&|n| n.interfaces[0].carrier = Some(false)));
        assert!(touches(&|n| n.interfaces[0].operstate = "down"));
        assert!(touches(&|n| n.interfaces[0].ipv4 = None));
        assert!(touches(&|n| n.interfaces[0].index = 30));
        assert!(touches(&|n| n.default_routes[0].gw = Some(ETH2)));
        assert!(touches(&|n| {
            n.default_routes.remove(0);
            n.interfaces[0].default_metric = None;
        }));
        // The owner's own route and netifd's pin are host routes.
        assert!(!touches(&|n| n.host_routes.push(host(
            "eth1",
            ETH1_GW,
            0,
            RTPROT_SCTL
        ))));
        assert!(!touches(&|n| n.host_routes.clear()));
        // Another uplink's change is not this one's.
        assert!(!touches(&|n| n.interfaces[1].carrier = Some(false)));
    }

    #[tokio::test]
    async fn nothing_is_written_before_the_tunnel_names_the_relay() {
        let fake = Fake::new(travel_router());
        let mut owner = Owner::new(fake.clone(), schedule(), Vec::new());
        assert!(owner.on_net(fake.state()).await.is_empty());
        assert!(fake.writes().is_empty());
        assert!(fake.probes().is_empty());
        let empty = owner
            .on_signal(TunnelSignal::Failed {
                from: None,
                relay: None,
            })
            .await;
        assert!(empty.is_empty());
        // A failed dial names the relay, but sctl holds no route to it: the
        // relay is named by a registration.
        for _ in 0..3 {
            assert!(owner.on_signal(failed(ETH1)).await.is_empty());
        }
        assert!(fake.writes().is_empty());
        assert!(fake.probes().is_empty());
    }

    #[tokio::test]
    async fn the_first_registration_puts_the_route_on_the_lowest_metric_uplink() {
        let fake = Fake::new(travel_router());
        let mut owner = Owner::new(fake.clone(), schedule(), Vec::new());
        owner.on_net(fake.state()).await;
        // The tunnel came up over netifd's pin on LTE.
        fake.tunnel_on(Some("wwan0"));
        let changes = owner.on_signal(registered(LTE)).await;
        assert_eq!(
            changes,
            moved(None, Some("eth1"), "lowest-metric default route")
        );
        assert_eq!(
            changes[0].to_string(),
            "none -> eth1 (lowest-metric default route)"
        );
        assert_eq!(
            fake.probes(),
            ["eth1"],
            "the wire is asked before the tunnel moves"
        );
        assert_eq!(
            fake.writes(),
            ["replace 174.138.114.209 via 10.42.0.1 dev eth1 metric 0"]
        );
        // netifd's pin stays, outranked.
        assert!(fake
            .state()
            .host_routes
            .contains(&host("wwan0", LTE_GW, 40, 4)));

        // The route's own event changes nothing.
        assert!(owner.on_net(fake.state()).await.is_empty());
        assert!(fake.writes().is_empty());
        let report = owner.report(RelayRouteMode::FollowDefault);
        assert_eq!(
            serde_json::to_value(&report).unwrap(),
            serde_json::json!({
                "mode": "follow_default",
                "dev": "eth1",
                "via": "10.42.0.1",
                "suspect": [],
                "rp_filter_strict": []
            })
        );
    }

    #[tokio::test]
    async fn a_route_through_an_onlink_gateway_is_written_onlink() {
        let mut net = travel_router();
        // The wire's gateway lies outside its prefix:
        // `default via 10.9.9.1 dev eth1 onlink`.
        net.default_routes[0].gw = Some(Ipv4Addr::new(10, 9, 9, 1));
        net.default_routes[0].onlink = true;
        let fake = Fake::new(net);
        let mut owner = Owner::new(fake.clone(), schedule(), Vec::new());
        fake.tunnel_on(Some("eth1"));
        owner.on_net(fake.state()).await;
        owner.on_signal(registered(ETH1)).await;
        assert_eq!(
            fake.writes(),
            ["replace 174.138.114.209 via 10.9.9.1 dev eth1 metric 0 onlink"]
        );
        // LTE's gateway is in its prefix: no flag.
        fake.kill("eth1");
        fake.tunnel_on(None);
        owner.on_signal(failed(ETH1)).await;
        owner.on_signal(failed(ETH1)).await;
        assert_eq!(
            fake.writes(),
            ["replace 174.138.114.209 via 10.180.41.232 dev wwan0 metric 0"]
        );
    }

    #[tokio::test]
    async fn a_tunnel_already_on_the_best_uplink_is_not_probed() {
        let fake = Fake::new(travel_router());
        let mut owner = Owner::new(fake.clone(), schedule(), Vec::new());
        fake.tunnel_on(Some("eth1"));
        owner.on_net(fake.state()).await;
        owner.on_signal(registered(ETH1)).await;
        assert!(fake.probes().is_empty());
        assert_eq!(fake.ours().len(), 1);
    }

    #[tokio::test]
    async fn a_wire_that_does_not_answer_is_not_given_the_route() {
        let fake = Fake::new(travel_router());
        fake.kill("eth1");
        let mut owner = Owner::new(fake.clone(), schedule(), Vec::new());
        fake.tunnel_on(Some("wwan0"));
        owner.on_net(fake.state()).await;
        let changes = owner.on_signal(registered(LTE)).await;
        assert_eq!(
            changes,
            moved(None, Some("wwan0"), "lowest-metric default route")
        );
        assert_eq!(fake.probes(), ["eth1"]);
        assert_eq!(fake.ours(), [host("wwan0", LTE_GW, 0, RTPROT_SCTL)]);
        assert_eq!(
            owner.report(RelayRouteMode::FollowDefault).suspect,
            ["eth1"]
        );
        assert!(owner.next_probe().is_some());
    }

    #[tokio::test]
    async fn a_route_left_by_an_earlier_run_is_kept_without_a_write() {
        let mut net = travel_router();
        net.host_routes.push(host("eth1", ETH1_GW, 0, RTPROT_SCTL));
        let fake = Fake::new(net);
        let mut owner = Owner::new(fake.clone(), schedule(), Vec::new());
        owner.on_net(fake.state()).await;
        assert!(owner.on_signal(registered(ETH1)).await.is_empty());
        assert!(fake.writes().is_empty());
        assert!(fake.probes().is_empty());
        assert_eq!(
            owner.report(RelayRouteMode::FollowDefault).dev.as_deref(),
            Some("eth1")
        );
    }

    #[tokio::test]
    async fn a_restarted_owner_fails_over_from_the_route_it_left_on_a_dead_uplink() {
        // The earlier run's route holds the relay on eth1, whose internet
        // died, so this run's tunnel never registers.
        let mut net = travel_router();
        net.host_routes.push(host("eth1", ETH1_GW, 0, RTPROT_SCTL));
        let fake = Fake::new(net);
        fake.kill("eth1");
        let mut owner = Owner::new(fake.clone(), schedule(), Vec::new());
        owner.on_net(fake.state()).await;
        assert!(owner.on_signal(failed(ETH1)).await.is_empty());
        assert!(fake.writes().is_empty(), "the route is adopted as it is");
        assert_eq!(
            owner.report(RelayRouteMode::FollowDefault).dev.as_deref(),
            Some("eth1")
        );
        let changes = owner.on_signal(failed(ETH1)).await;
        assert_eq!(
            changes,
            moved(
                Some("eth1"),
                Some("wwan0"),
                "eth1: no answer from the relay"
            )
        );
        assert_eq!(fake.ours(), [host("wwan0", LTE_GW, 0, RTPROT_SCTL)]);
    }

    #[tokio::test]
    async fn a_route_someone_else_installed_at_metric_0_is_left_alone() {
        let mut net = travel_router();
        // A hand-added pin, `ip route add` (protocol boot).
        net.host_routes.push(host("eth1", ETH1_GW, 0, 3));
        let fake = Fake::new(net);
        let mut owner = Owner::new(fake.clone(), schedule(), Vec::new());
        owner.on_net(fake.state()).await;
        let changes = owner.on_signal(registered(ETH1)).await;
        assert_eq!(changes.len(), 1);
        assert_eq!(
            changes[0].to_string(),
            "left to 174.138.114.209/32 via 10.42.0.1 dev eth1 proto 3, a route sctl did not install"
        );
        assert!(fake.writes().is_empty());
        // Said once.
        assert!(owner.on_net(fake.state()).await.is_empty());
        assert!(owner.on_signal(failed(ETH1)).await.is_empty());
        assert!(owner.on_signal(failed(ETH1)).await.is_empty());
        assert!(fake.writes().is_empty());
        assert!(fake.probes().is_empty());
        assert_eq!(owner.report(RelayRouteMode::FollowDefault).dev, None);

        // Once it is gone, the owner takes over.
        fake.edit(|n| n.host_routes.retain(|r| r.protocol != 3));
        let changes = owner.on_net(fake.state()).await;
        assert_eq!(
            changes,
            moved(None, Some("eth1"), "lowest-metric default route")
        );
    }

    #[tokio::test]
    async fn one_failure_between_registrations_does_not_move() {
        let fake = Fake::new(travel_router());
        let mut owner = settled(&fake).await;
        for _ in 0..3 {
            assert!(owner.on_signal(failed(ETH1)).await.is_empty());
            assert!(owner.on_signal(registered(ETH1)).await.is_empty());
        }
        assert!(fake.probes().is_empty());
        assert!(fake.writes().is_empty());
    }

    #[tokio::test]
    async fn failures_from_another_uplink_or_nowhere_do_not_count() {
        let fake = Fake::new(travel_router());
        let mut owner = settled(&fake).await;
        for _ in 0..4 {
            assert!(owner.on_signal(failed(LTE)).await.is_empty());
            assert!(owner
                .on_signal(TunnelSignal::Failed {
                    from: None,
                    relay: Some(RELAY),
                })
                .await
                .is_empty());
        }
        assert!(fake.probes().is_empty());
    }

    #[tokio::test]
    async fn two_failures_in_a_row_move_to_the_first_uplink_that_answers() {
        let mut net = travel_router();
        net.interfaces.insert(1, iface("eth2", 4, ETH2, Some(20)));
        net.default_routes
            .insert(1, default("eth2", Ipv4Addr::new(192, 168, 1, 1), 20));
        let fake = Fake::new(net);
        let mut owner = settled(&fake).await;
        fake.kill("eth1");
        fake.kill("eth2");
        assert!(owner.on_signal(failed(ETH1)).await.is_empty());
        let changes = owner.on_signal(failed(ETH1)).await;
        assert_eq!(
            changes,
            moved(
                Some("eth1"),
                Some("wwan0"),
                "eth1: no answer from the relay"
            )
        );
        assert_eq!(
            changes[0].to_string(),
            "eth1 -> wwan0 (eth1: no answer from the relay)"
        );
        assert_eq!(
            fake.probes(),
            ["eth1", "eth2", "wwan0"],
            "the held uplink too, then the others in metric order"
        );
        assert_eq!(
            fake.writes(),
            ["replace 174.138.114.209 via 10.180.41.232 dev wwan0 metric 0"]
        );
        let report = owner.report(RelayRouteMode::FollowDefault);
        assert_eq!(
            report.suspect,
            ["eth1", "eth2"],
            "and the uplink that was silent too"
        );
        assert_eq!(report.via, Some(LTE_GW));
        let due = owner.next_probe().unwrap() - Instant::now();
        assert!(
            due > Duration::from_secs(119) && due <= Duration::from_mins(2),
            "{due:?}"
        );
    }

    #[tokio::test]
    async fn a_relay_outage_moves_nothing_and_marks_nothing_suspect() {
        let fake = Fake::new(travel_router());
        let mut owner = settled(&fake).await;
        // The relay is down: no uplink reaches it.
        fake.kill("eth1");
        fake.kill("wwan0");
        fake.tunnel_on(None);
        owner.on_signal(failed(ETH1)).await;
        assert!(owner.on_signal(failed(ETH1)).await.is_empty());
        assert_eq!(fake.probes(), ["eth1", "wwan0"], "one round, both asked");
        assert!(fake.writes().is_empty());
        let report = owner.report(RelayRouteMode::FollowDefault);
        assert_eq!(report.dev.as_deref(), Some("eth1"));
        assert!(report.suspect.is_empty());
        assert!(
            owner.next_probe().is_none(),
            "no timer outside the degraded state"
        );
        // The count starts over: one more failure asks nobody.
        owner.on_signal(failed(ETH1)).await;
        assert!(fake.probes().is_empty());
        owner.on_signal(failed(ETH1)).await;
        assert_eq!(fake.probes(), ["eth1", "wwan0"]);

        // The relay is back: the tunnel registers over the wire, as before.
        fake.revive("eth1");
        fake.revive("wwan0");
        fake.tunnel_on(Some("eth1"));
        assert!(owner.on_signal(registered(ETH1)).await.is_empty());
        assert!(fake.writes().is_empty());
        assert!(owner
            .report(RelayRouteMode::FollowDefault)
            .suspect
            .is_empty());
    }

    #[tokio::test]
    async fn a_held_uplink_that_answers_keeps_the_route_even_when_another_answers_too() {
        // The relay came back while the round ran, or the failures were the
        // relay's own: the wire answers, and so does LTE.
        let fake = Fake::new(travel_router());
        let mut owner = settled(&fake).await;
        fake.tunnel_on(None);
        owner.on_signal(failed(ETH1)).await;
        assert!(owner.on_signal(failed(ETH1)).await.is_empty());
        assert_eq!(fake.probes(), ["eth1", "wwan0"]);
        assert!(fake.writes().is_empty());
        let report = owner.report(RelayRouteMode::FollowDefault);
        assert_eq!(report.dev.as_deref(), Some("eth1"));
        assert!(report.suspect.is_empty());
        assert!(owner.next_probe().is_none());
    }

    #[tokio::test]
    async fn with_a_single_uplink_nothing_is_asked() {
        let mut net = travel_router();
        net.default_routes.retain(|r| r.dev == "eth1");
        net.interfaces[1].default_metric = None;
        let fake = Fake::new(net);
        let mut owner = settled(&fake).await;
        fake.kill("eth1");
        owner.on_signal(failed(ETH1)).await;
        assert!(owner.on_signal(failed(ETH1)).await.is_empty());
        assert!(fake.probes().is_empty());
        assert!(owner
            .report(RelayRouteMode::FollowDefault)
            .suspect
            .is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_suspect_returns_only_after_two_good_probes_in_a_row() {
        let fake = Fake::new(travel_router());
        let mut owner = failed_over(&fake).await;
        // Not due yet: nobody is asked.
        assert!(owner.probe_suspects().await.is_empty());
        assert!(fake.probes().is_empty());
        // eth1 answers every other round: never twice in a row.
        for round in 0..4 {
            if round % 2 == 0 {
                fake.revive("eth1");
            } else {
                fake.kill("eth1");
            }
            assert!(next_round(&mut owner).await.is_empty(), "round {round}");
        }
        assert_eq!(fake.probes(), ["eth1"; 4]);
        assert!(fake.writes().is_empty());
        assert_eq!(
            owner.report(RelayRouteMode::FollowDefault).suspect,
            ["eth1"]
        );

        fake.revive("eth1");
        assert!(next_round(&mut owner).await.is_empty());
        let changes = next_round(&mut owner).await;
        assert_eq!(
            changes,
            moved(Some("wwan0"), Some("eth1"), "eth1 answers the relay again")
        );
        assert_eq!(fake.probes(), ["eth1", "eth1"], "no third probe to move");
        assert_eq!(
            fake.writes(),
            ["replace 174.138.114.209 via 10.42.0.1 dev eth1 metric 0"]
        );
        assert!(owner
            .report(RelayRouteMode::FollowDefault)
            .suspect
            .is_empty());
        assert!(
            owner.next_probe().is_none(),
            "the schedule ends with the last suspect"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_probe_schedule_backs_off_to_every_fifteen_minutes() {
        let fake = Fake::new(travel_router());
        let mut owner = failed_over(&fake).await;
        let mut waits = Vec::new();
        for _ in 0..6 {
            waits.push(minutes_to_next_round(&owner));
            next_round(&mut owner).await;
        }
        assert_eq!(waits, [2, 5, 10, 15, 15, 15]);
    }

    /// eth1, suspect, answers again: two probe rounds give it the route
    /// back, and the tunnel follows.
    async fn eth1_returns(owner: &mut Owner<Arc<Fake>>, fake: &Arc<Fake>) {
        fake.revive("eth1");
        assert!(next_round(owner).await.is_empty());
        assert_eq!(
            next_round(owner).await,
            moved(Some("wwan0"), Some("eth1"), "eth1 answers the relay again")
        );
        fake.tunnel_on(Some("eth1"));
        owner.on_signal(registered(ETH1)).await;
    }

    /// eth1, holding the route, dies: two tunnel failures move it to LTE.
    async fn eth1_dies(owner: &mut Owner<Arc<Fake>>, fake: &Arc<Fake>) {
        fake.kill("eth1");
        fake.tunnel_on(None);
        owner.on_signal(failed(ETH1)).await;
        assert_eq!(
            owner.on_signal(failed(ETH1)).await,
            moved(
                Some("eth1"),
                Some("wwan0"),
                "eth1: no answer from the relay"
            )
        );
        fake.tunnel_on(Some("wwan0"));
        owner.on_signal(registered(LTE)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn an_uplink_that_fails_soon_after_its_return_waits_twice_as_long() {
        let fake = Fake::new(travel_router());
        let mut owner = failed_over(&fake).await;
        let mut first_waits = Vec::new();
        let mut second_waits = Vec::new();
        for _ in 0..7 {
            // The same as eth1_returns, reading each wait on the way.
            first_waits.push(minutes_to_next_round(&owner));
            fake.revive("eth1");
            assert!(next_round(&mut owner).await.is_empty());
            second_waits.push(minutes_to_next_round(&owner));
            assert_eq!(next_round(&mut owner).await.len(), 1, "back on eth1");
            fake.tunnel_on(Some("eth1"));
            owner.on_signal(registered(ETH1)).await;
            // Five minutes after it got the route back, it dies again.
            tokio::time::sleep(Duration::from_mins(5)).await;
            eth1_dies(&mut owner, &fake).await;
        }
        assert_eq!(
            first_waits,
            [2, 4, 8, 16, 32, 60, 60],
            "never above an hour"
        );
        assert_eq!(second_waits, [5, 10, 20, 40, 60, 60, 60]);
    }

    #[tokio::test(start_paused = true)]
    async fn an_uplink_that_keeps_the_route_for_an_hour_is_forgiven() {
        let fake = Fake::new(travel_router());
        let mut owner = failed_over(&fake).await;
        for _ in 0..2 {
            eth1_returns(&mut owner, &fake).await;
            tokio::time::sleep(Duration::from_mins(1)).await;
            eth1_dies(&mut owner, &fake).await;
        }
        assert_eq!(minutes_to_next_round(&owner), 8, "two relapses");
        eth1_returns(&mut owner, &fake).await;
        assert!(
            owner.next_probe().is_none(),
            "no timer while nothing is suspect"
        );
        tokio::time::sleep(Duration::from_mins(61)).await;
        eth1_dies(&mut owner, &fake).await;
        assert_eq!(minutes_to_next_round(&owner), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failure_after_ten_minutes_neither_doubles_nor_forgives() {
        let fake = Fake::new(travel_router());
        let mut owner = failed_over(&fake).await;
        eth1_returns(&mut owner, &fake).await;
        eth1_dies(&mut owner, &fake).await;
        assert_eq!(minutes_to_next_round(&owner), 4, "one relapse");
        eth1_returns(&mut owner, &fake).await;
        tokio::time::sleep(Duration::from_mins(20)).await;
        eth1_dies(&mut owner, &fake).await;
        assert_eq!(minutes_to_next_round(&owner), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn each_suspect_keeps_its_own_schedule() {
        let mut net = travel_router();
        net.interfaces.insert(1, iface("eth2", 4, ETH2, Some(20)));
        net.default_routes
            .insert(1, default("eth2", Ipv4Addr::new(192, 168, 1, 1), 20));
        let fake = Fake::new(net);
        // eth1 is dead from the start: the route goes to eth2.
        fake.kill("eth1");
        let mut owner = Owner::new(fake.clone(), schedule(), Vec::new());
        fake.tunnel_on(Some("wwan0"));
        owner.on_net(fake.state()).await;
        owner.on_signal(registered(LTE)).await;
        fake.tunnel_on(Some("eth2"));
        owner.on_signal(registered(ETH2)).await;
        let report = owner.report(RelayRouteMode::FollowDefault);
        assert_eq!(
            (report.dev.as_deref(), report.suspect),
            (Some("eth2"), vec!["eth1".to_string()])
        );

        // Three minutes on, eth2 dies too and LTE takes the route.
        tokio::time::sleep(Duration::from_mins(3)).await;
        fake.kill("eth2");
        fake.tunnel_on(None);
        owner.on_signal(failed(ETH2)).await;
        assert_eq!(
            owner.on_signal(failed(ETH2)).await,
            moved(
                Some("eth2"),
                Some("wwan0"),
                "eth2: no answer from the relay"
            )
        );
        fake.probes();
        // eth1 has been due since minute 2; eth2 is due at minute 5.
        assert!(next_round(&mut owner).await.is_empty());
        assert_eq!(fake.probes(), ["eth1"], "only the one that is due");
        assert_eq!(minutes_to_next_round(&owner), 2);
        assert!(next_round(&mut owner).await.is_empty());
        assert_eq!(fake.probes(), ["eth2"]);
        // eth1's second wait (5) runs from minute 3, eth2's from minute 5.
        assert_eq!(minutes_to_next_round(&owner), 3);
    }

    #[tokio::test]
    async fn a_change_on_the_suspect_brings_it_back_when_it_answers() {
        let fake = Fake::new(travel_router());
        let mut owner = failed_over(&fake).await;
        // Replugged: a new lease.
        fake.revive("eth1");
        fake.edit(|n| {
            n.interfaces[0].ipv4 = Some(Ipv4Cidr {
                addr: ETH2,
                prefix: 24,
            });
        });
        let changes = owner.on_net(fake.state()).await;
        assert_eq!(changes, moved(Some("wwan0"), Some("eth1"), "eth1 changed"));
        assert_eq!(
            fake.probes(),
            ["eth1"],
            "asked once before the tunnel is moved"
        );
        assert!(owner.next_probe().is_none());
    }

    #[tokio::test]
    async fn a_change_on_a_suspect_that_still_does_not_answer_moves_nothing() {
        let fake = Fake::new(travel_router());
        let mut owner = failed_over(&fake).await;
        for flap in 0..3 {
            fake.edit(|n| n.interfaces[0].carrier = Some(flap % 2 == 1));
            assert!(owner.on_net(fake.state()).await.is_empty(), "flap {flap}");
        }
        assert_eq!(fake.probes(), ["eth1"; 3]);
        assert!(fake.writes().is_empty());
        assert_eq!(
            owner.report(RelayRouteMode::FollowDefault).suspect,
            ["eth1"]
        );
    }

    #[tokio::test]
    async fn a_registration_over_a_suspect_uplink_clears_it() {
        let fake = Fake::new(travel_router());
        let mut owner = failed_over(&fake).await;
        assert_eq!(
            owner.report(RelayRouteMode::FollowDefault).suspect,
            ["eth1"]
        );
        // The tunnel reaches the relay over the wire, before any probe did.
        fake.revive("eth1");
        fake.tunnel_on(Some("eth1"));
        let changes = owner.on_signal(registered(ETH1)).await;
        assert_eq!(
            changes,
            moved(
                Some("wwan0"),
                Some("eth1"),
                "the tunnel reached the relay over eth1"
            )
        );
        assert!(fake.probes().is_empty(), "the registration is the proof");
        assert_eq!(
            fake.writes(),
            ["replace 174.138.114.209 via 10.42.0.1 dev eth1 metric 0"]
        );
        assert!(owner
            .report(RelayRouteMode::FollowDefault)
            .suspect
            .is_empty());
        assert!(owner.next_probe().is_none(), "and its timer is gone");
    }

    #[tokio::test]
    async fn a_registration_over_another_uplink_leaves_a_suspect_alone() {
        let fake = Fake::new(travel_router());
        let mut owner = failed_over(&fake).await;
        assert!(owner.on_signal(registered(LTE)).await.is_empty());
        assert!(fake.probes().is_empty());
        assert_eq!(
            owner.report(RelayRouteMode::FollowDefault).suspect,
            ["eth1"]
        );
    }

    #[tokio::test]
    async fn the_owners_own_route_write_does_not_clear_a_suspect() {
        let fake = Fake::new(travel_router());
        let mut owner = failed_over(&fake).await;
        assert!(owner.on_net(fake.state()).await.is_empty());
        assert!(fake.probes().is_empty());
        assert_eq!(
            owner.report(RelayRouteMode::FollowDefault).suspect,
            ["eth1"]
        );
    }

    #[tokio::test]
    async fn uplinks_failing_in_turn_do_not_flap_the_route() {
        let fake = Fake::new(travel_router());
        let mut owner = failed_over(&fake).await;
        // Now LTE fails too, while the wire still does not answer.
        fake.tunnel_on(None);
        fake.kill("wwan0");
        for _ in 0..3 {
            owner.on_signal(failed(LTE)).await;
            owner.on_signal(failed(LTE)).await;
        }
        assert_eq!(fake.probes(), ["wwan0", "eth1"].repeat(3));
        assert!(fake.writes().is_empty());
        let report = owner.report(RelayRouteMode::FollowDefault);
        assert_eq!(report.dev.as_deref(), Some("wwan0"));
        assert_eq!(
            report.suspect,
            ["eth1"],
            "and LTE is not suspect: nothing answered"
        );

        // When the wire answers, LTE failing twice sends the route back.
        fake.revive("eth1");
        owner.on_signal(failed(LTE)).await;
        let changes = owner.on_signal(failed(LTE)).await;
        assert_eq!(
            changes,
            moved(
                Some("wwan0"),
                Some("eth1"),
                "wwan0: no answer from the relay"
            )
        );
        assert_eq!(
            owner.report(RelayRouteMode::FollowDefault).suspect,
            ["wwan0"]
        );
    }

    #[tokio::test]
    async fn losing_the_held_uplink_moves_to_the_next_that_answers() {
        let fake = Fake::new(travel_router());
        let mut owner = settled(&fake).await;
        fake.tunnel_on(None);
        // Unplugged: netifd drops the address and default route, and the
        // kernel flushes the routes through the link.
        fake.edit(|n| {
            n.interfaces[0].ipv4 = None;
            n.interfaces[0].default_metric = None;
            n.default_routes.retain(|r| r.dev != "eth1");
            n.host_routes.retain(|r| r.dev != "eth1");
        });
        let changes = owner.on_net(fake.state()).await;
        assert_eq!(
            changes,
            moved(Some("eth1"), Some("wwan0"), "lowest-metric default route")
        );
        assert_eq!(fake.probes(), ["wwan0"]);
        assert_eq!(fake.ours(), [host("wwan0", LTE_GW, 0, RTPROT_SCTL)]);
    }

    #[tokio::test]
    async fn with_no_default_route_left_the_route_is_removed() {
        let fake = Fake::new(travel_router());
        let mut owner = settled(&fake).await;
        fake.edit(|n| {
            n.default_routes.clear();
            for i in &mut n.interfaces {
                i.default_metric = None;
            }
        });
        let changes = owner.on_net(fake.state()).await;
        assert_eq!(
            changes,
            moved(Some("eth1"), None, "no uplink left to hold it")
        );
        assert_eq!(fake.writes(), ["delete 174.138.114.209"]);
        assert!(fake.ours().is_empty());
        assert!(fake
            .state()
            .host_routes
            .contains(&host("wwan0", LTE_GW, 40, 4)));
    }

    #[tokio::test]
    async fn a_new_relay_address_takes_the_route_with_it() {
        let fake = Fake::new(travel_router());
        let mut owner = settled(&fake).await;
        let moved_relay = SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 9), 443);
        // The fake only knows RELAY, so check the writes rather than probes.
        let changes = owner
            .on_signal(TunnelSignal::Registered {
                relay: moved_relay,
                local: ETH1,
            })
            .await;
        assert_eq!(
            changes,
            moved(None, Some("eth1"), "lowest-metric default route")
        );
        assert_eq!(
            fake.writes(),
            [
                "delete 174.138.114.209",
                "replace 203.0.113.9 via 10.42.0.1 dev eth1 metric 0"
            ]
        );
    }

    #[tokio::test]
    async fn a_refused_write_is_retried_on_the_next_change() {
        struct Refusing(Arc<Fake>, Mutex<bool>);
        impl Uplinks for Arc<Refusing> {
            async fn answers(&self, relay: SocketAddrV4, dev: &str, src: Ipv4Addr) -> bool {
                self.0.answers(relay, dev, src).await
            }
            async fn replace(&self, route: OwnedRoute) -> io::Result<()> {
                if *self.1.lock().unwrap() {
                    return Err(io::Error::from_raw_os_error(libc::EPERM));
                }
                self.0.replace(route).await
            }
            async fn delete(&self, route: OwnedRoute) -> io::Result<()> {
                self.0.delete(route).await
            }
            fn tunnel_dev(&self) -> Option<String> {
                self.0.tunnel_dev()
            }
        }
        let fake = Fake::new(travel_router());
        fake.tunnel_on(Some("eth1"));
        let refusing = Arc::new(Refusing(fake.clone(), Mutex::new(true)));
        let mut owner = Owner::new(refusing.clone(), schedule(), Vec::new());
        owner.on_net(fake.state()).await;
        assert!(owner.on_signal(registered(ETH1)).await.is_empty());
        assert_eq!(owner.report(RelayRouteMode::FollowDefault).dev, None);
        *refusing.1.lock().unwrap() = false;
        let changes = owner.on_net(fake.state()).await;
        assert_eq!(
            changes,
            moved(None, Some("eth1"), "lowest-metric default route")
        );
    }

    #[test]
    fn strict_rp_filter_is_the_larger_of_all_and_the_uplink() {
        let uplinks = ["eth1".to_string(), "wwan0".to_string(), "eth2".to_string()];
        let reading = |all: u8, values: &'static [(&'static str, u8)]| {
            move |name: &str| {
                if name == "all" {
                    return Some(all);
                }
                values.iter().find(|(n, _)| *n == name).map(|(_, v)| *v)
            }
        };
        // conf/all 0: each uplink's own value decides; unreadable counts as 0.
        assert_eq!(
            strict_rp_filter(&uplinks, reading(0, &[("eth1", 1), ("wwan0", 0)])),
            ["eth1"]
        );
        // conf/all 1 makes every uplink strict but one set loose itself.
        assert_eq!(
            strict_rp_filter(&uplinks, reading(1, &[("eth1", 0), ("wwan0", 2)])),
            ["eth1", "eth2"]
        );
        // conf/all 2, what the XE300 packaging sets, makes every one loose.
        assert!(strict_rp_filter(&uplinks, reading(2, &[("eth1", 1), ("wwan0", 1)])).is_empty());
        assert!(strict_rp_filter(&uplinks, |_| None).is_empty());
    }

    #[tokio::test]
    async fn the_report_names_every_uplink_it_may_probe() {
        let fake = Fake::new(travel_router());
        let owner = settled(&fake).await;
        let report = owner.report(RelayRouteMode::FollowDefault);
        assert_eq!(report.uplinks, ["eth1", "wwan0"]);
        assert!(serde_json::to_value(&report)
            .unwrap()
            .get("uplinks")
            .is_none());
    }

    #[tokio::test]
    async fn an_owner_that_is_off_drops_signals_and_reports_off() {
        let off = RelayRoute::new(RelayRouteMode::Off, Vec::new());
        off.signal(registered(ETH1));
        assert!(off.rx.lock().await.try_recv().is_err());
        assert_eq!(
            serde_json::to_value(off.report()).unwrap(),
            serde_json::json!({
                "mode": "off",
                "dev": null,
                "via": null,
                "suspect": [],
                "rp_filter_strict": []
            })
        );

        let on = RelayRoute::new(RelayRouteMode::FollowDefault, Vec::new());
        on.signal(registered(ETH1));
        assert_eq!(on.rx.lock().await.try_recv().unwrap(), registered(ETH1));

        let prefers = RelayRoute::new(RelayRouteMode::Prefer, prefer(&["eth0", "usb0"]));
        assert_eq!(prefers.prefer(), ["eth0", "usb0"]);
        let report = serde_json::to_value(prefers.report()).unwrap();
        assert_eq!(report["mode"], "prefer");
        assert_eq!(report["prefer"], serde_json::json!(["eth0", "usb0"]));
    }

    /// The probe against real sockets on loopback: `lo` stands in for an
    /// uplink, bound with SO_BINDTODEVICE as a real one is.
    mod probe {
        use std::fmt::Write as _;

        use rustls::pki_types::pem::PemObject;
        use rustls::pki_types::{CertificateDer, PrivateKeyDer};
        use sha2::{Digest, Sha256};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        use super::*;

        /// A throwaway CA made for these tests (P-256, valid 2026 to 2126).
        const TEST_CA: &str = concat!(
            "-----BEGIN CERTIFICATE-----\n",
            "MIIBZDCCAQqgAwIBAgIUI8lkzak4ugreveQWWrufISPFMkEwCgYIKoZIzj0EAwIw\n",
            "HTEbMBkGA1UEAwwSc2N0bCBwcm9iZSB0ZXN0IENBMCAXDTI2MDEwMTAwMDAwMFoY\n",
            "DzIxMjYwMTAxMDAwMDAwWjAdMRswGQYDVQQDDBJzY3RsIHByb2JlIHRlc3QgQ0Ew\n",
            "WTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAAS/+4ZtCOXeVxh/Ro517zCMH7htOWqP\n",
            "HJAceQtDyfsvvJdHk5ppEhla6x2lOLuf80l12LXGvoc4Tt9/yBv9qSsVoyYwJDAS\n",
            "BgNVHRMBAf8ECDAGAQH/AgEAMA4GA1UdDwEB/wQEAwIBBjAKBggqhkjOPQQDAgNI\n",
            "ADBFAiEAu0f0dQzAqhKndFCjwTJjm1y8nE0wXcHQp9ac6hZms2MCIDYoJMV6N3bF\n",
            "KlYXgVfjOv/+Hi6b4DhjoxfyHJ5gjtAd\n",
            "-----END CERTIFICATE-----\n",
        );

        /// The certificate it signed for `relay.test`.
        const TEST_LEAF: &str = concat!(
            "-----BEGIN CERTIFICATE-----\n",
            "MIIBgzCCASigAwIBAgIUIJlyS63ium/2B898jfQS4NfE3RYwCgYIKoZIzj0EAwIw\n",
            "HTEbMBkGA1UEAwwSc2N0bCBwcm9iZSB0ZXN0IENBMCAXDTI2MDEwMTAwMDAwMFoY\n",
            "DzIxMjYwMTAxMDAwMDAwWjAVMRMwEQYDVQQDDApyZWxheS50ZXN0MFkwEwYHKoZI\n",
            "zj0CAQYIKoZIzj0DAQcDQgAEXieO55mDEdk2QKKYHkKbjAuYNkHx8Mw9yX/pn3bh\n",
            "wPUMRKjbTUawFqf7N3EFuBt5CzS2Lrw/cwXcB72tasNIrqNMMEowDAYDVR0TAQH/\n",
            "BAIwADAOBgNVHQ8BAf8EBAMCB4AwEwYDVR0lBAwwCgYIKwYBBQUHAwEwFQYDVR0R\n",
            "BA4wDIIKcmVsYXkudGVzdDAKBggqhkjOPQQDAgNJADBGAiEA0Iiu/qIOlwDgfRrX\n",
            "zIBzvyvcRRtcfC19kCrM6uG5QzECIQCcifU2HwKXFb7xkxgFHFwGVBrITiwI5oqV\n",
            "EfHOfbWFbA==\n",
            "-----END CERTIFICATE-----\n",
        );

        /// The leaf's key: a test fixture, used nowhere else.
        const TEST_LEAF_KEY: &str = concat!(
            // The scan reads each line through `echo`, which would split
            // this one at a newline escape, away from its marker.
            "-----BEGIN PRIVATE KEY-----", // secret-scan: allow
            "\n",
            "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgZkgkiXi0zlbhRoIW\n",
            "CF31gVuRhcnWlqYjrmhPCTAissShRANCAAReJ47nmYMR2TZAopgeQpuMC5g2QfHw\n",
            "zD3Jf+mfduHA9QxEqNtNRrAWp/s3cQW4G3kLNLYuvD9zBdwHva1qw0iu\n",
            "-----END PRIVATE KEY-----\n",
        );

        /// What one connection to a stand-in relay came to.
        #[derive(Debug, PartialEq, Eq)]
        enum Seen {
            /// A completed TLS handshake, then this many bytes of data.
            Handshake(usize),
            /// A handshake that failed.
            NoHandshake,
        }

        fn leaf_der() -> CertificateDer<'static> {
            CertificateDer::from_pem_slice(TEST_LEAF.as_bytes()).unwrap()
        }

        /// A TLS server for `relay.test` on loopback, reporting each
        /// connection once it ends.
        async fn tls_relay() -> (SocketAddrV4, mpsc::UnboundedReceiver<Seen>) {
            let key = PrivateKeyDer::from_pem_slice(TEST_LEAF_KEY.as_bytes()).unwrap();
            let config = rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![leaf_der()], key)
                .unwrap();
            let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let SocketAddr::V4(addr) = listener.local_addr().unwrap() else {
                unreachable!("bound to an IPv4 address")
            };
            let (tx, rx) = mpsc::unbounded_channel();
            tokio::spawn(async move {
                while let Ok((tcp, _)) = listener.accept().await {
                    let (acceptor, tx) = (acceptor.clone(), tx.clone());
                    tokio::spawn(async move {
                        let seen = match acceptor.accept(tcp).await {
                            Ok(mut tls) => {
                                let mut data = Vec::new();
                                let _ = tls.read_to_end(&mut data).await;
                                Seen::Handshake(data.len())
                            }
                            Err(_) => Seen::NoHandshake,
                        };
                        let _ = tx.send(seen);
                    });
                }
            });
            (addr, rx)
        }

        /// A captive portal: it accepts any TCP connection and answers with a
        /// redirect, whatever it was sent.
        async fn captive_portal() -> SocketAddrV4 {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let SocketAddr::V4(addr) = listener.local_addr().unwrap() else {
                unreachable!("bound to an IPv4 address")
            };
            tokio::spawn(async move {
                while let Ok((mut tcp, _)) = listener.accept().await {
                    tokio::spawn(async move {
                        let mut buf = [0u8; 512];
                        let _ = tcp.read(&mut buf).await;
                        let _ = tcp
                            .write_all(
                                b"HTTP/1.1 302 Found\r\nLocation: http://portal.example/\r\n\
                                  Content-Length: 0\r\n\r\n",
                            )
                            .await;
                    });
                }
            });
            addr
        }

        /// A `wss://` tunnel to `relay.test` on `port`, with `extra` keys.
        fn tunnel(port: u16, extra: &str) -> TunnelConfig {
            toml::from_str(&format!(
                "tunnel_key = \"probe-test-key\"\n\
                 url = \"wss://relay.test:{port}/api/tunnel/register\"\n{extra}"
            ))
            .unwrap()
        }

        /// The test CA as a file, as `tls_ca_file` names one.
        struct CaFile(std::path::PathBuf);

        impl CaFile {
            fn new(name: &str) -> Self {
                let path = std::env::temp_dir()
                    .join(format!("sctl-probe-{}-{name}.pem", std::process::id()));
                std::fs::write(&path, TEST_CA).unwrap();
                Self(path)
            }

            fn key(&self) -> String {
                format!("tls_ca_file = \"{}\"", self.0.display())
            }
        }

        impl Drop for CaFile {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }

        /// Whether this kernel lets an unprivileged socket bind to a device
        /// (5.7 and later); older ones need CAP_NET_RAW.
        fn can_bind_to_lo() -> bool {
            let socket = TcpSocket::new_v4().unwrap();
            match source::bind_to_device(&socket, "lo") {
                Ok(()) => true,
                Err(e) => {
                    eprintln!("skipped: SO_BINDTODEVICE needs privileges here ({e})");
                    false
                }
            }
        }

        async fn ask(probe: &Probe, relay: SocketAddrV4) -> Result<(), String> {
            probe
                .ask(relay, "lo", Ipv4Addr::LOCALHOST, &Timing::default())
                .await
                .map_err(|e| e.to_string())
        }

        #[tokio::test]
        async fn a_probe_makes_the_tunnels_tls_handshake_and_sends_nothing_else() {
            if !can_bind_to_lo() {
                return;
            }
            let (relay, mut seen) = tls_relay().await;
            let ca = CaFile::new("handshake");
            let probe = Probe::for_tunnel(&tunnel(relay.port(), &ca.key()));
            assert!(matches!(probe, Probe::Tls(_)));
            ask(&probe, relay).await.unwrap();
            assert_eq!(
                seen.recv().await,
                Some(Seen::Handshake(0)),
                "a handshake, closed with no upgrade and no key"
            );
        }

        #[tokio::test]
        async fn an_uplink_that_accepts_tcp_but_breaks_tls_fails_the_probe() {
            if !can_bind_to_lo() {
                return;
            }
            let portal = captive_portal().await;
            let ca = CaFile::new("portal");
            // A bare connect, the probe before, passes on it.
            ask(&Probe::Connect, portal).await.unwrap();
            let probe = Probe::for_tunnel(&tunnel(portal.port(), &ca.key()));
            let err = ask(&probe, portal).await.unwrap_err();
            eprintln!("the portal fails the probe: {err}");
        }

        #[tokio::test]
        async fn a_probe_checks_the_certificate_as_the_tunnel_does() {
            if !can_bind_to_lo() {
                return;
            }
            let (relay, mut seen) = tls_relay().await;
            // Without the CA file the relay's certificate is not trusted.
            let untrusted = Probe::for_tunnel(&tunnel(relay.port(), ""));
            let err = ask(&untrusted, relay).await.unwrap_err();
            assert!(err.contains("certificate"), "{err}");
            assert_eq!(seen.recv().await, Some(Seen::NoHandshake));

            // The pin of the relay's own certificate passes; another fails.
            let ca = CaFile::new("pin");
            let pin =
                Sha256::digest(leaf_der().as_ref())
                    .iter()
                    .fold(String::new(), |mut hex, b| {
                        let _ = write!(hex, "{b:02x}");
                        hex
                    });
            let pinned = tunnel(
                relay.port(),
                &format!("{}\ntls_server_cert_sha256 = \"{pin}\"", ca.key()),
            );
            ask(&Probe::for_tunnel(&pinned), relay).await.unwrap();
            let other = format!(
                "{}{}",
                if pin.starts_with('0') { '1' } else { '0' },
                &pin[1..]
            );
            let mispinned = tunnel(
                relay.port(),
                &format!("{}\ntls_server_cert_sha256 = \"{other}\"", ca.key()),
            );
            let err = ask(&Probe::for_tunnel(&mispinned), relay)
                .await
                .unwrap_err();
            assert!(err.contains("pin mismatch"), "{err}");
        }

        #[tokio::test]
        async fn a_probe_is_bound_to_its_uplink() {
            if !can_bind_to_lo() {
                return;
            }
            // Over an interface that does not exist the probe cannot leave,
            // even though loopback would reach the relay.
            let (relay, _seen) = tls_relay().await;
            let err = Probe::Connect
                .ask(
                    relay,
                    "no-such-if0",
                    Ipv4Addr::LOCALHOST,
                    &Timing::default(),
                )
                .await
                .unwrap_err();
            let errno = err
                .downcast_ref::<io::Error>()
                .and_then(io::Error::raw_os_error);
            assert_eq!(errno, Some(libc::ENODEV), "{err}");
        }

        #[test]
        fn only_a_wss_tunnel_gets_a_tls_probe() {
            let plain: TunnelConfig = toml::from_str(
                "tunnel_key = \"probe-test-key\"\nurl = \"ws://10.0.0.1:8080/api/tunnel/register\"\n",
            )
            .unwrap();
            assert!(matches!(Probe::for_tunnel(&plain), Probe::Connect));
            assert!(matches!(Probe::for_tunnel(&tunnel(443, "")), Probe::Tls(_)));
        }
    }
}
