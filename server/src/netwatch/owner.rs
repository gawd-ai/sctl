//! One owner for the host route to the relay (`[tunnel] relay_route`).
//!
//! With `relay_route = "follow_default"` the agent keeps one route,
//! `relay/32 via <gw> dev <uplink> metric 0 proto 83`, where `relay` is the
//! address the tunnel connected to. At metric 0 it outranks the pin netifd
//! installs for the WireGuard endpoint (the same address) without touching
//! it. It stays in place when the agent exits, since WireGuard uses it too.
//!
//! The uplink is the lowest-metric default route whose interface is not
//! suspect ([`choose`]): netifd's metrics already rank the wire above LTE. It
//! is chosen again on every network change netwatch publishes and after every
//! registration. The route only moves to an uplink that answers the relay: a
//! TCP connect bound to that interface (`SO_BINDTODEVICE`), unless the tunnel
//! is registered over it already. An uplink that does not answer is suspect.
//!
//! A link that is up while its internet is dead sends no kernel event. The
//! tunnel's own failures are the signal: after two in a row on the uplink the
//! route uses (a dial nothing answered, a handshake that broke or stalled, a
//! pong timeout, a read or write error), that uplink and every other one
//! with a default route are asked in the same round. The route moves only
//! when its own uplink does not answer while another does, to the first of
//! those in metric order. The failing uplink, and any ranked above the one
//! that answered, become suspect. When every uplink fails, the relay itself
//! is down: nothing moves and nothing becomes suspect.
//!
//! A suspect uplink is asked again at once when netwatch reports a change on
//! it (link, address or default route: a replug, a new lease). Otherwise it
//! is probed on a schedule that exists only while something is suspect: after
//! 2, 5 and 10 minutes, then every 15. It gets the route back after two good
//! probes in a row. In steady state the owner runs no timer at all.
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
use tokio::net::{TcpSocket, TcpStream};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{debug, info, warn};

use super::route::{self, OwnedRoute, RTPROT_SCTL};
use super::{next_state, source, DefaultRoute, HostRoute, NetState, NetWatch};
use crate::config::RelayRouteMode;
use crate::state::{TunnelEventType, TunnelStats};

/// Tunnel failures in a row on the route's uplink before the others are asked.
const FAILURES_TO_FAIL_OVER: u8 = 2;
/// Good probes in a row before a suspect uplink gets the route back.
const PROBES_TO_RETURN: u8 = 2;
/// Tunnel signals queued for the owner; newer ones are dropped past this.
const SIGNAL_QUEUE: usize = 16;

/// How the owner paces its probes. Tests shorten it.
#[derive(Clone, Debug)]
pub struct Timing {
    /// Waits between probe rounds while an uplink is suspect. The last one
    /// repeats.
    pub return_schedule: Vec<Duration>,
    /// How long one probe's TCP connect may take.
    pub probe_timeout: Duration,
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
            // What the tunnel itself gives a TCP connect.
            probe_timeout: Duration::from_secs(10),
        }
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
    /// The uplink sctl's route to the relay goes through; None while sctl
    /// holds no route.
    pub dev: Option<String>,
    pub via: Option<Ipv4Addr>,
    /// Uplinks that did not answer the relay, waiting to be asked again.
    pub suspect: Vec<String>,
}

/// The owner's shared side: the tunnel's signals in, the report out. One per
/// tunnel client, whatever the mode.
pub struct RelayRoute {
    mode: RelayRouteMode,
    tx: mpsc::Sender<TunnelSignal>,
    /// Held by the running owner; a restarted one takes it back.
    rx: tokio::sync::Mutex<mpsc::Receiver<TunnelSignal>>,
    report: std::sync::Mutex<Report>,
}

impl RelayRoute {
    pub fn new(mode: RelayRouteMode) -> Self {
        let (tx, rx) = mpsc::channel(SIGNAL_QUEUE);
        Self {
            mode,
            tx,
            rx: tokio::sync::Mutex::new(rx),
            report: std::sync::Mutex::new(Report {
                mode,
                dev: None,
                via: None,
                suspect: Vec::new(),
            }),
        }
    }

    pub fn mode(&self) -> RelayRouteMode {
        self.mode
    }

    /// Tell the owner how the tunnel is doing. Never waits: with the mode
    /// off, or the owner behind, the signal is dropped.
    pub fn signal(&self, signal: TunnelSignal) {
        if self.mode != RelayRouteMode::Off {
            let _ = self.tx.try_send(signal);
        }
    }

    pub fn report(&self) -> Report {
        self.report
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn set_report(&self, report: Report) {
        *self
            .report
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = report;
    }
}

/// The uplink for the relay route: the lowest-metric default route whose
/// interface is not suspect. Ties go to the interface that sorts first, as in
/// [`NetState::lowest_default`].
pub fn choose(net: &NetState, suspect: impl Fn(&str) -> bool) -> Option<&DefaultRoute> {
    net.default_routes
        .iter()
        .filter(|r| !suspect(&r.dev))
        .min_by(|a, b| (a.metric, &a.dev).cmp(&(b.metric, &b.dev)))
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

/// Every uplink but `except` with a default route and an address, lowest
/// metric first, in [`choose`]'s order.
fn candidates(net: &NetState, except: &str) -> Vec<(String, Ipv4Addr)> {
    let mut routes: Vec<&DefaultRoute> = net.default_routes.iter().collect();
    routes.sort_by(|a, b| (a.metric, &a.dev).cmp(&(b.metric, &b.dev)));
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
    /// Whether a TCP connect to `relay`, bound to `dev` and `src`, succeeds.
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

/// The owner's state, fed one event at a time.
pub(crate) struct Owner<U> {
    uplinks: U,
    schedule: Vec<Duration>,
    relay: Option<SocketAddrV4>,
    net: Option<Arc<NetState>>,
    /// sctl's route as sctl last wrote it (or found it, once, at start).
    held: Option<Held>,
    adopted: bool,
    /// Suspect uplinks, each with its good probes in a row.
    suspect: BTreeMap<String, u8>,
    /// Tunnel failures in a row on the held uplink.
    failures: u8,
    /// Probe rounds since the schedule started, and when the next one is due.
    rounds: usize,
    next_probe: Option<Instant>,
    /// The route someone else installed that the owner stands aside for.
    left_to: Option<HostRoute>,
    write_failing: bool,
}

impl<U: Uplinks> Owner<U> {
    pub(crate) fn new(uplinks: U, schedule: Vec<Duration>) -> Self {
        Self {
            uplinks,
            schedule,
            relay: None,
            net: None,
            held: None,
            adopted: false,
            suspect: BTreeMap::new(),
            failures: 0,
            rounds: 0,
            next_probe: None,
            left_to: None,
            write_failing: false,
        }
    }

    pub(crate) fn report(&self, mode: RelayRouteMode) -> Report {
        Report {
            mode,
            dev: self.held.as_ref().map(|h| h.dev.clone()),
            via: self.held.as_ref().and_then(|h| h.via),
            suspect: self.suspect.keys().cloned().collect(),
        }
    }

    /// When the next probe round is due; None outside the degraded state.
    pub(crate) fn next_probe(&self) -> Option<Instant> {
        self.next_probe
    }

    /// A new network state.
    pub(crate) async fn on_net(&mut self, net: Arc<NetState>) -> Vec<Change> {
        let mut reason = "lowest-metric default route".to_string();
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
                self.settle_schedule();
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
                self.reconcile(&[], "lowest-metric default route".into())
                    .await
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
                    changes = self
                        .reconcile(&[], "lowest-metric default route".into())
                        .await;
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

    /// Ask every suspect uplink once; one that answered twice in a row gets
    /// its place back.
    pub(crate) async fn probe_suspects(&mut self) -> Vec<Change> {
        let (Some(net), Some(relay)) = (self.net.clone(), self.relay) else {
            return Vec::new();
        };
        let mut back = Vec::new();
        let devs: Vec<String> = self.suspect.keys().cloned().collect();
        for dev in devs {
            let answered = self.asks(&net, relay, &dev).await;
            if let Some(good) = self.suspect.get_mut(&dev) {
                *good = if answered { *good + 1 } else { 0 };
                if *good >= PROBES_TO_RETURN {
                    back.push(dev);
                }
            }
        }
        for dev in &back {
            self.suspect.remove(dev);
        }
        self.rounds += 1;
        self.next_probe = (!self.suspect.is_empty()).then(|| Instant::now() + self.delay());
        if back.is_empty() {
            return Vec::new();
        }
        let reason = format!("{} answers the relay again", back.join(", "));
        self.reconcile(&back, reason).await
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
    /// suspect, so a relay outage never sends a unit to LTE.
    async fn fail_over(&mut self) -> Vec<Change> {
        let (Some(net), Some(relay), Some(held)) =
            (self.net.clone(), self.relay, self.held.clone())
        else {
            return Vec::new();
        };
        let others = candidates(&net, &held.dev);
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
            let Some(want) = choose(&net, |dev| self.suspect.contains_key(dev)).cloned() else {
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
            let route = OwnedRoute {
                dst: relay_ip,
                via: want.gw,
                oif,
                metric: 0,
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
            };
            self.write(Write::Delete(any)).await;
            info!("relay route: the relay is no longer at {old}; its route is removed");
        }
        self.failures = 0;
    }

    /// Mark `dev` suspect. A newly suspect uplink starts the probe schedule
    /// again.
    fn mark_suspect(&mut self, dev: &str) {
        if self.suspect.insert(dev.to_string(), 0).is_none() {
            self.rounds = 0;
            self.next_probe = Some(Instant::now() + self.delay());
        }
    }

    /// Stop the schedule once nothing is suspect.
    fn settle_schedule(&mut self) {
        if self.suspect.is_empty() {
            self.rounds = 0;
            self.next_probe = None;
        }
    }

    /// The wait before the next probe round.
    fn delay(&self) -> Duration {
        self.schedule
            .get(self.rounds)
            .or(self.schedule.last())
            .copied()
            .unwrap_or(Duration::from_mins(15))
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

/// The real uplinks: kernel routes, bound connects and the tunnel's stats.
struct Kernel {
    stats: Arc<TunnelStats>,
    probe_timeout: Duration,
}

impl Uplinks for Kernel {
    async fn answers(&self, relay: SocketAddrV4, dev: &str, src: Ipv4Addr) -> bool {
        match connect_from(relay, dev, src, self.probe_timeout).await {
            Ok(_) => {
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
/// events. Parks when the mode is off. Cancelling leaves the route in place.
pub async fn run(
    handle: Arc<RelayRoute>,
    mut net: NetWatch,
    stats: Arc<TunnelStats>,
    timing: Timing,
) {
    if handle.mode() == RelayRouteMode::Off {
        return std::future::pending().await;
    }
    // A restarted owner takes the queue back from the one that panicked.
    let mut signals = handle.rx.lock().await;
    let mut owner = Owner::new(
        Kernel {
            stats: stats.clone(),
            probe_timeout: timing.probe_timeout,
        },
        timing.return_schedule,
    );
    info!("relay route: keeping the relay's route on the best uplink that answers it");
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
        }
    }

    fn default(dev: &str, gw: Ipv4Addr, metric: u32) -> DefaultRoute {
        DefaultRoute {
            dev: dev.into(),
            gw: Some(gw),
            metric,
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
                "replace {} via {} dev {dev} metric {}",
                route.dst,
                route.via.map_or("-".into(), |v| v.to_string()),
                route.metric
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
        let mut owner = Owner::new(fake.clone(), schedule());
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
        assert_eq!(choose(&net, |_| false).unwrap().dev, "eth1");
        assert_eq!(choose(&net, |d| d == "eth1").unwrap().dev, "wwan0");
        assert!(choose(&net, |_| true).is_none());
        assert!(choose(&NetState::default(), |_| false).is_none());
    }

    #[test]
    fn choose_breaks_ties_by_name_whatever_the_order() {
        let mut net = travel_router();
        net.default_routes = vec![
            default("wwan0", LTE_GW, 10),
            default("eth1", ETH1_GW, 10),
            default("eth0", ETH1_GW, 20),
        ];
        assert_eq!(choose(&net, |_| false).unwrap().dev, "eth1");
        assert_eq!(choose(&net, |d| d == "eth1").unwrap().dev, "wwan0");
        assert_eq!(choose(&net, |d| d != "eth0").unwrap().dev, "eth0");
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
        let mut owner = Owner::new(fake.clone(), schedule());
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
        let mut owner = Owner::new(fake.clone(), schedule());
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
            serde_json::json!({"mode": "follow_default", "dev": "eth1", "via": "10.42.0.1", "suspect": []})
        );
    }

    #[tokio::test]
    async fn a_tunnel_already_on_the_best_uplink_is_not_probed() {
        let fake = Fake::new(travel_router());
        let mut owner = Owner::new(fake.clone(), schedule());
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
        let mut owner = Owner::new(fake.clone(), schedule());
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
        let mut owner = Owner::new(fake.clone(), schedule());
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
        let mut owner = Owner::new(fake.clone(), schedule());
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
        let mut owner = Owner::new(fake.clone(), schedule());
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

    #[tokio::test]
    async fn a_suspect_returns_only_after_two_good_probes_in_a_row() {
        let fake = Fake::new(travel_router());
        let mut owner = failed_over(&fake).await;
        // eth1 answers every other round: never twice in a row.
        for round in 0..4 {
            if round % 2 == 0 {
                fake.revive("eth1");
            } else {
                fake.kill("eth1");
            }
            assert!(owner.probe_suspects().await.is_empty(), "round {round}");
        }
        assert_eq!(fake.probes(), ["eth1"; 4]);
        assert!(fake.writes().is_empty());
        assert_eq!(
            owner.report(RelayRouteMode::FollowDefault).suspect,
            ["eth1"]
        );

        fake.revive("eth1");
        assert!(owner.probe_suspects().await.is_empty());
        let changes = owner.probe_suspects().await;
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

    #[tokio::test]
    async fn the_probe_schedule_backs_off_to_every_fifteen_minutes() {
        let fake = Fake::new(travel_router());
        let mut owner = failed_over(&fake).await;
        let mut waits = Vec::new();
        for _ in 0..6 {
            let due = owner.next_probe().unwrap() - Instant::now();
            waits.push(due.as_secs().div_ceil(60));
            owner.probe_suspects().await;
        }
        assert_eq!(waits, [2, 5, 10, 15, 15, 15]);
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
        let mut owner = Owner::new(refusing.clone(), schedule());
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

    #[tokio::test]
    async fn an_owner_that_is_off_drops_signals_and_reports_off() {
        let off = RelayRoute::new(RelayRouteMode::Off);
        off.signal(registered(ETH1));
        assert!(off.rx.lock().await.try_recv().is_err());
        assert_eq!(
            serde_json::to_value(off.report()).unwrap(),
            serde_json::json!({"mode": "off", "dev": null, "via": null, "suspect": []})
        );

        let on = RelayRoute::new(RelayRouteMode::FollowDefault);
        on.signal(registered(ETH1));
        assert_eq!(on.rx.lock().await.try_recv().unwrap(), registered(ETH1));
    }
}
