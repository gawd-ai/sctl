//! Tunnel client — outbound WS connection from device to relay.
//!
//! Spawned on startup when `[tunnel] url` is configured. Maintains a persistent
//! WebSocket to the relay with exponential-backoff reconnect, heartbeat, and
//! handles proxied requests by calling local route handlers.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures_util::{FutureExt, SinkExt, StreamExt};
use rustls::pki_types::{pem::PemObject, CertificateDer, ServerName};
use rustls::{ClientConfig as RustlsClientConfig, RootCertStore};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch, Mutex, Semaphore};
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;
use tracing::{error, info, warn};

use crate::activity::{self, ActivityType, CachedExecResult};
use crate::atomic::AtomicU64;
use crate::config::{RelayRouteMode, TunnelConfig};
use crate::netwatch::owner::TunnelSignal;
use crate::netwatch::route::RTPROT_SCTL;
use crate::netwatch::{self, source, NetWatch};
use crate::sessions::buffer::{OutputBuffer, OutputEntry};
use crate::state::{TunnelEventType, TunnelPath};
use crate::AppState;

use super::{decode_binary_frame, encode_binary_frame};
use super::{infra_state, net_state};

/// Static heartbeat message — avoids serde allocation on every heartbeat tick.
const PING_TEXT: &str = r#"{"type":"tunnel.ping"}"#;
/// LTE paths can stall transiently under output bursts without being truly dead.
/// Keep the local socket/write deadlines comfortably above the relay's liveness
/// window so the device does not self-abort first.
const TUNNEL_TCP_USER_TIMEOUT_MS: libc::c_int = 15_000;
const TUNNEL_WRITER_SEND_TIMEOUT_SECS: u64 = 20;
/// Coalesce adjacent PTY output chunks into larger tunnel frames. LTE links are
/// much less tolerant of hundreds of tiny JSON WS frames than a handful of
/// larger ones carrying the same bytes.
const TUNNEL_STREAM_BATCH_MAX_ENTRIES: usize = 32;
const TUNNEL_STREAM_BATCH_MAX_BYTES: usize = 8 * 1024;
// Flap detection: track last N connection durations. If recent connections
// are all short-lived, extend backoff to avoid hammering the relay.
const FLAP_WINDOW: usize = 10;
const FLAP_THRESHOLD_SECS: u64 = 30;
const FLAP_CHECK_COUNT: usize = 3;
/// At most one re-home per this interval: a route that keeps changing its
/// mind must not become a stream of reconnects.
const REHOME_MIN_INTERVAL: Duration = Duration::from_secs(5);
/// A wait to redial that a network change cuts short still lasts this long,
/// so a burst of network events cannot become a burst of dials.
const EARLY_DIAL_FLOOR: Duration = Duration::from_secs(1);

enum TunnelIo {
    Plain(TcpStream),
    // Boxed: a `TlsStream` is ~1 KiB, dwarfing the bare `TcpStream`, so inlining
    // it would bloat every `TunnelIo` (and its enclosing futures) to that size.
    Tls(Box<TlsStream<TcpStream>>),
}

impl AsyncRead for TunnelIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match &mut *self {
            Self::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            Self::Tls(stream) => Pin::new(&mut **stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for TunnelIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match &mut *self {
            Self::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            Self::Tls(stream) => Pin::new(&mut **stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match &mut *self {
            Self::Plain(stream) => Pin::new(stream).poll_flush(cx),
            Self::Tls(stream) => Pin::new(&mut **stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match &mut *self {
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::Tls(stream) => Pin::new(&mut **stream).poll_shutdown(cx),
        }
    }
}

/// Resolve a `bind_address` config value to a concrete IP address.
///
/// Accepts either:
/// - A literal IP address (e.g. `"10.180.41.231"`) — returned as-is
/// - A network interface name (e.g. `"wwan0"`) — resolved to its current IPv4
///
/// Returns `None` if an interface name was given but the interface is down,
/// missing, or has no IPv4 address assigned.
#[cfg(unix)]
fn resolve_bind_address(value: &str) -> Option<std::net::IpAddr> {
    if let Ok(ip) = value.parse::<std::net::IpAddr>() {
        return Some(ip);
    }
    crate::netwatch::source::interface_ipv4(value).map(std::net::IpAddr::V4)
}

/// Probe whether a local IP address is currently available for binding.
async fn is_local_address_available(addr: &std::net::IpAddr) -> bool {
    tokio::net::UdpSocket::bind(SocketAddr::new(*addr, 0))
        .await
        .is_ok()
}

/// Channel-based WS sender with separate lanes for control, request/ack, and
/// stream output traffic. This keeps liveness and acks responsive even when a
/// PTY is producing a large amount of output.
#[derive(Clone)]
#[allow(clippy::struct_field_names)]
struct WsSink {
    priority_tx: mpsc::Sender<tokio_tungstenite::tungstenite::Message>,
    request_tx: mpsc::Sender<tokio_tungstenite::tungstenite::Message>,
    stream_tx: mpsc::Sender<tokio_tungstenite::tungstenite::Message>,
}

/// Spawn the tunnel client task. Returns a `JoinHandle` that runs until cancelled.
pub fn spawn(state: AppState, tunnel_config: TunnelConfig) -> tokio::task::JoinHandle<()> {
    tokio::spawn(tunnel_client_loop(state, tunnel_config))
}

/// Main loop: connect, handle messages, reconnect on failure.
async fn tunnel_client_loop(state: AppState, config: TunnelConfig) {
    let relay_url = config
        .url
        .as_deref()
        .expect("tunnel.url must be set for client mode");
    let mut backoff = Duration::from_secs(config.reconnect_delay_secs);
    let max_backoff = Duration::from_secs(config.reconnect_max_delay_secs);
    let mut reconnects: u64 = 0;
    let mut flap = FlapWindow::default();
    // Network changes move the tunnel when the kernel's route to the relay
    // moves, and cut a wait to redial short. A bind_address pins the tunnel
    // to one interface on purpose, so a pinned tunnel does not watch.
    let mut net = if config.bind_address.is_none() {
        state.netwatch.clone()
    } else {
        None
    };
    // Where the relay was last reached, and when the tunnel last re-homed.
    let mut relay: Option<SocketAddrV4> = None;
    let mut last_rehome: Option<Instant> = None;
    // With relay_route on, the relay's known address stands in for its name
    // when the name does not resolve.
    let owns_route = state
        .relay_route
        .as_ref()
        .is_some_and(|route| route.mode() != RelayRouteMode::Off);
    let relay_port = url_host_port(relay_url).1;
    // A loop restarted after a panic starts with no connection.
    state.tunnel_stats.set_path(None);

    loop {
        info!("Tunnel: connecting to relay at {relay_url}");
        state
            .tunnel_stats
            .push_event(
                TunnelEventType::ReconnectAttempt,
                format!("attempt #{reconnects}"),
            )
            .await;
        let mut escalate_backoff = false;
        let connect_start = Instant::now();
        // Where this attempt goes as far as is known before it runs: where
        // the relay was last reached or, with relay_route on and after a
        // restart, where the route the earlier run left points.
        let known = relay.or_else(|| {
            owns_route
                .then(|| own_relay_route(net.as_ref()))
                .flatten()
                .map(|ip| SocketAddrV4::new(ip, relay_port))
        });
        // The source this attempt leaves from, for when it fails before it
        // has a connection to take it from.
        let dialed_from = known
            .filter(|_| net.is_some())
            .and_then(|relay| source::source_for(relay).ok());
        let fallback = known.filter(|_| owns_route).map(|relay| *relay.ip());
        state
            .tunnel_stats
            .reconnecting
            .store(true, Ordering::Relaxed);
        let result = connect_and_run(&state, &config, relay_url, &mut net, fallback).await;
        state
            .tunnel_stats
            .reconnecting
            .store(false, Ordering::Relaxed);
        // What a dead way to the relay causes, as opposed to the relay
        // answering (a close, a shutdown, a rejected key) or a move.
        let path_failed = matches!(
            result,
            Ok(DisconnectReason::PongTimeout
                | DisconnectReason::ReadError
                | DisconnectReason::WriterExit)
                | Err(ConnectError::Path(_))
        );
        // The connection is gone; keep where the relay was and which address
        // the attempt left from.
        let (from, dialed) = match state.tunnel_stats.set_path(None) {
            Some(TunnelPath {
                local: SocketAddr::V4(local),
                remote: SocketAddr::V4(remote),
                ..
            }) => {
                relay = Some(remote);
                (Some(*local.ip()), Some(remote))
            }
            _ => (dialed_from, known),
        };
        if path_failed {
            if let Some(route) = &state.relay_route {
                route.signal(TunnelSignal::Failed {
                    from,
                    relay: dialed,
                });
            }
        }
        let class = match result {
            Ok(DisconnectReason::Rehome) => {
                // The move itself was logged and recorded as it was decided.
                let wait = rehome_wait(last_rehome.map(|at| at.elapsed()));
                last_rehome = Some(Instant::now() + wait);
                DelayClass::Rehome(wait)
            }
            Ok(DisconnectReason::RelayShutdown) => {
                info!("Tunnel: relay shutting down, reconnecting...");
                state
                    .tunnel_stats
                    .push_event(TunnelEventType::Disconnected, "relay shutdown".into())
                    .await;
                DelayClass::RelayShutdown
            }
            Ok(
                reason @ (DisconnectReason::WsClose
                | DisconnectReason::PongTimeout
                | DisconnectReason::WriterExit
                | DisconnectReason::ReadError),
            ) => {
                info!("Tunnel: disconnected (reason: {reason}), reconnecting...");
                state
                    .tunnel_stats
                    .push_event(TunnelEventType::Disconnected, reason.to_string())
                    .await;
                DelayClass::CleanClose
            }
            Err(ConnectError::AuthRejected(msg)) => {
                // Never exit: a rejected key usually means the relay's key was
                // rotated ahead of the fleet. The device that gives up needs a
                // site visit; the device that retries slowly heals the moment
                // the operator restores the key.
                error!(
                    "Tunnel: registration rejected: {msg} — retrying on a slow cadence \
                     (is tunnel_key current?)"
                );
                state
                    .tunnel_stats
                    .push_event(
                        TunnelEventType::Disconnected,
                        format!("auth rejected: {msg}"),
                    )
                    .await;
                DelayClass::AuthRejected
            }
            Err(ConnectError::Transient(e) | ConnectError::Path(e)) => {
                let msg = e.to_string();
                state
                    .tunnel_stats
                    .push_event(TunnelEventType::Disconnected, msg.clone())
                    .await;
                if msg.contains("bind_address") && msg.contains("not available")
                    || msg.contains("Address not available")
                    || msg.contains("os error 99")
                {
                    // Interface is down (EADDRNOTAVAIL) — fixed cadence, no escalation.
                    warn!("Tunnel: bind address unavailable ({msg}), retrying in ~5s");
                    DelayClass::BindUnavailable
                } else {
                    warn!("Tunnel: connection error: {msg}");
                    escalate_backoff = true;
                    DelayClass::Transient(backoff)
                }
            }
        };
        reconnects += 1;
        state
            .tunnel_stats
            .reconnects
            .store(reconnects, std::sync::atomic::Ordering::Relaxed);
        state
            .tunnel_stats
            .connected
            .store(false, std::sync::atomic::Ordering::Relaxed);
        // Reset uptime on disconnect
        state
            .tunnel_stats
            .current_uptime_ms
            .store(0, Ordering::Relaxed);

        // Track connection duration for flap detection
        let class = flap.record(class, connect_start.elapsed().as_secs());
        if class == DelayClass::Flap {
            warn!(
                "Tunnel: flap detected ({FLAP_CHECK_COUNT} connections lasted <{FLAP_THRESHOLD_SECS}s), extending backoff"
            );
            escalate_backoff = false; // don't double-escalate
        }

        let sleep_for = reconnect_delay(class, random_draw());
        info!("Tunnel: next attempt in {:.1}s", sleep_for.as_secs_f64());
        if !class.wakes_on_network_change() {
            tokio::time::sleep(sleep_for).await;
        } else if wait_to_dial(sleep_for, &mut net, relay, from).await {
            info!("Tunnel: the route to the relay changed, dialing now");
        }
        if escalate_backoff {
            backoff = (backoff * 2).min(max_backoff);
        } else {
            backoff = Duration::from_secs(config.reconnect_delay_secs);
        }
    }
}

/// Classification of the next reconnect delay. Every path through the loop
/// maps to exactly one class; [`reconnect_delay`] turns a class plus a random
/// draw into a concrete sleep.
///
/// Jitter exists because the relay is a fan-in point: a relay-side event
/// (shutdown broadcast, crash, restart) disconnects the entire fleet in the
/// same instant, and identical delays would bring the entire fleet back in
/// the same instant too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DelayClass {
    /// `tunnel.relay_shutdown` broadcast — every device heard it at once, so
    /// this class carries the widest spread.
    RelayShutdown,
    /// Individual clean close (ws close, pong timeout, writer exit, read
    /// error). Usually one device, but a relay crash produces these
    /// fleet-wide in the same instant, so a small spread still applies.
    CleanClose,
    /// EADDRNOTAVAIL — the bind interface is down; fixed cadence while the
    /// link comes back.
    BindUnavailable,
    /// Transient connect error, carrying the current exponential backoff
    /// (whose escalation state lives in the loop).
    Transient(Duration),
    /// Flap damping engaged after consecutive short-lived connections.
    Flap,
    /// Registration FORBIDDEN — the relay rejected our tunnel key. Slow
    /// cadence, forever: never stop (recovery is an operator fixing the key,
    /// not a site visit), never hammer (the key may stay wrong for days).
    AuthRejected,
    /// The kernel's route to the relay moved and the tunnel left to follow
    /// it. Carries the rest of the [`REHOME_MIN_INTERVAL`] since the last
    /// re-home, usually zero. No jitter: one device's route moving is not a
    /// fleet-wide event.
    Rehome(Duration),
}

impl DelayClass {
    /// Whether a move of the kernel's route to the relay may cut this wait
    /// short: any wait the path could have caused. That includes flap
    /// damping, since three quick dials with no route engage it and a
    /// returning uplink is exactly what should end that wait; damping still
    /// holds while the route stays put. Another route does not bring a
    /// restarting relay back or fix a rejected key, and the re-home limit
    /// exists to hold dials back.
    fn wakes_on_network_change(self) -> bool {
        matches!(
            self,
            Self::CleanClose | Self::BindUnavailable | Self::Transient(_) | Self::Flap
        )
    }
}

/// Flap detection over the last [`FLAP_WINDOW`] connection durations.
#[derive(Default)]
struct FlapWindow {
    durations: VecDeque<u64>,
}

impl FlapWindow {
    /// Record an attempt that lasted `duration_secs` and ended on `class`.
    /// Returns the class to wait on: [`DelayClass::Flap`] when the last
    /// [`FLAP_CHECK_COUNT`] connections all lasted under
    /// [`FLAP_THRESHOLD_SECS`], else `class`.
    fn record(&mut self, class: DelayClass, duration_secs: u64) -> DelayClass {
        // A re-home is the agent moving on purpose, not a failure: it is
        // neither counted nor damped.
        if matches!(class, DelayClass::Rehome(_)) {
            return class;
        }
        if self.durations.len() >= FLAP_WINDOW {
            self.durations.pop_front();
        }
        self.durations.push_back(duration_secs);
        // An auth-rejected loop is already on a far slower cadence than the
        // flap window: damping must never *shorten* it.
        if class != DelayClass::AuthRejected
            && self.durations.len() >= FLAP_CHECK_COUNT
            && self
                .durations
                .iter()
                .rev()
                .take(FLAP_CHECK_COUNT)
                .all(|&d| d < FLAP_THRESHOLD_SECS)
        {
            return DelayClass::Flap;
        }
        class
    }
}

/// How long a re-home waits, given the time since the last one (None: never).
fn rehome_wait(since_last: Option<Duration>) -> Duration {
    since_last.map_or(Duration::ZERO, |since| {
        REHOME_MIN_INTERVAL.saturating_sub(since)
    })
}

/// The kernel's source address toward the relay when it moved away from
/// `from`, the address the tunnel left from (None: it had none). None when
/// nothing moved, and when the kernel has no route at all: a dial would only
/// fail, and a live connection is better left to its own timeouts.
fn moved_source(from: Option<Ipv4Addr>, now: Option<Ipv4Addr>) -> Option<Ipv4Addr> {
    now.filter(|now| Some(*now) != from)
}

/// The path of a freshly connected tunnel socket.
fn tunnel_path(stream: &TcpStream) -> Option<TunnelPath> {
    let local = stream.local_addr().ok()?;
    let remote = stream.peer_addr().ok()?;
    let dev = match local.ip() {
        IpAddr::V4(ip) => source::interface_with_ipv4(ip),
        IpAddr::V6(_) => None,
    };
    Some(TunnelPath { local, remote, dev })
}

/// The relay's address and the tunnel's own, when the tunnel can follow the
/// kernel's route: the route lookup is IPv4 only.
fn rehome_ends(path: &TunnelPath) -> Option<(SocketAddrV4, Ipv4Addr)> {
    match (path.remote, path.local) {
        (SocketAddr::V4(remote), SocketAddr::V4(local)) => Some((remote, *local.ip())),
        _ => None,
    }
}

/// One end of a move, as `dev address` or the bare address.
fn path_end(dev: Option<&str>, ip: Ipv4Addr) -> String {
    match dev {
        Some(dev) => format!("{dev} {ip}"),
        None => ip.to_string(),
    }
}

/// Sleep `delay` before the next dial, or less when the kernel's route to the
/// relay moves meanwhile (a cable plugged in, LTE attaching): then dial once
/// [`EARLY_DIAL_FLOOR`] has passed since the wait began. `relay` is where the
/// relay was last reached and `from` the address the last attempt left from;
/// with no known relay, any network change counts. Returns true when the wait
/// was cut short. Without a network watch, or before it has published, this
/// is a plain sleep.
async fn wait_to_dial(
    delay: Duration,
    net: &mut Option<NetWatch>,
    relay: Option<SocketAddrV4>,
    from: Option<Ipv4Addr>,
) -> bool {
    let start = tokio::time::Instant::now();
    let deadline = start + delay;
    let early = deadline.min(start + EARLY_DIAL_FLOOR);
    let moved =
        || relay.is_none_or(|relay| moved_source(from, source::source_for(relay).ok()).is_some());
    // Changes already made are judged by the check below, not replayed.
    let watching = net
        .as_mut()
        .is_some_and(|rx| rx.borrow_and_update().is_some());
    // A move while the last attempt was failing counts too.
    if watching && relay.is_some() && moved() {
        tokio::time::sleep_until(early).await;
        return true;
    }
    let sleep = tokio::time::sleep_until(deadline);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            () = &mut sleep => return false,
            _ = netwatch::next_state(net.as_mut()) => {
                if moved() {
                    tokio::time::sleep_until(early).await;
                    return true;
                }
            }
        }
    }
}

/// Map a delay class and a uniform random draw to a concrete delay.
fn reconnect_delay(class: DelayClass, draw: u64) -> Duration {
    /// Uniform in `[base, base + spread_ms]`, millisecond granularity.
    fn uniform(base: Duration, spread_ms: u64, draw: u64) -> Duration {
        base + Duration::from_millis(draw % spread_ms.saturating_add(1))
    }
    match class {
        DelayClass::RelayShutdown => uniform(Duration::from_secs(2), 10_000, draw),
        DelayClass::CleanClose => uniform(Duration::from_secs(1), 3_000, draw),
        DelayClass::BindUnavailable => uniform(Duration::from_secs(5), 2_000, draw),
        // Equal jitter: keep half the deterministic backoff as a floor so
        // escalation still means something, randomize the other half.
        DelayClass::Transient(backoff) => {
            let half = backoff / 2;
            let spread_ms = u64::try_from(half.as_millis()).unwrap_or(u64::MAX);
            uniform(half, spread_ms, draw)
        }
        DelayClass::Flap => uniform(Duration::from_mins(1), 30_000, draw),
        DelayClass::AuthRejected => uniform(Duration::from_mins(5), 600_000, draw),
        DelayClass::Rehome(wait) => wait,
    }
}

/// A uniform-enough random draw without carrying an RNG dependency: every
/// `RandomState` hashes with a fresh key derived from OS entropy plus a
/// per-thread counter, so successive draws differ and distinct devices are
/// decorrelated — which is all jitter needs.
fn random_draw() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish()
}

/// Reason the tunnel connection ended.
enum DisconnectReason {
    /// Relay sent `tunnel.relay_shutdown` — intentional, skip backoff.
    RelayShutdown,
    /// Normal close frame or EOF.
    WsClose,
    /// Heartbeat pong timeout — no response from relay.
    PongTimeout,
    /// Writer task exited (WS send failure or timeout).
    WriterExit,
    /// WS read error.
    ReadError,
    /// The kernel's route to the relay moved to another source address.
    Rehome,
}

impl DisconnectReason {
    fn as_str(&self) -> &'static str {
        match self {
            Self::RelayShutdown => "relay_shutdown",
            Self::WsClose => "ws_close",
            Self::PongTimeout => "pong_timeout",
            Self::WriterExit => "writer_exit",
            Self::ReadError => "read_error",
            Self::Rehome => "rehome",
        }
    }
}

impl std::fmt::Display for DisconnectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Classification of connection errors for backoff strategy.
enum ConnectError {
    /// Registration FORBIDDEN (invalid tunnel key) — retry on the slow
    /// [`DelayClass::AuthRejected`] cadence, forever.
    AuthRejected(String),
    /// DNS timeout, TCP timeout, TLS failure — exponential backoff.
    Transient(Box<dyn std::error::Error + Send + Sync>),
    /// The way to the relay failed: no address answered the dial, or the
    /// connection broke or stalled before registration completed. Retried
    /// exactly like [`ConnectError::Transient`]; the relay route owner also
    /// counts it against the uplink it left from.
    Path(Box<dyn std::error::Error + Send + Sync>),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::AuthRejected(msg) => write!(f, "{msg}"),
            ConnectError::Transient(e) | ConnectError::Path(e) => write!(f, "{e}"),
        }
    }
}

/// Configure TCP keepalive on a connected stream.
///
/// LTE carriers commonly have NAT timeouts of 30-60s. Without keepalive,
/// a silent NAT expiry kills the connection and the relay won't see heartbeats.
/// Parameters: start probing after `idle` seconds, probe every `interval` seconds,
/// give up after `count` failed probes.
///
#[cfg(unix)]
#[allow(clippy::cast_possible_wrap)]
fn set_tcp_keepalive(stream: &TcpStream, idle: u32, interval: u32, count: u32) {
    use std::ptr;

    let fd = stream.as_raw_fd();
    let sz = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    unsafe {
        let enable: libc::c_int = 1;
        let idle = idle as libc::c_int;
        let interval = interval as libc::c_int;
        let count = count as libc::c_int;
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_KEEPALIVE,
            ptr::addr_of!(enable).cast(),
            sz,
        );
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_KEEPIDLE,
            ptr::addr_of!(idle).cast(),
            sz,
        );
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_KEEPINTVL,
            ptr::addr_of!(interval).cast(),
            sz,
        );
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_KEEPCNT,
            ptr::addr_of!(count).cast(),
            sz,
        );
        // TCP_USER_TIMEOUT: abort connection if sent data goes unacknowledged
        // for 15s. On LTE with CGNAT, NAT mappings can silently expire, causing
        // TCP retransmissions to loop for minutes. Without this, send() succeeds
        // into the local buffer but data never reaches the relay, and neither
        // keepalive (requires idle connection) nor application-level timeouts
        // (writer only sees local buffer) can detect it. Worst case detection
        // is heartbeat_interval + 15s.
        let user_timeout = TUNNEL_TCP_USER_TIMEOUT_MS;
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_USER_TIMEOUT,
            ptr::addr_of!(user_timeout).cast(),
            sz,
        );
    }
}

/// Resolve DNS for a `wss://` URL and connect TCP, preferring IPv4 addresses.
///
/// Many embedded devices (LTE/CGNAT) have broken IPv6 routes that cause ~4 minute
/// TCP connect timeouts before falling back to IPv4. By sorting IPv4 first we
/// avoid the delay.
///
/// A resolved relay that no address answers is a [`ConnectError::Path`]; a
/// failed lookup or an unusable `bind_address` is not. With `fallback`, the
/// relay's known address, a failed lookup dials that address instead (see
/// [`dial_addresses`]).
async fn connect_tcp_ipv4_preferred(
    url: &str,
    bind_address: Option<&str>,
    fallback: Option<Ipv4Addr>,
) -> Result<TcpStream, ConnectError> {
    let (host, port) = url_host_port(url);
    let host_port = format!("{host}:{port}");

    // Resolve with timeout — DNS can hang on broken resolvers
    let lookup =
        match tokio::time::timeout(Duration::from_secs(10), tokio::net::lookup_host(&host_port))
            .await
        {
            Ok(Ok(found)) => Ok(found.collect()),
            Ok(Err(e)) => Err(ConnectError::Transient(e.into())),
            Err(_) => Err(ConnectError::Transient(
                format!("DNS lookup timed out (10s) for {host}").into(),
            )),
        };
    let addrs = dial_addresses(
        lookup,
        host,
        fallback.map(|ip| SocketAddr::from((ip, port))),
    )?;

    // Resolve bind address (accepts IP or interface name like "wwan0")
    // We track the interface name separately for SO_BINDTODEVICE — bind() alone
    // only sets the source IP but doesn't control the output interface (routing
    // table still picks the lowest-metric route, usually eth/LAN not wwan/LTE).
    #[cfg(unix)]
    let (bind_addr, bind_iface): (Option<std::net::IpAddr>, Option<String>) = match bind_address {
        Some(s) => {
            let is_iface = s.parse::<std::net::IpAddr>().is_err();
            match resolve_bind_address(s) {
                Some(ip) => {
                    let iface = if is_iface {
                        info!("Tunnel: bind_address '{s}' (interface) resolved to {ip}");
                        Some(s.to_string())
                    } else {
                        info!("Tunnel: bind_address '{s}' (IP literal)");
                        None
                    };
                    (Some(ip), iface)
                }
                None => {
                    return Err(ConnectError::Transient(
                        format!("bind_address '{s}' not available (interface down or no IPv4?)")
                            .into(),
                    ));
                }
            }
        }
        None => (None, None),
    };
    #[cfg(not(unix))]
    let (bind_addr, _bind_iface): (Option<std::net::IpAddr>, Option<String>) = {
        let addr = bind_address
            .map(|s| {
                s.parse::<std::net::IpAddr>()
                    .map_err(|e| format!("invalid bind_address '{s}': {e}"))
            })
            .transpose()
            .map_err(|e| ConnectError::Transient(e.into()))?;
        (addr, None)
    };

    if let Some(ref ba) = bind_addr {
        if !is_local_address_available(ba).await {
            return Err(ConnectError::Transient(
                format!("bind_address {ba} not available (interface down?)").into(),
            ));
        }
    }

    // Try each address with a short timeout
    let mut last_err = None;
    for addr in &addrs {
        let connect_fut = async {
            if let Some(ba) = bind_addr {
                let socket = if addr.is_ipv4() {
                    tokio::net::TcpSocket::new_v4()?
                } else {
                    tokio::net::TcpSocket::new_v6()?
                };

                // SO_BINDTODEVICE forces ALL packets through the named interface,
                // regardless of routing table metrics. Without this, bind() only
                // sets the source IP — the kernel still routes via the lowest-metric
                // interface (typically eth/LAN), causing asymmetric routing failures.
                #[cfg(unix)]
                if let Some(ref iface) = bind_iface {
                    if let Err(err) = source::bind_to_device(&socket, iface) {
                        warn!("Tunnel: SO_BINDTODEVICE({iface}) failed: {err}");
                        return Err(err);
                    }
                    info!("Tunnel: SO_BINDTODEVICE({iface}) set on socket");
                }

                socket.bind(SocketAddr::new(ba, 0))?;
                socket.connect(*addr).await
            } else {
                TcpStream::connect(addr).await
            }
        };

        match tokio::time::timeout(Duration::from_secs(10), connect_fut).await {
            Ok(Ok(stream)) => {
                // TCP keepalive: probe after 15s idle, every 5s, 3 probes before dead.
                // Keeps LTE NAT mappings alive and detects dead connections in ~30s.
                #[cfg(unix)]
                set_tcp_keepalive(&stream, 15, 5, 3);
                // Disable Nagle — send small WS frames (heartbeat pings) immediately
                // rather than buffering. Critical on LTE where delayed pings cause
                // relay heartbeat timeouts.
                let _ = stream.set_nodelay(true);
                info!("Tunnel: TCP connected to {addr}");
                return Ok(stream);
            }
            Ok(Err(e)) => {
                warn!("Tunnel: TCP connect to {addr} failed: {e}");
                last_err = Some(e.into());
            }
            Err(_) => {
                warn!("Tunnel: TCP connect to {addr} timed out (10s)");
                last_err = Some(format!("connect to {addr} timed out").into());
            }
        }
    }

    Err(ConnectError::Path(
        last_err.unwrap_or_else(|| "all addresses failed".into()),
    ))
}

/// The host and port of a `ws://` or `wss://` URL, the port defaulting to
/// 443 for `wss://` and 80 otherwise.
fn url_host_port(url: &str) -> (&str, u16) {
    let without_scheme = url
        .strip_prefix("wss://")
        .or_else(|| url.strip_prefix("ws://"))
        .unwrap_or(url);
    let authority = without_scheme.split('/').next().unwrap_or(without_scheme);
    let default_port = if url.starts_with("wss://") { 443 } else { 80 };
    match authority.rfind(':') {
        Some(colon) => match authority[colon + 1..].parse::<u16>() {
            Ok(port) => (&authority[..colon], port),
            Err(_) => (authority, default_port),
        },
        None => (authority, default_port),
    }
}

/// The addresses to dial, IPv4 first: what the lookup found or, when it found
/// nothing, `fallback`, the relay's known address. The resolver's upstream is
/// usually reached through the same uplink as the relay, so when that uplink
/// dies the name stops resolving too; dialing the known address is what then
/// fails as a path failure the relay route owner counts, and what reaches the
/// relay once the route has moved.
fn dial_addresses(
    lookup: Result<Vec<SocketAddr>, ConnectError>,
    host: &str,
    fallback: Option<SocketAddr>,
) -> Result<Vec<SocketAddr>, ConnectError> {
    let resolved = lookup.and_then(|mut addrs| {
        // Sort: IPv4 first, then IPv6
        addrs.sort_by_key(|a| i32::from(!a.is_ipv4()));
        if addrs.is_empty() {
            Err(ConnectError::Transient(
                format!("DNS resolution failed for {host}").into(),
            ))
        } else {
            Ok(addrs)
        }
    });
    match (resolved, fallback) {
        (Err(e), Some(addr)) => {
            warn!("Tunnel: {e}; dialing the relay's known address {addr}");
            Ok(vec![addr])
        }
        (resolved, _) => resolved,
    }
}

/// Where sctl's own route to the relay points (`[tunnel] relay_route`): the
/// relay's address as an earlier run of the agent reached it.
fn own_relay_route(net: Option<&NetWatch>) -> Option<Ipv4Addr> {
    let state = net?.borrow().clone()?;
    state
        .host_routes
        .iter()
        .find(|r| r.metric == 0 && r.protocol == RTPROT_SCTL)
        .map(|r| r.dst)
}

fn tunnel_url_host(url: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let without_scheme = url
        .strip_prefix("wss://")
        .or_else(|| url.strip_prefix("ws://"))
        .unwrap_or(url);
    let authority = without_scheme.split('/').next().unwrap_or(without_scheme);
    if authority.is_empty() {
        return Err("tunnel URL host is empty".into());
    }
    if let Some(rest) = authority.strip_prefix('[') {
        let end = rest
            .find(']')
            .ok_or("invalid bracketed IPv6 address in tunnel URL")?;
        return Ok(rest[..end].to_string());
    }
    if let Some((host, port)) = authority.rsplit_once(':') {
        if port.parse::<u16>().is_ok() {
            return Ok(host.to_string());
        }
    }
    Ok(authority.to_string())
}

fn parse_sha256_pin(pin: &str) -> Result<[u8; 32], String> {
    let mut digits = Vec::with_capacity(64);
    for ch in pin.chars() {
        if ch == ':' || ch.is_ascii_whitespace() {
            continue;
        }
        let digit = ch
            .to_digit(16)
            .ok_or_else(|| format!("invalid SHA-256 pin character '{ch}'"))?;
        digits.push(u8::try_from(digit).expect("hex digit fits in u8"));
    }
    if digits.len() != 64 {
        return Err(format!(
            "SHA-256 pin must contain 64 hex digits, got {}",
            digits.len()
        ));
    }
    let mut out = [0u8; 32];
    for (idx, pair) in digits.chunks_exact(2).enumerate() {
        out[idx] = (pair[0] << 4) | pair[1];
    }
    Ok(out)
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

fn build_tunnel_tls_config(
    config: &TunnelConfig,
) -> Result<Arc<RustlsClientConfig>, Box<dyn std::error::Error + Send + Sync>> {
    let mut root_store = RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    if let Some(path) = config.tls_ca_file.as_deref() {
        let certs = CertificateDer::pem_file_iter(path)
            .map_err(|e| format!("failed to open tunnel TLS CA file {path}: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("failed to parse tunnel TLS CA file {path}: {e}"))?;
        if certs.is_empty() {
            return Err(format!("tunnel TLS CA file {path} contains no certificates").into());
        }
        let total = certs.len();
        let (added, ignored) = root_store.add_parsable_certificates(certs);
        if added == 0 {
            return Err(
                format!("tunnel TLS CA file {path} contains no usable certificates").into(),
            );
        }
        if ignored > 0 {
            warn!("Tunnel: added {added}/{total} certificates from {path} ({ignored} ignored)");
        } else {
            info!("Tunnel: added {added} certificate(s) from {path}");
        }
    }

    Ok(Arc::new(
        RustlsClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth(),
    ))
}

fn verify_tls_server_pin(
    stream: &TlsStream<TcpStream>,
    expected_pin: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let expected = parse_sha256_pin(expected_pin)?;
    let (_, session) = stream.get_ref();
    let certs = session
        .peer_certificates()
        .ok_or("tunnel TLS peer did not provide certificates")?;
    let leaf = certs
        .first()
        .ok_or("tunnel TLS peer certificate chain is empty")?;
    let actual = Sha256::digest(leaf.as_ref());
    if actual.as_slice() != expected {
        return Err(format!(
            "tunnel TLS server certificate pin mismatch: expected {}, got {}",
            hex_lower(&expected),
            hex_lower(actual.as_slice())
        )
        .into());
    }
    Ok(())
}

async fn connect_tunnel_io(
    url: &str,
    tcp_stream: TcpStream,
    config: &TunnelConfig,
) -> Result<TunnelIo, Box<dyn std::error::Error + Send + Sync>> {
    if !url.starts_with("wss://") {
        return Ok(TunnelIo::Plain(tcp_stream));
    }
    Ok(TunnelIo::Tls(Box::new(
        tls_handshake(url, tcp_stream, config).await?,
    )))
}

/// The tunnel's TLS handshake over `tcp_stream`: the host of `url` as the
/// server name (SNI), the public roots plus `tls_ca_file`, then the
/// `tls_server_cert_sha256` pin. The relay route owner's probe makes the
/// same handshake, so an uplink that breaks TLS fails the probe as it fails
/// the tunnel. A handshake that broke on the wire, or met a certificate from
/// something other than the relay, is an `io::Error`; a bad CA file or server
/// name is not.
pub(crate) async fn tls_handshake(
    url: &str,
    tcp_stream: TcpStream,
    config: &TunnelConfig,
) -> Result<TlsStream<TcpStream>, Box<dyn std::error::Error + Send + Sync>> {
    let host = tunnel_url_host(url)?;
    let server_name = ServerName::try_from(host.as_str())
        .map_err(|_| format!("invalid TLS server name in tunnel URL: {host}"))?
        .to_owned();
    let connector = TlsConnector::from(build_tunnel_tls_config(config)?);
    let tls_stream = connector.connect(server_name, tcp_stream).await?;
    if let Some(pin) = config.tls_server_cert_sha256.as_deref() {
        verify_tls_server_pin(&tls_stream, pin)?;
    }
    Ok(tls_stream)
}

/// Panic-path cleanup for `connect_and_run`: mirrors its normal-exit cleanup
/// so an unwinding panic cannot leak the connection's heartbeat, writer and
/// subscriber tasks, attached sessions or unpaused transfers into the
/// respawned client. The normal exit path disarms it and cleans up inline.
struct CleanupGuard {
    armed: bool,
    heartbeat: tokio::task::AbortHandle,
    writer: tokio::task::AbortHandle,
    net_state: Option<tokio::task::AbortHandle>,
    infra_state: Option<tokio::task::AbortHandle>,
    subscribers: Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
    session_manager: crate::sessions::SessionManager,
    transfer_manager: Arc<crate::gawdxfer::manager::TransferManager>,
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        warn!("Tunnel: connection loop unwinding, running panic-path cleanup");
        self.heartbeat.abort();
        self.writer.abort();
        if let Some(net_state) = &self.net_state {
            net_state.abort();
        }
        if let Some(infra_state) = &self.infra_state {
            infra_state.abort();
        }
        let subscribers = self.subscribers.clone();
        let sessions = self.session_manager.clone();
        let transfers = self.transfer_manager.clone();
        tokio::spawn(async move {
            let ids: Vec<String> = {
                let tasks = subscribers.lock().await;
                for task in tasks.values() {
                    task.abort();
                }
                tasks.keys().cloned().collect()
            };
            if !ids.is_empty() {
                sessions.detach_all(&ids).await;
            }
            transfers.pause_all().await;
        });
    }
}

/// A single connection attempt: connect, register, handle messages until
/// disconnect. With `net`, a network change that moves the kernel's route to
/// the relay away from the tunnel's address ends it with
/// [`DisconnectReason::Rehome`]. `fallback` is dialed when the relay's name
/// does not resolve.
async fn connect_and_run(
    state: &AppState,
    config: &TunnelConfig,
    relay_url: &str,
    net: &mut Option<NetWatch>,
    fallback: Option<Ipv4Addr>,
) -> Result<DisconnectReason, ConnectError> {
    // The key's real home is the Authorization header (added at the WS
    // handshake below). It ALSO still rides the query because a payload-#1
    // device may be talking to a pre-0.6.0 relay that only reads `?token=`;
    // the query copy is deleted in 0.7.0 once the fleet's relay is current,
    // which is what finally keeps keys out of fronting-proxy access logs.
    let url = format!(
        "{}?token={}&serial={}",
        relay_url, config.tunnel_key, state.config.device.serial
    );

    let connect_start = Instant::now();

    // DNS + TCP with IPv4 preference (avoids long IPv6 timeouts on LTE/CGNAT)
    let tcp_stream =
        connect_tcp_ipv4_preferred(&url, config.bind_address.as_deref(), fallback).await?;
    let tcp_elapsed = connect_start.elapsed();
    let path = tunnel_path(&tcp_stream);
    if let Some(ref p) = path {
        info!(
            "Tunnel: path {} -> {} on {}",
            p.local,
            p.remote,
            p.dev.as_deref().unwrap_or("?")
        );
    }
    state.tunnel_stats.set_path(path.clone());
    // Followed only when the kernel's lookup agrees with the address it just
    // gave this connection. When it does not (a multipath route can answer
    // each lookup differently), a comparison would move a healthy tunnel.
    let rehome_from = path
        .as_ref()
        .and_then(rehome_ends)
        .filter(|_| net.is_some())
        .filter(|&(relay, local)| {
            let agrees = source::source_for(relay).ok() == Some(local);
            if !agrees {
                warn!(
                    "Tunnel: the kernel's route to the relay does not name {local}; \
                     route changes will not move this connection"
                );
            }
            agrees
        });

    // TLS + WebSocket handshake with timeout (can hang on riscv64/slow networks)
    let tls_start = Instant::now();
    let tunnel_io = tokio::time::timeout(
        Duration::from_secs(15),
        connect_tunnel_io(&url, tcp_stream, config),
    )
    .await
    .map_err(|_| ConnectError::Path("TLS handshake timed out (15s)".into()))?
    .map_err(|e| {
        // A handshake that broke on the wire (or met a certificate from
        // something other than the relay) is an io::Error; a bad CA file or
        // server name is not, and says nothing about the path.
        if e.is::<std::io::Error>() {
            ConnectError::Path(e)
        } else {
            ConnectError::Transient(e)
        }
    })?;
    let mut ws_request = {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
        url.as_str()
            .into_client_request()
            .map_err(|e| ConnectError::Transient(e.into()))?
    };
    if let Ok(hv) = format!("Bearer {}", config.tunnel_key).parse() {
        ws_request.headers_mut().insert("authorization", hv);
    }
    let (ws_stream, _response) = tokio::time::timeout(
        Duration::from_secs(15),
        tokio_tungstenite::client_async(ws_request, tunnel_io),
    )
    .await
    .map_err(|_| ConnectError::Path("TLS/WS handshake timed out (15s)".into()))?
    .map_err(|e| match &e {
        // The relay validates the tunnel key AT THE UPGRADE (it rides the
        // registration URL), so a rotated key surfaces as an HTTP 401/403
        // rejection here — not as the in-band FORBIDDEN frame below. Both
        // must land on the slow AuthRejected cadence; classifying this as
        // transient would keep a rejected fleet hammering on flap cadence.
        // (Observed live: local relay with a rotated key → "HTTP error:
        // 403 Forbidden" → flap's 60-90s instead of auth's 5-15min.)
        tokio_tungstenite::tungstenite::Error::Http(resp)
            if matches!(resp.status().as_u16(), 401 | 403) =>
        {
            ConnectError::AuthRejected(format!(
                "relay refused the WS upgrade: HTTP {}",
                resp.status()
            ))
        }
        tokio_tungstenite::tungstenite::Error::Io(_)
        | tokio_tungstenite::tungstenite::Error::ConnectionClosed
        | tokio_tungstenite::tungstenite::Error::AlreadyClosed => ConnectError::Path(e.into()),
        _ => ConnectError::Transient(e.into()),
    })?;
    let tls_elapsed = tls_start.elapsed();

    let (mut raw_ws_sink, mut ws_stream) = ws_stream.split();

    // Send registration directly on the raw sink (before spawning writer task)
    let reg_start = Instant::now();
    {
        let mut reg = json!({
            "type": "tunnel.register",
            "serial": state.config.device.serial,
            "api_key": state.config.auth.api_key,
        });
        // What this device can push. A relay that predates features
        // ignores the field.
        let mut features = Vec::new();
        if state.netwatch.is_some() {
            features.push(net_state::FEATURE);
        }
        if state.infra_state.is_some() {
            features.push(infra_state::FEATURE);
        }
        if !features.is_empty() {
            reg["features"] = json!(features);
        }
        raw_ws_sink
            .send(tokio_tungstenite::tungstenite::Message::Text(
                serde_json::to_string(&reg)
                    .map_err(|e| ConnectError::Transient(e.into()))?
                    .into(),
            ))
            .await
            .map_err(|e| ConnectError::Path(e.into()))?;
    }

    // Whether the relay's ack advertises net.state and infra.state (an
    // older relay's does not).
    let relay_takes_net_state;
    let relay_takes_infra_state;

    // Wait for registration ack with timeout
    match tokio::time::timeout(Duration::from_secs(10), ws_stream.next()).await {
        Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text)))) => {
            match serde_json::from_str::<Value>(&text) {
                Ok(msg) => {
                    let msg_type = msg["type"].as_str().unwrap_or("");
                    match msg_type {
                        "tunnel.register.ack" => {
                            relay_takes_net_state = net_state::advertised(&msg);
                            relay_takes_infra_state = infra_state::advertised(&msg);
                            let reg_elapsed = reg_start.elapsed();
                            let total = connect_start.elapsed();
                            info!(
                                "Tunnel: connected (DNS+TCP: {}ms, TLS+WS: {}ms, reg: {}ms, total: {}ms)",
                                tcp_elapsed.as_millis(),
                                tls_elapsed.as_millis(),
                                reg_elapsed.as_millis(),
                                total.as_millis(),
                            );
                            state
                                .tunnel_stats
                                .connected
                                .store(true, std::sync::atomic::Ordering::Relaxed);
                            state
                                .tunnel_stats
                                .push_event(
                                    TunnelEventType::Connected,
                                    format!("latency {}ms", total.as_millis()),
                                )
                                .await;
                            if let (Some(route), Some((relay, local))) =
                                (&state.relay_route, path.as_ref().and_then(rehome_ends))
                            {
                                route.signal(TunnelSignal::Registered { relay, local });
                            }
                        }
                        "error" => {
                            let code = msg["code"].as_str().unwrap_or("");
                            let message =
                                msg["message"].as_str().unwrap_or("registration rejected");
                            if code == "FORBIDDEN" {
                                return Err(ConnectError::AuthRejected(message.to_string()));
                            }
                            return Err(ConnectError::Transient(
                                format!("Registration error: {message}").into(),
                            ));
                        }
                        _ => {
                            return Err(ConnectError::Transient(
                                format!("Unexpected message during registration: {msg_type}")
                                    .into(),
                            ));
                        }
                    }
                }
                Err(e) => {
                    return Err(ConnectError::Transient(
                        format!("Invalid JSON from relay during registration: {e}").into(),
                    ));
                }
            }
        }
        Ok(Some(Ok(_))) => {
            return Err(ConnectError::Transient(
                "Non-text message during registration".into(),
            ));
        }
        Ok(Some(Err(e))) => {
            return Err(ConnectError::Path(e.into()));
        }
        Ok(None) => {
            return Err(ConnectError::Path(
                "Connection closed during registration".into(),
            ));
        }
        Err(_) => {
            return Err(ConnectError::Path(
                "Registration ack timed out (10s)".into(),
            ));
        }
    }

    // Channel-based writer: bulk traffic goes through ws_sink, while control
    // frames (heartbeat ping/pong) use a small priority lane. Without this,
    // session output can fill the main queue and cause relay pongs to be
    // dropped, which looks like a dead write path and forces a reconnect.
    let (request_tx, mut request_rx) =
        mpsc::channel::<tokio_tungstenite::tungstenite::Message>(256);
    let (stream_tx, mut stream_rx) = mpsc::channel::<tokio_tungstenite::tungstenite::Message>(1024);
    let (priority_tx, mut priority_rx) =
        mpsc::channel::<tokio_tungstenite::tungstenite::Message>(16);
    let ws_sink = WsSink {
        priority_tx: priority_tx.clone(),
        request_tx: request_tx.clone(),
        stream_tx: stream_tx.clone(),
    };
    let (writer_exit_tx, mut writer_exit_rx) = oneshot::channel::<()>();
    let writer_stats = state.tunnel_stats.clone();
    let writer_task = tokio::spawn(async move {
        loop {
            let msg = tokio::select! {
                biased;
                Some(msg) = priority_rx.recv() => msg,
                Some(msg) = request_rx.recv() => msg,
                Some(msg) = stream_rx.recv() => msg,
                else => break,
            };
            match tokio::time::timeout(
                Duration::from_secs(TUNNEL_WRITER_SEND_TIMEOUT_SECS),
                raw_ws_sink.send(msg),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    warn!("Tunnel: writer task WS send failed: {e}");
                    writer_stats
                        .push_event(TunnelEventType::WriterFailed, format!("WS send error: {e}"))
                        .await;
                    break;
                }
                Err(_) => {
                    warn!(
                        timeout_secs = TUNNEL_WRITER_SEND_TIMEOUT_SECS,
                        "Tunnel: writer task WS send timed out"
                    );
                    writer_stats
                        .push_event(
                            TunnelEventType::WriterFailed,
                            format!("WS send timeout ({TUNNEL_WRITER_SEND_TIMEOUT_SECS}s)"),
                        )
                        .await;
                    break;
                }
            }
            writer_stats.messages_sent.fetch_add(1, Ordering::Relaxed);
        }
        warn!("Tunnel: writer task exited");
        let _ = writer_exit_tx.send(());
    });

    // The device's network, pushed on this connection when the relay takes
    // it: now, then on every change that says something new.
    let net_state_task = net_state::spawn_forwarder(
        relay_takes_net_state,
        state.netwatch.clone(),
        ws_sink.request_tx.clone(),
        {
            let tunnel_stats = state.tunnel_stats.clone();
            move |net| {
                let tunnel = tunnel_stats.path();
                async move { net_state::look_up(&net, tunnel).await }
            }
        },
    );

    // The device's Infra results, pushed the same way: now, then whenever a
    // target's status changes or a config is applied.
    let infra_state_task = infra_state::spawn_forwarder(
        relay_takes_infra_state,
        state.infra_state.clone(),
        ws_sink.request_tx.clone(),
    );

    // Subscribe to session lifecycle broadcasts so we can forward them
    let mut broadcast_rx = state.session_events.subscribe();

    // Track subscriber tasks for session output forwarding
    let subscriber_tasks: Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let handler_permits = Arc::new(Semaphore::new(32));

    // Heartbeat failure notification channel
    let (heartbeat_cancel_tx, mut heartbeat_cancel_rx) = watch::channel(false);

    // Pong watchdog — detects one-way TCP failures where sends succeed
    // (data enters local TCP buffer) but never reach the relay.
    let connection_epoch = Instant::now();
    let last_pong_ms = Arc::new(AtomicU64::new(0));
    // Timestamp of last ping sent (ms since connection_epoch), for RTT computation
    let last_ping_sent_ms = Arc::new(AtomicU64::new(0));

    // Heartbeat task — uses a static string to avoid serde allocation per tick.
    // Includes pong watchdog: if no pong arrives within 3× heartbeat interval,
    // the connection is assumed dead and we force a reconnect.
    let heartbeat_sink = ws_sink.priority_tx.clone();
    let heartbeat_interval_secs = state.config.effective_client_heartbeat_interval_secs();
    let heartbeat_interval = Duration::from_secs(heartbeat_interval_secs);
    let pong_timeout_ms = (heartbeat_interval_secs * 3).max(15) * 1000;
    let heartbeat_epoch = connection_epoch;
    let heartbeat_last_pong = last_pong_ms.clone();
    let heartbeat_ping_sent = last_ping_sent_ms.clone();
    let heartbeat_stats = state.tunnel_stats.clone();
    let heartbeat_ws_sink = ws_sink.clone();
    let heartbeat_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(heartbeat_interval);
        loop {
            interval.tick().await;

            // Update uptime counter
            let uptime_ms = heartbeat_epoch.elapsed().as_millis() as u64;
            heartbeat_stats
                .current_uptime_ms
                .store(uptime_ms, Ordering::Relaxed);

            // Pong watchdog: check if relay is actually responding
            let last = heartbeat_last_pong.load(Ordering::Relaxed);
            // Update pong age for health endpoint
            let pong_age = heartbeat_epoch.elapsed().as_millis() as u64;
            heartbeat_stats
                .last_pong_age_ms
                .store(pong_age.saturating_sub(last), Ordering::Relaxed);
            let now_ms = heartbeat_epoch.elapsed().as_millis() as u64;
            if last > 0 && now_ms.saturating_sub(last) > pong_timeout_ms {
                warn!(
                    "Tunnel: no pong for {}ms (limit {}ms), forcing reconnect",
                    now_ms.saturating_sub(last),
                    pong_timeout_ms
                );
                heartbeat_stats
                    .push_event(
                        TunnelEventType::PongTimeout,
                        format!("no pong for {}ms", now_ms.saturating_sub(last)),
                    )
                    .await;
                let _ = heartbeat_cancel_tx.send(true);
                break;
            }

            // 3.4: Monitor writer channel capacity — early congestion indicator
            let request_capacity = heartbeat_ws_sink.request_tx.capacity();
            let stream_capacity = heartbeat_ws_sink.stream_tx.capacity();
            if request_capacity < 64 || stream_capacity < 128 {
                warn!(
                    request_capacity,
                    stream_capacity, "Tunnel: writer channel backpressure"
                );
            }

            match heartbeat_sink.try_send(tokio_tungstenite::tungstenite::Message::Text(
                PING_TEXT.into(),
            )) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!(
                        "Tunnel: heartbeat channel full, writer likely stuck — forcing reconnect"
                    );
                    let _ = heartbeat_cancel_tx.send(true);
                    break;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    warn!("Tunnel: heartbeat channel closed, triggering reconnect");
                    let _ = heartbeat_cancel_tx.send(true);
                    break;
                }
            }
            // Record ping timestamp for RTT calculation on pong
            heartbeat_ping_sent.store(
                heartbeat_epoch.elapsed().as_millis() as u64,
                Ordering::Relaxed,
            );
            tracing::debug!(
                "Tunnel: ping sent (pong age: {}ms)",
                now_ms.saturating_sub(last)
            );
        }
    });

    // Panic-path cleanup. A panic in the select loop below unwinds out of
    // this function into the JoinError-restart supervisor in main; without
    // this guard the heartbeat/writer/subscriber tasks of the dead
    // connection kept running, sessions stayed attached and in-flight
    // transfers were never paused — the cleanup block after the loop only
    // runs on normal exit. Drop runs during the unwind and finishes the
    // job; the normal path disarms the guard first.
    let mut cleanup_guard = CleanupGuard {
        armed: true,
        heartbeat: heartbeat_task.abort_handle(),
        writer: writer_task.abort_handle(),
        net_state: net_state_task
            .as_ref()
            .map(tokio::task::JoinHandle::abort_handle),
        infra_state: infra_state_task
            .as_ref()
            .map(tokio::task::JoinHandle::abort_handle),
        subscribers: subscriber_tasks.clone(),
        session_manager: state.session_manager.clone(),
        transfer_manager: state.transfer_manager.clone(),
    };

    // Periodic reaping of finished subscriber tasks (30s interval)
    let mut reap_interval = tokio::time::interval(Duration::from_secs(30));
    reap_interval.tick().await; // consume the immediate first tick

    let mut disconnect_reason = DisconnectReason::WsClose;

    // Do not auto-subscribe running sessions on reconnect.
    //
    // The relay/browser side re-attaches sessions explicitly when a client is
    // actually watching them. Auto-subscribing every running session here
    // leaks ghost subscribers across tunnel reconnects and can leave PTYs
    // effectively "attached" even when no browser client exists.

    loop {
        tokio::select! {
            msg = ws_stream.next() => {
                let Some(msg) = msg else { break };
                let msg = match msg {
                    Ok(m) => m,
                    Err(e) => {
                        warn!("Tunnel: WS read error: {e}");
                        disconnect_reason = DisconnectReason::ReadError;
                        break;
                    }
                };
                match msg {
                    tokio_tungstenite::tungstenite::Message::Text(text) => {
                        let parsed: Value = match serde_json::from_str(&text) {
                            Ok(v) => v,
                            Err(e) => {
                                warn!("Tunnel: invalid JSON from relay: {e}");
                                continue;
                            }
                        };
                        state.tunnel_stats.messages_received.fetch_add(1, Ordering::Relaxed);
                        // Any message from the relay proves the connection is alive.
                        // Update pong timestamp so the pong watchdog doesn't fire
                        // when relay pongs are queued behind sctlin request bursts.
                        last_pong_ms.store(connection_epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
                        let msg_type = parsed["type"].as_str().unwrap_or("");
                        match msg_type {
                            "tunnel.relay_shutdown" => {
                                info!("Tunnel: relay sent shutdown notification");
                                disconnect_reason = DisconnectReason::RelayShutdown;
                                break;
                            }
                            // Pong: handle inline — must never be blocked by slow handlers
                            "tunnel.pong" => {
                                let ms = connection_epoch.elapsed().as_millis() as u64;
                                last_pong_ms.store(ms, Ordering::Relaxed);
                                state.tunnel_stats.last_pong_age_ms.store(0, Ordering::Relaxed);
                                // Compute RTT from last ping timestamp
                                let ping_ms = last_ping_sent_ms.load(Ordering::Relaxed);
                                if ping_ms > 0 {
                                    let rtt = ms.saturating_sub(ping_ms);
                                    state.tunnel_stats.record_rtt(rtt).await;
                                    tracing::debug!("Tunnel: pong received (RTT {}ms)", rtt);
                                } else {
                                    tracing::debug!("Tunnel: pong received (epoch +{}ms)", ms);
                                }
                            }
                            "tunnel.register.ack" | "ping" => {}
                            // Relay-initiated ping — respond with try_send to never
                            // block the read loop (if channel full, write path is stuck anyway)
                            "tunnel.ping" => {
                                let pong = tokio_tungstenite::tungstenite::Message::Text(
                                    r#"{"type":"tunnel.pong"}"#.into(),
                                );
                                if let Err(e) = ws_sink.priority_tx.try_send(pong) {
                                    warn!("Tunnel: pong dropped (channel: {e}), write path likely stuck");
                                }
                            }
                            // Everything else: spawn as task to keep the read loop responsive.
                            // This prevents slow handlers (exec, file I/O) from blocking
                            // pong reads, which would trigger the pong watchdog.
                            _ => {
                                let st = state.clone();
                                let tx = ws_sink.clone();
                                let tasks = subscriber_tasks.clone();
                                let permits = handler_permits.clone();
                                tokio::spawn(async move {
                                    let _permit = permits.acquire_owned().await.ok();
                                    if let Err(e) = AssertUnwindSafe(
                                        handle_relay_message(&st, &tx, &tasks, parsed)
                                    ).catch_unwind().await {
                                        error!("Panic in tunnel message handler: {e:?}");
                                    }
                                });
                            }
                        }
                    }
                    tokio_tungstenite::tungstenite::Message::Binary(data) => {
                        if let Some((header, payload)) = decode_binary_frame(&data) {
                            let st = state.clone();
                            let tx = ws_sink.clone();
                            let permits = handler_permits.clone();
                            let payload = payload.to_vec();
                            tokio::spawn(async move {
                                let _permit = permits.acquire_owned().await.ok();
                                if let Err(e) = AssertUnwindSafe(
                                    handle_relay_binary(&st, &tx, header, &payload)
                                ).catch_unwind().await {
                                    error!("Panic in tunnel binary handler: {e:?}");
                                }
                            });
                        }
                    }
                    tokio_tungstenite::tungstenite::Message::Close(_) => break,
                    _ => {}
                }
            }
            broadcast_msg = broadcast_rx.recv() => {
                if let Ok(event) = broadcast_msg {
                    // Forward session lifecycle events to relay
                    let text = serde_json::to_string(&event)
                        .unwrap_or_else(|_| r#"{"type":"error","message":"serialize failed"}"#.to_string());
                    if let Err(e) = ws_sink.request_tx.try_send(tokio_tungstenite::tungstenite::Message::Text(
                        text.into(),
                    )) {
                        warn!("Tunnel: session event dropped (channel: {e})");
                    }
                }
            }
            _ = reap_interval.tick() => {
                subscriber_tasks.lock().await.retain(|_, h| !h.is_finished());
            }
            _ = heartbeat_cancel_rx.changed() => {
                warn!("Tunnel: heartbeat failure detected, disconnecting");
                disconnect_reason = DisconnectReason::PongTimeout;
                break;
            }
            _ = &mut writer_exit_rx => {
                warn!("Tunnel: writer task exited, disconnecting");
                disconnect_reason = DisconnectReason::WriterExit;
                break;
            }
            _ = netwatch::next_state(net.as_mut()), if rehome_from.is_some() => {
                let Some((relay, local)) = rehome_from else { continue };
                let now = source::route_to(relay).ok();
                if let Some(to) = moved_source(Some(local), now.as_ref().map(|r| r.source)) {
                    let detail = format!(
                        "{} -> {}",
                        path_end(path.as_ref().and_then(|p| p.dev.as_deref()), local),
                        path_end(now.as_ref().and_then(|r| r.dev.as_deref()), to),
                    );
                    info!("Tunnel: the route to the relay moved ({detail}), re-homing");
                    state
                        .tunnel_stats
                        .push_event(TunnelEventType::Rehome, detail)
                        .await;
                    disconnect_reason = DisconnectReason::Rehome;
                    break;
                }
            }
        }
    }

    // Cleanup (normal exit — the panic path runs CleanupGuard instead)
    cleanup_guard.armed = false;
    heartbeat_task.abort();
    writer_task.abort();
    if let Some(task) = &net_state_task {
        task.abort();
    }
    if let Some(task) = &infra_state_task {
        task.abort();
    }
    let attached_sessions: Vec<String> = {
        let tasks = subscriber_tasks.lock().await;
        let ids = tasks.keys().cloned().collect();
        for task in tasks.values() {
            task.abort();
        }
        ids
    };
    if !attached_sessions.is_empty() {
        state.session_manager.detach_all(&attached_sessions).await;
    }

    // Pause all active transfers on tunnel disconnect
    state.transfer_manager.pause_all().await;

    // Summary log: single parseable line with everything needed for diagnosis
    {
        let duration_secs = connection_epoch.elapsed().as_secs();
        let pong_age_ms = {
            let last = last_pong_ms.load(Ordering::Relaxed);
            if last > 0 {
                (connection_epoch.elapsed().as_millis() as u64).saturating_sub(last)
            } else {
                0
            }
        };
        let msgs_sent = state.tunnel_stats.messages_sent.load(Ordering::Relaxed);
        let msgs_recv = state.tunnel_stats.messages_received.load(Ordering::Relaxed);
        info!(
            duration_secs,
            reason = disconnect_reason.as_str(),
            last_pong_age_ms = pong_age_ms,
            messages_sent = msgs_sent,
            messages_received = msgs_recv,
            "Tunnel: disconnected"
        );
    }

    Ok(disconnect_reason)
}

/// Handle a message from the relay (proxied client request or control message).
async fn handle_relay_message(
    state: &AppState,
    ws_sink: &WsSink,
    subscriber_tasks: &Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
    msg: Value,
) {
    let msg_type = msg["type"].as_str().unwrap_or("");
    let request_id = msg["request_id"].as_str().map(ToString::to_string);

    match msg_type {
        "tunnel.exec" => {
            handle_tunnel_exec(state, ws_sink, &msg, request_id.as_deref()).await;
        }
        "tunnel.exec_batch" => {
            handle_tunnel_exec_batch(state, ws_sink, &msg, request_id.as_deref()).await;
        }
        "tunnel.info" => {
            handle_tunnel_info(state, ws_sink, &msg, request_id.as_deref()).await;
        }
        "tunnel.health" => {
            handle_tunnel_health(state, ws_sink, request_id.as_deref()).await;
        }
        "http.request" => {
            handle_tunnel_http_request(state, ws_sink, &msg, request_id.as_deref()).await;
        }
        "tunnel.diagnostics" => {
            handle_tunnel_diagnostics(state, ws_sink, &msg, request_id.as_deref()).await;
        }
        "tunnel.file.read" => {
            handle_tunnel_file_read(state, ws_sink, &msg, request_id.as_deref()).await;
        }
        "tunnel.file.write" => {
            handle_tunnel_file_write(state, ws_sink, &msg, request_id.as_deref()).await;
        }
        "tunnel.activity" => {
            handle_tunnel_activity(state, ws_sink, &msg, request_id.as_deref()).await;
        }
        "tunnel.sessions" => {
            handle_tunnel_sessions(state, ws_sink, request_id.as_deref()).await;
        }
        "tunnel.shells" => {
            handle_tunnel_shells(state, ws_sink, request_id.as_deref()).await;
        }
        "tunnel.session.signal" => {
            handle_tunnel_session_signal(state, ws_sink, &msg, request_id.as_deref()).await;
        }
        "tunnel.session.kill" => {
            handle_tunnel_session_kill(state, ws_sink, &msg, request_id.as_deref()).await;
        }
        "tunnel.session.patch" => {
            handle_tunnel_session_patch(state, ws_sink, &msg, request_id.as_deref()).await;
        }
        "tunnel.file.delete" => {
            handle_tunnel_file_delete(state, ws_sink, &msg, request_id.as_deref()).await;
        }
        "tunnel.playbooks.list" => {
            handle_tunnel_playbooks_list(state, ws_sink, &msg, request_id.as_deref()).await;
        }
        "tunnel.playbooks.get" => {
            handle_tunnel_playbooks_get(state, ws_sink, &msg, request_id.as_deref()).await;
        }
        "tunnel.playbooks.put" => {
            handle_tunnel_playbooks_put(state, ws_sink, &msg, request_id.as_deref()).await;
        }
        "tunnel.playbooks.delete" => {
            handle_tunnel_playbooks_delete(state, ws_sink, &msg, request_id.as_deref()).await;
        }
        "tunnel.exec_result" => {
            handle_tunnel_exec_result(state, ws_sink, &msg, request_id.as_deref()).await;
        }
        "tunnel.gps" => {
            handle_tunnel_gps(state, ws_sink, request_id.as_deref()).await;
        }
        "tunnel.lte" => {
            handle_tunnel_lte(state, ws_sink, request_id.as_deref()).await;
        }
        "tunnel.lte.bands" => {
            handle_tunnel_lte_bands(state, ws_sink, &msg, request_id.as_deref()).await;
        }
        "tunnel.lte.scan" => {
            handle_tunnel_lte_scan(state, ws_sink, &msg, request_id.as_deref()).await;
        }
        "tunnel.lte.speedtest" => {
            handle_tunnel_lte_speedtest(state, ws_sink, request_id.as_deref()).await;
        }
        // Infra monitoring tunnel messages
        "tunnel.infra.results" => {
            handle_tunnel_infra_results(state, ws_sink, request_id.as_deref()).await;
        }
        "tunnel.infra.discover" => {
            // Spawn as task — scan can take 60s+ and must not block the message loop
            // (otherwise progress queries can't get through)
            let s = state.clone();
            let sink = ws_sink.clone();
            let m = msg.clone();
            let rid = request_id.clone();
            tokio::spawn(async move {
                handle_tunnel_infra_discover(&s, &sink, &m, rid.as_deref()).await;
            });
        }
        "tunnel.infra.discover.progress" => {
            handle_tunnel_infra_discover_progress(state, ws_sink, request_id.as_deref()).await;
        }
        "tunnel.infra.discover.subnets" => {
            handle_tunnel_infra_discover_subnets(ws_sink, request_id.as_deref()).await;
        }
        "tunnel.infra.config" => {
            handle_tunnel_infra_config(state, ws_sink, &msg, request_id.as_deref()).await;
        }
        "tunnel.infra.config.delete" => {
            handle_tunnel_infra_config_delete(state, ws_sink, request_id.as_deref()).await;
        }
        "tunnel.infra.check" => {
            handle_tunnel_infra_check(state, ws_sink, &msg, request_id.as_deref()).await;
        }
        // gawdxfer transfer protocol messages
        "gx.download.init" => {
            handle_gx_download_init(state, ws_sink, &msg, request_id.as_deref()).await;
        }
        "gx.upload.init" => {
            handle_gx_upload_init(state, ws_sink, &msg, request_id.as_deref()).await;
        }
        "gx.chunk.request" => {
            handle_gx_chunk_request(state, ws_sink, &msg, request_id.as_deref()).await;
        }
        "gx.resume" => {
            handle_gx_resume(state, ws_sink, &msg, request_id.as_deref()).await;
        }
        "gx.abort" => {
            handle_gx_abort(state, ws_sink, &msg, request_id.as_deref()).await;
        }
        "gx.status" => {
            handle_gx_status(state, ws_sink, &msg, request_id.as_deref()).await;
        }
        "gx.list" => {
            handle_gx_list(state, ws_sink, request_id.as_deref()).await;
        }
        // Forwarded session.*, shell.*, and job.* messages from clients via relay.
        // GUARD: Any new WS message prefix (e.g. "foo.*") requires adding it here,
        // otherwise tunnel clients won't handle those messages and they'll fall
        // through to the "Unknown tunnel message type" warning below.
        // (job.* was the A′ streaming-jobs prefix that fell through here until added.)
        t if t.starts_with("session.") || t.starts_with("shell.") || t.starts_with("job.") => {
            handle_forwarded_session_message(state, ws_sink, subscriber_tasks, &msg).await;
        }
        // Client WS keep-alive ping — ignore
        "ping" => {}
        _ => {
            warn!(msg_type, "Unknown tunnel message type");
        }
    }
}

/// Build a `HeaderMap` with `x-sctl-client` from the tunnel message's `_source` field.
///
/// If the relay forwarded a `_source` (e.g. `"mcp"`), use that. Otherwise default
/// to `"tunnel"` so route handlers attribute activity to the tunnel.
fn tunnel_headers(msg: &Value) -> axum::http::HeaderMap {
    let mut headers = axum::http::HeaderMap::new();
    let source = msg["_source"].as_str().unwrap_or("tunnel");
    if let Ok(val) = axum::http::HeaderValue::from_str(source) {
        headers.insert("x-sctl-client", val);
    }
    headers
}

/// Send a JSON response back through the tunnel WS channel.
///
/// Fast path uses `try_send` to avoid scheduler hops. If the request lane is
/// full, falls back to async `.send()` so request/ack traffic remains lossless
/// without blocking Tokio worker threads.
#[allow(clippy::needless_pass_by_value)]
async fn send_response_async(ws_sink: &WsSink, msg: Value) -> bool {
    let msg_type = msg["type"].as_str().unwrap_or("unknown");
    let request_id = msg["request_id"].as_str().unwrap_or("");
    let text = serde_json::to_string(&msg)
        .unwrap_or_else(|_| r#"{"type":"error","message":"serialize failed"}"#.to_string());
    let body_len = text.len();
    let msg = tokio_tungstenite::tungstenite::Message::Text(text.into());
    match ws_sink.request_tx.try_send(msg) {
        Ok(()) => true,
        Err(tokio::sync::mpsc::error::TrySendError::Full(msg)) => {
            warn!(
                msg_type,
                request_id,
                body_len,
                "Tunnel: request lane full, awaiting ordered response delivery"
            );
            if let Err(e) = ws_sink.request_tx.send(msg).await {
                warn!(
                    msg_type,
                    request_id,
                    body_len,
                    "Tunnel: async response send failed after backpressure: {e}"
                );
                return false;
            }
            true
        }
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
            warn!(
                msg_type,
                request_id, body_len, "Tunnel: async response dropped (request lane closed)"
            );
            false
        }
    }
}

/// Log a successful exec from a tunnel request (mirrors `routes::exec::log_exec_ok`).
async fn log_tunnel_exec_ok(
    state: &AppState,
    source: activity::ActivitySource,
    command: &str,
    result: &crate::shell::process::ExecResult,
    request_id: Option<String>,
) {
    let activity_id = state
        .activity_log
        .log(
            ActivityType::Exec,
            source,
            activity::truncate_str(command, 80),
            Some(json!({
                "exit_code": result.exit_code,
                "duration_ms": result.duration_ms,
                "stdout_preview": activity::truncate_str(&result.stdout, 200),
                "stderr_preview": activity::truncate_str(&result.stderr, 200),
                "has_full_output": true,
            })),
            request_id,
        )
        .await;
    state
        .exec_results_cache
        .store(CachedExecResult {
            activity_id,
            exit_code: result.exit_code,
            stdout: result.stdout.clone(),
            stderr: result.stderr.clone(),
            duration_ms: result.duration_ms,
            command: command.to_string(),
            status: "ok".to_string(),
            error_message: None,
        })
        .await;
}

/// Log a failed exec from a tunnel request (mirrors `routes::exec::log_exec_err`).
async fn log_tunnel_exec_err(
    state: &AppState,
    source: activity::ActivitySource,
    command: &str,
    status: &str,
    error_msg: &str,
    duration_ms: u64,
    request_id: Option<String>,
) {
    let activity_id = state
        .activity_log
        .log(
            ActivityType::Exec,
            source,
            activity::truncate_str(command, 80),
            Some(json!({
                "exit_code": -1,
                "duration_ms": duration_ms,
                "status": status,
                "error": error_msg,
                "has_full_output": true,
            })),
            request_id,
        )
        .await;
    state
        .exec_results_cache
        .store(CachedExecResult {
            activity_id,
            exit_code: -1,
            stdout: String::new(),
            stderr: error_msg.to_string(),
            duration_ms,
            command: command.to_string(),
            status: status.to_string(),
            error_message: Some(error_msg.to_string()),
        })
        .await;
}

/// Handle tunnel.exec — one-shot command execution
async fn handle_tunnel_exec(
    state: &AppState,
    ws_sink: &WsSink,
    msg: &Value,
    request_id: Option<&str>,
) {
    let command = msg["command"].as_str().unwrap_or("");
    let timeout_ms = msg["timeout_ms"]
        .as_u64()
        .unwrap_or(state.config.server.exec_timeout_ms);
    let shell = msg["shell"]
        .as_str()
        .unwrap_or(&state.config.shell.default_shell);
    let raw_dir = msg["working_dir"]
        .as_str()
        .unwrap_or(&state.config.shell.default_working_dir);
    let expanded_dir = crate::util::expand_tilde(raw_dir);
    let working_dir = expanded_dir.as_ref();
    let env: Option<HashMap<String, String>> = msg
        .get("env")
        .and_then(|v| serde_json::from_value(v.clone()).ok());

    let source = activity::source_from_headers(&tunnel_headers(msg));
    let req_id = request_id.map(ToString::to_string);

    let result = match Box::pin(crate::shell::process::exec_command(
        shell,
        working_dir,
        command,
        timeout_ms,
        env.as_ref(),
    ))
    .await
    {
        Ok(r) => {
            log_tunnel_exec_ok(state, source, command, &r, req_id).await;
            json!({
                "type": "tunnel.exec.result",
                "request_id": request_id,
                "status": 200,
                "body": {
                    "exit_code": r.exit_code,
                    "stdout": r.stdout,
                    "stderr": r.stderr,
                    "duration_ms": r.duration_ms,
                }
            })
        }
        Err(crate::shell::process::ExecError::Timeout) => {
            log_tunnel_exec_err(
                state,
                source,
                command,
                "timeout",
                "Command timed out",
                timeout_ms,
                req_id,
            )
            .await;
            json!({
                "type": "tunnel.exec.result",
                "request_id": request_id,
                "status": 504,
                "body": {"error": "Command timed out", "code": "TIMEOUT"}
            })
        }
        Err(e) => {
            log_tunnel_exec_err(state, source, command, "error", &e.to_string(), 0, req_id).await;
            json!({
                "type": "tunnel.exec.result",
                "request_id": request_id,
                "status": 500,
                "body": {"error": e.to_string(), "code": "EXEC_FAILED"}
            })
        }
    };

    send_response_async(ws_sink, result).await;
}

/// Handle `tunnel.exec_batch` — batch command execution
async fn handle_tunnel_exec_batch(
    state: &AppState,
    ws_sink: &WsSink,
    msg: &Value,
    request_id: Option<&str>,
) {
    let Some(commands) = msg["commands"].as_array() else {
        send_response_async(
            ws_sink,
            json!({
                "type": "tunnel.exec_batch.result",
                "request_id": request_id,
                "status": 400,
                "body": {"error": "commands array is required", "code": "INVALID_REQUEST"}
            }),
        )
        .await;
        return;
    };

    if commands.len() > state.config.server.max_batch_size {
        send_response_async(
            ws_sink,
            json!({
                "type": "tunnel.exec_batch.result",
                "request_id": request_id,
                "status": 400,
                "body": {
                    "error": format!("Too many commands (max {})", state.config.server.max_batch_size),
                    "code": crate::error::codes::BATCH_TOO_LARGE
                }
            }),
        ).await;
        return;
    }

    let default_shell = msg["shell"]
        .as_str()
        .unwrap_or(&state.config.shell.default_shell);
    let default_dir = msg["working_dir"]
        .as_str()
        .unwrap_or(&state.config.shell.default_working_dir);
    let expanded_default_dir = crate::util::expand_tilde(default_dir);
    let batch_env: Option<HashMap<String, String>> = msg
        .get("env")
        .and_then(|v| serde_json::from_value(v.clone()).ok());

    let source = activity::source_from_headers(&tunnel_headers(msg));
    let req_id = request_id.map(ToString::to_string);

    let mut results = Vec::with_capacity(commands.len());
    for cmd in commands {
        let command = cmd["command"].as_str().unwrap_or("");
        let shell = cmd["shell"].as_str().unwrap_or(default_shell);
        let raw_cmd_dir = cmd["working_dir"].as_str().unwrap_or(&expanded_default_dir);
        let expanded_cmd_dir = crate::util::expand_tilde(raw_cmd_dir);
        let working_dir: &str = expanded_cmd_dir.as_ref();
        let timeout = cmd["timeout_ms"]
            .as_u64()
            .unwrap_or(state.config.server.exec_timeout_ms);

        let cmd_env: Option<HashMap<String, String>> = cmd
            .get("env")
            .and_then(|v| serde_json::from_value(v.clone()).ok());

        let merged_env = match (&batch_env, &cmd_env) {
            (None, None) => None,
            (Some(base), None) => Some(base.clone()),
            (None, Some(over)) => Some(over.clone()),
            (Some(base), Some(over)) => {
                let mut merged = base.clone();
                merged.extend(over.iter().map(|(k, v)| (k.clone(), v.clone())));
                Some(merged)
            }
        };

        match Box::pin(crate::shell::process::exec_command(
            shell,
            working_dir,
            command,
            timeout,
            merged_env.as_ref(),
        ))
        .await
        {
            Ok(r) => {
                log_tunnel_exec_ok(state, source, command, &r, req_id.clone()).await;
                results.push(json!({
                    "exit_code": r.exit_code,
                    "stdout": r.stdout,
                    "stderr": r.stderr,
                    "duration_ms": r.duration_ms,
                }));
            }
            Err(crate::shell::process::ExecError::Timeout) => {
                log_tunnel_exec_err(
                    state,
                    source,
                    command,
                    "timeout",
                    "Command timed out",
                    timeout,
                    req_id.clone(),
                )
                .await;
                results.push(json!({
                    "exit_code": -1,
                    "stdout": "",
                    "stderr": "Command timed out",
                    "duration_ms": timeout,
                }));
            }
            Err(e) => {
                log_tunnel_exec_err(
                    state,
                    source,
                    command,
                    "error",
                    &e.to_string(),
                    0,
                    req_id.clone(),
                )
                .await;
                results.push(json!({
                    "exit_code": -1,
                    "stdout": "",
                    "stderr": e.to_string(),
                    "duration_ms": 0,
                }));
            }
        }
    }

    send_response_async(
        ws_sink,
        json!({
            "type": "tunnel.exec_batch.result",
            "request_id": request_id,
            "status": 200,
            "body": {"results": results}
        }),
    )
    .await;
}

/// Handle tunnel.info — system information
async fn handle_tunnel_info(
    state: &AppState,
    ws_sink: &WsSink,
    msg: &Value,
    request_id: Option<&str>,
) {
    let start = std::time::Instant::now();
    let groups = msg["groups"].as_array().map(|items| {
        items
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join(",")
    });
    // Call the info handler directly — it returns JSON
    match crate::routes::info::info(
        axum::extract::State(state.clone()),
        axum::extract::Query(crate::routes::info::InfoQuery { groups }),
    )
    .await
    {
        Ok(axum::Json(body)) => {
            let elapsed = start.elapsed();
            let body_len = serde_json::to_string(&body).map_or(0, |s| s.len());
            info!(
                "tunnel.info: handler completed in {}ms, body={}B, rid={:?}",
                elapsed.as_millis(),
                body_len,
                request_id
            );
            let queued = send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.info.result",
                    "request_id": request_id,
                    "status": 200,
                    "body": body,
                }),
            )
            .await;
            info!(
                queued,
                body_len,
                rid = ?request_id,
                "tunnel.info: response queue result"
            );
        }
        Err(status) => {
            warn!(
                "tunnel.info: handler failed with {} in {}ms",
                status,
                start.elapsed().as_millis()
            );
            let queued = send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.info.result",
                    "request_id": request_id,
                    "status": status.as_u16(),
                    "body": {"error": "Failed to get info"},
                }),
            )
            .await;
            info!(queued, rid = ?request_id, "tunnel.info: error response queue result");
        }
    }
}

/// Handle tunnel.health — health check
async fn handle_tunnel_health(state: &AppState, ws_sink: &WsSink, request_id: Option<&str>) {
    let axum::Json(body) = crate::routes::health::health(axum::extract::State(state.clone())).await;
    send_response_async(
        ws_sink,
        json!({
            "type": "tunnel.health.result",
            "request_id": request_id,
            "status": 200,
            "body": body,
        }),
    )
    .await;
}

/// Handle a generic `http.request` frame by dispatching it into the device's
/// own axum router — the same routing table, extractors, and auth middleware
/// that serve the LAN port. This is what frees the relay from hand-wrapping a
/// named tunnel message per endpoint (and what lets a crash-looping CGNAT
/// device have its safe-mode flag cleared remotely).
async fn handle_tunnel_http_request(
    state: &AppState,
    ws_sink: &WsSink,
    msg: &Value,
    request_id: Option<&str>,
) {
    let mut out = match run_tunnel_http_request(state, msg).await {
        Ok(v) => v,
        Err((status, error)) => json!({ "status": status, "error": error }),
    };
    out["type"] = json!("http.result");
    out["request_id"] = json!(request_id);
    send_response_async(ws_sink, out).await;
}

/// The dispatch core of [`handle_tunnel_http_request`]; errors become
/// `(status, message)` pairs the relay surfaces as catalog-shaped JSON.
async fn run_tunnel_http_request(state: &AppState, msg: &Value) -> Result<Value, (u16, String)> {
    use base64::Engine as _;
    use tower::ServiceExt as _;

    /// Response bodies ride a single tunnel text frame; cap them well below
    /// anything that could wedge the WS writer.
    const MAX_TUNNEL_BODY_BYTES: usize = 8 * 1024 * 1024;

    let path = msg["path"].as_str().unwrap_or_default();
    if !path.starts_with("/api/") {
        return Err((400, "path must start with /api/".to_string()));
    }
    // Streaming endpoints cannot ride a request/response frame: their
    // responses never end. The tunnel's named session/WS machinery is the
    // transport for those.
    if ["/api/ws", "/api/events"].iter().any(|p| {
        path == *p || path.starts_with(&format!("{p}?")) || path.starts_with(&format!("{p}/"))
    }) {
        return Err((
            400,
            "streaming endpoints are not tunnelable via http.request".to_string(),
        ));
    }
    let Some(router) = state.api_router.get() else {
        return Err((503, "router not initialized yet".to_string()));
    };

    let method = axum::http::Method::from_bytes(msg["method"].as_str().unwrap_or("GET").as_bytes())
        .map_err(|_| (400, "invalid method".to_string()))?;
    let uri: axum::http::Uri = path
        .parse()
        .map_err(|_| (400, "invalid path".to_string()))?;
    let body = match msg["body_b64"].as_str() {
        Some(b64) => base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|_| (400, "invalid body_b64".to_string()))?,
        None => Vec::new(),
    };

    let mut req = axum::http::Request::new(axum::body::Body::from(body));
    *req.method_mut() = method;
    *req.uri_mut() = uri;
    if let Some(headers) = msg["headers"].as_object() {
        for (k, v) in headers {
            if let (Ok(name), Some(Ok(value))) = (
                axum::http::HeaderName::from_bytes(k.as_bytes()),
                v.as_str().map(axum::http::HeaderValue::from_str),
            ) {
                req.headers_mut().insert(name, value);
            }
        }
    }

    let resp = router
        .clone()
        .oneshot(req)
        .await
        .map_err(|_| (500, "router dispatch failed".to_string()))?;

    let status = resp.status().as_u16();
    let content_type = resp
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);
    let content_disposition = resp
        .headers()
        .get(axum::http::header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);

    let body = match tokio::time::timeout(
        Duration::from_secs(55),
        axum::body::to_bytes(resp.into_body(), MAX_TUNNEL_BODY_BYTES),
    )
    .await
    {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(_)) => {
            return Err((
                502,
                format!("response body exceeds the {MAX_TUNNEL_BODY_BYTES}-byte tunnel cap"),
            ));
        }
        Err(_) => return Err((504, "response body collection timed out".to_string())),
    };

    Ok(json!({
        "status": status,
        "content_type": content_type,
        "content_disposition": content_disposition,
        "body_b64": base64::engine::general_purpose::STANDARD.encode(&body),
    }))
}

/// Handle tunnel.diagnostics — server diagnostics snapshot
async fn handle_tunnel_diagnostics(
    state: &AppState,
    ws_sink: &WsSink,
    msg: &Value,
    request_id: Option<&str>,
) {
    let log_lines = msg["log_lines"].as_u64().map(|n| n as u32);
    let log_since = msg["log_since"].as_str().map(String::from);
    let query = crate::routes::diagnostics::DiagnosticsQuery {
        log_lines,
        log_since,
    };
    match crate::routes::diagnostics::diagnostics(
        axum::extract::State(state.clone()),
        axum::extract::Query(query),
    )
    .await
    {
        Ok(axum::Json(body)) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.diagnostics.result",
                    "request_id": request_id,
                    "status": 200,
                    "body": body,
                }),
            )
            .await;
        }
        Err(status) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.diagnostics.result",
                    "request_id": request_id,
                    "status": status.as_u16(),
                    "body": {"error": "Failed to get diagnostics"},
                }),
            )
            .await;
        }
    }
}

/// Handle tunnel.file.read — file read or directory list
async fn handle_tunnel_file_read(
    state: &AppState,
    ws_sink: &WsSink,
    msg: &Value,
    request_id: Option<&str>,
) {
    let path = msg["path"].as_str().unwrap_or("");
    let list = msg["list"].as_bool().unwrap_or(false);

    let offset = msg["offset"].as_u64();
    let limit = msg["limit"].as_u64().map(|l| l as usize);

    let query = crate::routes::files::FilesQuery {
        path: path.to_string(),
        list,
        offset,
        limit,
    };

    match crate::routes::files::get_file(
        axum::extract::State(state.clone()),
        tunnel_headers(msg),
        axum::extract::Query(query),
    )
    .await
    {
        Ok(axum::Json(body)) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.file.read.result",
                    "request_id": request_id,
                    "status": 200,
                    "body": body,
                }),
            )
            .await;
        }
        Err((status, axum::Json(body))) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.file.read.result",
                    "request_id": request_id,
                    "status": status.as_u16(),
                    "body": body,
                }),
            )
            .await;
        }
    }
}

/// Handle tunnel.file.write — file write
async fn handle_tunnel_file_write(
    state: &AppState,
    ws_sink: &WsSink,
    msg: &Value,
    request_id: Option<&str>,
) {
    let path = msg["path"].as_str().unwrap_or("").to_string();
    let content = msg["content"].as_str().unwrap_or("").to_string();
    let create_dirs = msg["create_dirs"].as_bool().unwrap_or(false);
    let mode = msg["mode"].as_str().map(ToString::to_string);
    let encoding = msg["encoding"].as_str().map(ToString::to_string);

    let payload = crate::routes::files::FileWriteRequest {
        path,
        content,
        create_dirs,
        mode,
        encoding,
    };

    match crate::routes::files::put_file(
        axum::extract::State(state.clone()),
        tunnel_headers(msg),
        axum::Json(payload),
    )
    .await
    {
        Ok(axum::Json(body)) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.file.write.result",
                    "request_id": request_id,
                    "status": 200,
                    "body": body,
                }),
            )
            .await;
        }
        Err((status, axum::Json(body))) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.file.write.result",
                    "request_id": request_id,
                    "status": status.as_u16(),
                    "body": body,
                }),
            )
            .await;
        }
    }
}

/// Handle tunnel.activity — activity journal read
async fn handle_tunnel_activity(
    state: &AppState,
    ws_sink: &WsSink,
    msg: &Value,
    request_id: Option<&str>,
) {
    let since_id = msg["since_id"].as_u64().unwrap_or(0);
    let limit = usize::try_from(msg["limit"].as_u64().unwrap_or(50)).unwrap_or(50);
    let entries = state
        .activity_log
        .read_since(since_id, limit.min(200))
        .await;

    send_response_async(
        ws_sink,
        json!({
            "type": "tunnel.activity.result",
            "request_id": request_id,
            "status": 200,
            "body": { "entries": entries },
        }),
    )
    .await;
}

/// Handle tunnel.exec_result — cached exec result lookup
async fn handle_tunnel_exec_result(
    state: &AppState,
    ws_sink: &WsSink,
    msg: &Value,
    request_id: Option<&str>,
) {
    let activity_id = msg["activity_id"].as_u64().unwrap_or(0);

    match state.exec_results_cache.get(activity_id).await {
        Some(result) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.exec_result.result",
                    "request_id": request_id,
                    "status": 200,
                    "body": result,
                }),
            )
            .await;
        }
        None => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.exec_result.result",
                    "request_id": request_id,
                    "status": 404,
                    "body": {"error": "Exec result not found or evicted", "code": "NOT_FOUND"},
                }),
            )
            .await;
        }
    }
}

/// Handle tunnel.sessions — REST session list
async fn handle_tunnel_sessions(state: &AppState, ws_sink: &WsSink, request_id: Option<&str>) {
    let axum::Json(body) =
        crate::routes::sessions::list_sessions(axum::extract::State(state.clone())).await;
    send_response_async(
        ws_sink,
        json!({
            "type": "tunnel.sessions.result",
            "request_id": request_id,
            "status": 200,
            "body": body,
        }),
    )
    .await;
}

/// Handle tunnel.shells — shell list
async fn handle_tunnel_shells(state: &AppState, ws_sink: &WsSink, request_id: Option<&str>) {
    let axum::Json(body) =
        crate::routes::shells::list_shells(axum::extract::State(state.clone())).await;
    send_response_async(
        ws_sink,
        json!({
            "type": "tunnel.shells.result",
            "request_id": request_id,
            "status": 200,
            "body": body,
        }),
    )
    .await;
}

/// Handle tunnel.session.signal — signal a session
async fn handle_tunnel_session_signal(
    state: &AppState,
    ws_sink: &WsSink,
    msg: &Value,
    request_id: Option<&str>,
) {
    let session_id = msg["session_id"].as_str().unwrap_or("");
    let signal = msg["signal"].as_i64().unwrap_or(0) as i32;

    let payload = crate::routes::sessions::SignalRequest { signal };
    match crate::routes::sessions::signal_session(
        axum::extract::State(state.clone()),
        axum::extract::Path(session_id.to_string()),
        tunnel_headers(msg),
        axum::Json(payload),
    )
    .await
    {
        Ok(axum::Json(body)) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.session.signal.result",
                    "request_id": request_id,
                    "status": 200,
                    "body": body,
                }),
            )
            .await;
        }
        Err((status, axum::Json(body))) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.session.signal.result",
                    "request_id": request_id,
                    "status": status.as_u16(),
                    "body": body,
                }),
            )
            .await;
        }
    }
}

/// Handle tunnel.session.kill — kill a session
async fn handle_tunnel_session_kill(
    state: &AppState,
    ws_sink: &WsSink,
    msg: &Value,
    request_id: Option<&str>,
) {
    let session_id = msg["session_id"].as_str().unwrap_or("");
    match crate::routes::sessions::kill_session(
        axum::extract::State(state.clone()),
        axum::extract::Path(session_id.to_string()),
        tunnel_headers(msg),
    )
    .await
    {
        Ok(axum::Json(body)) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.session.kill.result",
                    "request_id": request_id,
                    "status": 200,
                    "body": body,
                }),
            )
            .await;
        }
        Err((status, axum::Json(body))) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.session.kill.result",
                    "request_id": request_id,
                    "status": status.as_u16(),
                    "body": body,
                }),
            )
            .await;
        }
    }
}

/// Handle tunnel.session.patch — rename, AI permission, AI status
async fn handle_tunnel_session_patch(
    state: &AppState,
    ws_sink: &WsSink,
    msg: &Value,
    request_id: Option<&str>,
) {
    let session_id = msg["session_id"].as_str().unwrap_or("");
    let patch = crate::routes::sessions::SessionPatch {
        name: msg["name"].as_str().map(ToString::to_string),
        allowed: msg["allowed"].as_bool(),
        working: msg["working"].as_bool(),
        activity: msg["activity"].as_str().map(ToString::to_string),
        message: msg["message"].as_str().map(ToString::to_string),
    };

    match crate::routes::sessions::patch_session(
        axum::extract::State(state.clone()),
        axum::extract::Path(session_id.to_string()),
        axum::Json(patch),
    )
    .await
    {
        Ok(axum::Json(body)) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.session.patch.result",
                    "request_id": request_id,
                    "status": 200,
                    "body": body,
                }),
            )
            .await;
        }
        Err((status, axum::Json(body))) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.session.patch.result",
                    "request_id": request_id,
                    "status": status.as_u16(),
                    "body": body,
                }),
            )
            .await;
        }
    }
}

/// Handle tunnel.file.delete — file deletion
async fn handle_tunnel_file_delete(
    state: &AppState,
    ws_sink: &WsSink,
    msg: &Value,
    request_id: Option<&str>,
) {
    let path = msg["path"].as_str().unwrap_or("").to_string();

    match crate::routes::files::delete_file(
        axum::extract::State(state.clone()),
        tunnel_headers(msg),
        axum::Json(crate::routes::files::FileDeleteRequest { path }),
    )
    .await
    {
        Ok(axum::Json(body)) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.file.delete.result",
                    "request_id": request_id,
                    "status": 200,
                    "body": body,
                }),
            )
            .await;
        }
        Err((status, axum::Json(body))) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.file.delete.result",
                    "request_id": request_id,
                    "status": status.as_u16(),
                    "body": body,
                }),
            )
            .await;
        }
    }
}

/// Handle a binary frame from the relay (gx.chunk for upload).
async fn handle_relay_binary(state: &AppState, ws_sink: &WsSink, header: Value, payload: &[u8]) {
    let msg_type = header["type"].as_str().unwrap_or("");

    match msg_type {
        "gx.chunk" => {
            handle_gx_chunk_receive(state, ws_sink, &header, payload).await;
        }
        _ => {
            warn!(msg_type, "Unknown binary tunnel message type");
        }
    }
}

// ─── gawdxfer tunnel handlers ────────────────────────────────────────────────

/// Handle gx.download.init — init a chunked download.
async fn handle_gx_download_init(
    state: &AppState,
    ws_sink: &WsSink,
    msg: &Value,
    request_id: Option<&str>,
) {
    let path = msg["path"].as_str().unwrap_or("");
    let chunk_size = msg["chunk_size"].as_u64().map(|v| v as u32);

    match state.transfer_manager.init_download(path, chunk_size).await {
        Ok(result) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "gx.download.init.result",
                    "request_id": request_id,
                    "status": 200,
                    "body": serde_json::to_value(&result).unwrap_or_default(),
                }),
            )
            .await;
        }
        Err(e) => {
            send_response_async(
                ws_sink,
                gx_error_response("gx.download.init.result", request_id, &e),
            )
            .await;
        }
    }
}

/// Handle gx.upload.init — init a chunked upload.
async fn handle_gx_upload_init(
    state: &AppState,
    ws_sink: &WsSink,
    msg: &Value,
    request_id: Option<&str>,
) {
    let req = crate::gawdxfer::types::InitUpload {
        path: msg["path"].as_str().unwrap_or("").to_string(),
        filename: msg["filename"].as_str().unwrap_or("").to_string(),
        file_size: msg["file_size"].as_u64().unwrap_or(0),
        file_hash: msg["file_hash"].as_str().unwrap_or("").to_string(),
        chunk_size: msg["chunk_size"].as_u64().unwrap_or(0) as u32,
        total_chunks: msg["total_chunks"].as_u64().unwrap_or(0) as u32,
        mode: msg["mode"].as_str().map(ToString::to_string),
    };

    match state.transfer_manager.init_upload(req).await {
        Ok(result) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "gx.upload.init.result",
                    "request_id": request_id,
                    "status": 200,
                    "body": serde_json::to_value(&result).unwrap_or_default(),
                }),
            )
            .await;
        }
        Err(e) => {
            send_response_async(
                ws_sink,
                gx_error_response("gx.upload.init.result", request_id, &e),
            )
            .await;
        }
    }
}

/// Handle gx.chunk.request — serve a chunk for download (binary response).
async fn handle_gx_chunk_request(
    state: &AppState,
    ws_sink: &WsSink,
    msg: &Value,
    request_id: Option<&str>,
) {
    let transfer_id = msg["transfer_id"].as_str().unwrap_or("");
    let chunk_index = msg["chunk_index"].as_u64().unwrap_or(0) as u32;

    match state
        .transfer_manager
        .serve_chunk(transfer_id, chunk_index)
        .await
    {
        Ok((chunk_header, data)) => {
            // Send as binary frame with chunk metadata in header
            let header = json!({
                "type": "gx.chunk",
                "request_id": request_id,
                "transfer_id": chunk_header.transfer_id,
                "chunk_index": chunk_header.chunk_index,
                "chunk_hash": chunk_header.chunk_hash,
            });
            let frame = encode_binary_frame(&header, &data);
            let msg = tokio_tungstenite::tungstenite::Message::Binary(frame.into());
            match ws_sink.request_tx.try_send(msg) {
                Ok(()) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Full(msg)) => {
                    warn!("Tunnel: binary chunk request lane full, awaiting delivery");
                    if let Err(e) = ws_sink.request_tx.send(msg).await {
                        warn!("Tunnel: binary chunk send failed after backpressure: {e}");
                    }
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    warn!("Tunnel: binary chunk dropped (request lane closed)");
                }
            }
        }
        Err(e) => {
            send_response_async(ws_sink, gx_error_response("gx.chunk.ack", request_id, &e)).await;
        }
    }
}

/// Handle gx.chunk (binary) — receive a chunk for upload.
async fn handle_gx_chunk_receive(
    state: &AppState,
    ws_sink: &WsSink,
    header: &Value,
    payload: &[u8],
) {
    let request_id = header["request_id"].as_str();
    let transfer_id = header["transfer_id"].as_str().unwrap_or("");
    let chunk_index = header["chunk_index"].as_u64().unwrap_or(0) as u32;
    let chunk_hash = header["chunk_hash"].as_str().unwrap_or("");

    match state
        .transfer_manager
        .receive_chunk(transfer_id, chunk_index, chunk_hash, payload)
        .await
    {
        Ok(ack) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "gx.chunk.ack",
                    "request_id": request_id,
                    "body": serde_json::to_value(&ack).unwrap_or_default(),
                }),
            )
            .await;
        }
        Err(e) => {
            send_response_async(ws_sink, gx_error_response("gx.chunk.ack", request_id, &e)).await;
        }
    }
}

/// Handle gx.resume — resume a paused transfer.
async fn handle_gx_resume(
    state: &AppState,
    ws_sink: &WsSink,
    msg: &Value,
    request_id: Option<&str>,
) {
    let transfer_id = msg["transfer_id"].as_str().unwrap_or("");
    match state.transfer_manager.resume(transfer_id).await {
        Ok(result) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "gx.resume.result",
                    "request_id": request_id,
                    "status": 200,
                    "body": serde_json::to_value(&result).unwrap_or_default(),
                }),
            )
            .await;
        }
        Err(e) => {
            send_response_async(
                ws_sink,
                gx_error_response("gx.resume.result", request_id, &e),
            )
            .await;
        }
    }
}

/// Handle gx.abort — abort a transfer.
async fn handle_gx_abort(
    state: &AppState,
    ws_sink: &WsSink,
    msg: &Value,
    request_id: Option<&str>,
) {
    let transfer_id = msg["transfer_id"].as_str().unwrap_or("");
    let reason = msg["reason"].as_str().unwrap_or("remote abort");
    match state.transfer_manager.abort(transfer_id, reason).await {
        Ok(()) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "gx.abort.result",
                    "request_id": request_id,
                    "status": 200,
                    "body": {"ok": true, "transfer_id": transfer_id},
                }),
            )
            .await;
        }
        Err(e) => {
            send_response_async(
                ws_sink,
                gx_error_response("gx.abort.result", request_id, &e),
            )
            .await;
        }
    }
}

/// Handle gx.status — get transfer status.
async fn handle_gx_status(
    state: &AppState,
    ws_sink: &WsSink,
    msg: &Value,
    request_id: Option<&str>,
) {
    let transfer_id = msg["transfer_id"].as_str().unwrap_or("");
    match state.transfer_manager.status(transfer_id).await {
        Ok(result) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "gx.status.result",
                    "request_id": request_id,
                    "status": 200,
                    "body": serde_json::to_value(&result).unwrap_or_default(),
                }),
            )
            .await;
        }
        Err(e) => {
            send_response_async(
                ws_sink,
                gx_error_response("gx.status.result", request_id, &e),
            )
            .await;
        }
    }
}

/// Handle gx.list — list all transfers.
async fn handle_gx_list(state: &AppState, ws_sink: &WsSink, request_id: Option<&str>) {
    let result = state.transfer_manager.list().await;
    send_response_async(
        ws_sink,
        json!({
            "type": "gx.list.result",
            "request_id": request_id,
            "status": 200,
            "body": serde_json::to_value(&result).unwrap_or_default(),
        }),
    )
    .await;
}

/// Build a JSON error response for gx.* messages.
fn gx_error_response(
    result_type: &str,
    request_id: Option<&str>,
    e: &crate::gawdxfer::types::TransferError,
) -> Value {
    let status = match e.code.as_str() {
        "FILE_NOT_FOUND" | "TRANSFER_NOT_FOUND" => 404,
        "PERMISSION_DENIED" => 403,
        "DISK_FULL" => 507,
        "MAX_TRANSFERS" => 429,
        _ => 400,
    };
    json!({
        "type": result_type,
        "request_id": request_id,
        "status": status,
        "body": {
            "error": e.message,
            "code": e.code,
            "transfer_id": e.transfer_id,
            "recoverable": e.recoverable,
        },
    })
}

/// Handle forwarded `session.*` messages from clients through the relay.
///
/// These are the same message types as in `ws/mod.rs` but forwarded over the tunnel.
/// We dispatch to the `SessionManager` and send responses back through the tunnel.
async fn handle_forwarded_session_message(
    state: &AppState,
    ws_sink: &WsSink,
    subscriber_tasks: &Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
    msg: &Value,
) {
    let msg_type = msg["type"].as_str().unwrap_or("");
    let request_id = msg["request_id"].as_str().map(ToString::to_string);

    match msg_type {
        "session.start" => {
            let working_dir = msg["working_dir"].as_str().map(ToString::to_string);
            let persistent = msg["persistent"].as_bool().unwrap_or(false);
            let env: Option<HashMap<String, String>> = msg
                .get("env")
                .and_then(|v| serde_json::from_value(v.clone()).ok());
            let shell = msg["shell"].as_str().map(ToString::to_string);
            let use_pty = msg["pty"].as_bool().unwrap_or(false);
            let name = msg["name"].as_str().map(ToString::to_string);
            let user_allows_ai = msg["user_allows_ai"].as_bool();
            let rows = msg["rows"]
                .as_u64()
                .unwrap_or(u64::from(state.config.server.default_terminal_rows))
                as u16;
            let cols = msg["cols"]
                .as_u64()
                .unwrap_or(u64::from(state.config.server.default_terminal_cols))
                as u16;
            let idle_timeout = msg["idle_timeout"].as_u64().unwrap_or(0);

            let raw_dir = working_dir
                .as_deref()
                .unwrap_or(&state.config.shell.default_working_dir);
            let expanded = crate::util::expand_tilde(raw_dir);
            let dir = expanded.as_ref();
            let sh = shell
                .as_deref()
                .unwrap_or(&state.config.shell.default_shell);
            let allows_ai = user_allows_ai.unwrap_or(true);

            info!(
                request_id = request_id.as_deref().unwrap_or(""),
                shell = sh,
                working_dir = dir,
                pty = use_pty,
                rows,
                cols,
                persistent,
                "Tunnel: session.start received"
            );

            info!(
                request_id = request_id.as_deref().unwrap_or(""),
                shell = sh,
                working_dir = dir,
                "Tunnel: session.start spawning PTY"
            );
            match state
                .session_manager
                .create_session_with_pty(
                    sh,
                    dir,
                    env.as_ref(),
                    persistent,
                    use_pty,
                    rows,
                    cols,
                    idle_timeout,
                    name.as_deref(),
                )
                .await
            {
                Ok((session_id, pid)) => {
                    info!(
                        request_id = request_id.as_deref().unwrap_or(""),
                        session_id = %session_id,
                        pid,
                        "Tunnel: session.start PTY spawn succeeded"
                    );
                    if !allows_ai {
                        let _ = state
                            .session_manager
                            .set_user_allows_ai(&session_id, false)
                            .await;
                    }

                    // Send session.started BEFORE spawning subscriber to avoid
                    // a race where the subscriber grabs ws_sink first and blocks
                    // this response (the subscriber immediately sends shell prompt).
                    let mut resp = json!({
                        "type": "session.started",
                        "session_id": session_id,
                        "pid": pid,
                        "persistent": persistent,
                        "pty": use_pty,
                        "user_allows_ai": allows_ai,
                    });
                    if let Some(n) = name.as_deref() {
                        resp["name"] = json!(n);
                    }
                    if let Some(ref rid) = request_id {
                        resp["request_id"] = json!(rid);
                    }
                    let queued = send_response_async(ws_sink, resp).await;
                    info!(
                        request_id = request_id.as_deref().unwrap_or(""),
                        session_id = %session_id,
                        queued,
                        "Tunnel: session.started queued"
                    );

                    // Now start subscriber for output forwarding
                    if let Some(buffer) = state.session_manager.get_buffer(&session_id).await {
                        let sink_clone = ws_sink.clone();
                        let sid = session_id.clone();
                        let task = tokio::spawn(tunnel_subscriber_task(
                            state.clone(),
                            sid.clone(),
                            buffer,
                            sink_clone,
                            0,
                        ));
                        subscriber_tasks.lock().await.insert(sid, task);
                        info!(
                            request_id = request_id.as_deref().unwrap_or(""),
                            session_id = %session_id,
                            "Tunnel: session.start subscriber task started"
                        );
                    }

                    // Broadcast
                    let mut broadcast = json!({
                        "type": "session.created",
                        "session_id": session_id,
                        "pid": pid,
                        "pty": use_pty,
                        "persistent": persistent,
                        "user_allows_ai": allows_ai,
                    });
                    if let Some(n) = name.as_deref() {
                        broadcast["name"] = json!(n);
                    }
                    let _ = state.session_events.send(broadcast);
                }
                Err(e) => {
                    warn!(
                        request_id = request_id.as_deref().unwrap_or(""),
                        shell = sh,
                        working_dir = dir,
                        error = %e,
                        "Tunnel: session.start PTY spawn failed"
                    );
                    let mut resp = json!({
                        "type": "error",
                        "code": "SESSION_LIMIT",
                        "message": e,
                    });
                    if let Some(ref rid) = request_id {
                        resp["request_id"] = json!(rid);
                    }
                    send_response_async(ws_sink, resp).await;
                }
            }
        }
        "job.start" => {
            let command = msg["command"].as_str().unwrap_or("");
            if command.is_empty() {
                let mut resp = json!({
                    "type": "error",
                    "code": "MISSING_FIELD",
                    "message": "command is required",
                });
                if let Some(ref rid) = request_id {
                    resp["request_id"] = json!(rid);
                }
                send_response_async(ws_sink, resp).await;
            } else {
                let working_dir = msg["working_dir"].as_str().map(ToString::to_string);
                let env: Option<HashMap<String, String>> = msg
                    .get("env")
                    .and_then(|v| serde_json::from_value(v.clone()).ok());
                let shell = msg["shell"].as_str().map(ToString::to_string);
                let name = msg["name"].as_str().map(ToString::to_string);
                let raw_dir = working_dir
                    .as_deref()
                    .unwrap_or(&state.config.shell.default_working_dir);
                let expanded = crate::util::expand_tilde(raw_dir);
                let dir = expanded.as_ref();
                let sh = shell
                    .as_deref()
                    .unwrap_or(&state.config.shell.default_shell);

                info!(
                    request_id = request_id.as_deref().unwrap_or(""),
                    shell = sh,
                    working_dir = dir,
                    "Tunnel: job.start received"
                );

                match state
                    .session_manager
                    .create_job(
                        sh,
                        dir,
                        command,
                        env.as_ref(),
                        name.as_deref(),
                        crate::sessions::JOB_IDLE_TIMEOUT_SECS,
                        state.session_events.clone(),
                    )
                    .await
                {
                    Ok((session_id, pid)) => {
                        info!(
                            request_id = request_id.as_deref().unwrap_or(""),
                            session_id = %session_id,
                            pid,
                            "Tunnel: job.start spawn succeeded"
                        );
                        let mut resp = json!({
                            "type": "session.started",
                            "session_id": session_id,
                            "pid": pid,
                            "persistent": true,
                            "pty": false,
                            "user_allows_ai": true,
                            "created_at": crate::sessions::journal::now_ms(),
                        });
                        if let Some(n) = name.as_deref() {
                            resp["name"] = json!(n);
                        }
                        if let Some(ref rid) = request_id {
                            resp["request_id"] = json!(rid);
                        }
                        send_response_async(ws_sink, resp).await;

                        // Stream job output up the tunnel via the same subscriber
                        // terminals use. No `session.created` broadcast — jobs stay
                        // out of the terminal/tabs UI.
                        if let Some(buffer) = state.session_manager.get_buffer(&session_id).await {
                            let task = tokio::spawn(tunnel_subscriber_task(
                                state.clone(),
                                session_id.clone(),
                                buffer,
                                ws_sink.clone(),
                                0,
                            ));
                            subscriber_tasks
                                .lock()
                                .await
                                .insert(session_id.clone(), task);
                        }
                    }
                    Err(e) => {
                        warn!(
                            request_id = request_id.as_deref().unwrap_or(""),
                            error = %e,
                            "Tunnel: job.start spawn failed"
                        );
                        let mut resp = json!({
                            "type": "error",
                            "code": "SESSION_LIMIT",
                            "message": e,
                        });
                        if let Some(ref rid) = request_id {
                            resp["request_id"] = json!(rid);
                        }
                        send_response_async(ws_sink, resp).await;
                    }
                }
            }
        }
        "session.exec" => {
            let session_id = msg["session_id"].as_str().unwrap_or("");
            let command = msg["command"].as_str().unwrap_or("");
            state.session_manager.touch_ai_activity(session_id).await;
            if let Err(e) = state
                .session_manager
                .exec_command(session_id, command)
                .await
            {
                let mut resp = json!({
                    "type": "error",
                    "code": "SESSION_ERROR",
                    "session_id": session_id,
                    "message": e,
                });
                if let Some(ref rid) = request_id {
                    resp["request_id"] = json!(rid);
                }
                send_response_async(ws_sink, resp).await;
            } else {
                let mut resp = json!({
                    "type": "session.exec.ack",
                    "session_id": session_id,
                });
                if let Some(ref rid) = request_id {
                    resp["request_id"] = json!(rid);
                }
                send_response_async(ws_sink, resp).await;
            }
        }
        "session.stdin" => {
            let session_id = msg["session_id"].as_str().unwrap_or("");
            let data = msg["data"].as_str().unwrap_or("");
            if !session_id.is_empty() {
                state.session_manager.touch_ai_activity(session_id).await;
                if let Err(e) = state
                    .session_manager
                    .send_to_session(session_id, data)
                    .await
                {
                    send_response_async(
                        ws_sink,
                        json!({
                            "type": "error",
                            "code": "SESSION_ERROR",
                            "session_id": session_id,
                            "message": e,
                        }),
                    )
                    .await;
                }
            }
        }
        "session.kill" => {
            let session_id = msg["session_id"].as_str().unwrap_or("");
            if !session_id.is_empty() {
                let found = state.session_manager.kill_session(session_id).await;
                if found {
                    let mut resp = json!({
                        "type": "session.closed",
                        "session_id": session_id,
                        "reason": "killed",
                    });
                    if let Some(ref rid) = request_id {
                        resp["request_id"] = json!(rid);
                    }
                    send_response_async(ws_sink, resp).await;
                    let _ = state.session_events.send(json!({
                        "type": "session.destroyed",
                        "session_id": session_id,
                        "reason": "killed",
                    }));
                    // Abort subscriber
                    if let Some(task) = subscriber_tasks.lock().await.remove(session_id) {
                        task.abort();
                    }
                } else {
                    let mut resp = json!({
                        "type": "error",
                        "code": "SESSION_NOT_FOUND",
                        "session_id": session_id,
                        "message": format!("Session {session_id} not found"),
                    });
                    if let Some(ref rid) = request_id {
                        resp["request_id"] = json!(rid);
                    }
                    send_response_async(ws_sink, resp).await;
                }
            }
        }
        "session.signal" => {
            let session_id = msg["session_id"].as_str().unwrap_or("");
            let signal = msg["signal"].as_i64().unwrap_or(0);
            if !session_id.is_empty() && signal != 0 {
                let signal_i32 = signal as i32;
                match state
                    .session_manager
                    .signal_session(session_id, signal_i32)
                    .await
                {
                    Ok(()) => {
                        let mut resp = json!({
                            "type": "session.signal.ack",
                            "session_id": session_id,
                            "signal": signal,
                        });
                        if let Some(ref rid) = request_id {
                            resp["request_id"] = json!(rid);
                        }
                        send_response_async(ws_sink, resp).await;
                    }
                    Err(e) => {
                        let mut resp = json!({
                            "type": "error",
                            "code": "SESSION_ERROR",
                            "session_id": session_id,
                            "message": e,
                        });
                        if let Some(ref rid) = request_id {
                            resp["request_id"] = json!(rid);
                        }
                        send_response_async(ws_sink, resp).await;
                    }
                }
            }
        }
        "session.attach" => {
            let session_id = msg["session_id"].as_str().unwrap_or("");
            let since = msg["since"].as_u64().unwrap_or(0);
            if !session_id.is_empty() {
                // Abort any existing subscriber for this session
                if let Some(task) = subscriber_tasks.lock().await.remove(session_id) {
                    task.abort();
                }

                if let Some(buffer) = state.session_manager.attach_shared(session_id).await {
                    let (entries, dropped) = {
                        let buf = buffer.lock().await;
                        buf.read_since(since)
                    };
                    let entries_json: Vec<Value> = entries
                        .iter()
                        .map(|e| entry_to_ws_message(session_id, e))
                        .collect();
                    let last_seq = entries.last().map_or(since, |e| e.seq);

                    let mut resp = json!({
                        "type": "session.attached",
                        "session_id": session_id,
                        "entries": entries_json,
                        "dropped": dropped,
                    });
                    if let Some(ref rid) = request_id {
                        resp["request_id"] = json!(rid);
                    }
                    send_response_async(ws_sink, resp).await;

                    // Start subscriber
                    let sink_clone = ws_sink.clone();
                    let sid = session_id.to_string();
                    let task = tokio::spawn(tunnel_subscriber_task(
                        state.clone(),
                        sid.clone(),
                        buffer,
                        sink_clone,
                        last_seq,
                    ));
                    subscriber_tasks.lock().await.insert(sid, task);
                } else {
                    let mut resp = json!({
                        "type": "error",
                        "code": "SESSION_NOT_FOUND",
                        "session_id": session_id,
                        "message": format!("Session {session_id} not found"),
                    });
                    if let Some(ref rid) = request_id {
                        resp["request_id"] = json!(rid);
                    }
                    send_response_async(ws_sink, resp).await;
                }
            }
        }
        "session.detach" => {
            let session_id = msg["session_id"].as_str().unwrap_or("");
            if !session_id.is_empty() {
                // Abort subscriber for this session
                if let Some(task) = subscriber_tasks.lock().await.remove(session_id) {
                    task.abort();
                }
                state.session_manager.detach(session_id).await;
            }
        }
        "session.list" => {
            let items = state.session_manager.list_sessions().await;
            let sessions_json: Vec<Value> = items
                .iter()
                .map(|s| {
                    let mut obj = json!({
                        "session_id": s.session_id,
                        "pid": s.pid,
                        "persistent": s.persistent,
                        "pty": s.pty,
                        "attached": s.attached,
                        "status": s.status,
                        "idle": s.idle,
                        "idle_timeout": s.idle_timeout,
                        "user_allows_ai": s.user_allows_ai,
                        "ai_is_working": s.ai_is_working,
                    });
                    if let Some(ref name) = s.name {
                        obj["name"] = json!(name);
                    }
                    if let Some(ref activity) = s.ai_activity {
                        obj["ai_activity"] = json!(activity);
                    }
                    if let Some(ref msg) = s.ai_status_message {
                        obj["ai_status_message"] = json!(msg);
                    }
                    obj
                })
                .collect();
            let mut resp = json!({
                "type": "session.listed",
                "sessions": sessions_json,
            });
            if let Some(ref rid) = request_id {
                resp["request_id"] = json!(rid);
            }
            send_response_async(ws_sink, resp).await;
        }
        "session.resize" => {
            let session_id = msg["session_id"].as_str().unwrap_or("");
            let rows = msg["rows"].as_u64().unwrap_or(0) as u16;
            let cols = msg["cols"].as_u64().unwrap_or(0) as u16;
            if !session_id.is_empty() && rows > 0 && cols > 0 {
                match state
                    .session_manager
                    .resize_session(session_id, rows, cols)
                    .await
                {
                    Ok(()) => {
                        let mut resp = json!({
                            "type": "session.resize.ack",
                            "session_id": session_id,
                            "rows": rows,
                            "cols": cols,
                        });
                        if let Some(ref rid) = request_id {
                            resp["request_id"] = json!(rid);
                        }
                        send_response_async(ws_sink, resp).await;
                    }
                    Err(e) => {
                        let mut resp = json!({
                            "type": "error",
                            "code": "SESSION_ERROR",
                            "session_id": session_id,
                            "message": e,
                        });
                        if let Some(ref rid) = request_id {
                            resp["request_id"] = json!(rid);
                        }
                        send_response_async(ws_sink, resp).await;
                    }
                }
            }
        }
        "session.allow_ai" => {
            let session_id = msg["session_id"].as_str().unwrap_or("");
            let allowed = msg["allowed"].as_bool();
            if !session_id.is_empty() {
                if let Some(allowed) = allowed {
                    match state
                        .session_manager
                        .set_user_allows_ai(session_id, allowed)
                        .await
                    {
                        Ok(ai_cleared) => {
                            let mut resp = json!({
                                "type": "session.allow_ai.ack",
                                "session_id": session_id,
                                "allowed": allowed,
                            });
                            if let Some(ref rid) = request_id {
                                resp["request_id"] = json!(rid);
                            }
                            send_response_async(ws_sink, resp).await;
                            let _ = state.session_events.send(json!({
                                "type": "session.ai_permission_changed",
                                "session_id": session_id,
                                "allowed": allowed,
                            }));
                            if ai_cleared {
                                let _ = state.session_events.send(json!({
                                    "type": "session.ai_status_changed",
                                    "session_id": session_id,
                                    "working": false,
                                }));
                            }
                        }
                        Err(e) => {
                            let mut resp = json!({
                                "type": "error",
                                "code": "SESSION_NOT_FOUND",
                                "session_id": session_id,
                                "message": e,
                            });
                            if let Some(ref rid) = request_id {
                                resp["request_id"] = json!(rid);
                            }
                            send_response_async(ws_sink, resp).await;
                        }
                    }
                }
            }
        }
        "session.ai_status" => {
            let session_id = msg["session_id"].as_str().unwrap_or("");
            let working = msg["working"].as_bool();
            if !session_id.is_empty() {
                if let Some(working) = working {
                    let activity = msg["activity"].as_str();
                    let message = msg["message"].as_str();
                    match state
                        .session_manager
                        .set_ai_status(session_id, working, activity, message)
                        .await
                    {
                        Ok(()) => {
                            let mut resp = json!({
                                "type": "session.ai_status.ack",
                                "session_id": session_id,
                                "working": working,
                            });
                            if let Some(a) = activity {
                                resp["activity"] = json!(a);
                            }
                            if let Some(m) = message {
                                resp["message"] = json!(m);
                            }
                            if let Some(ref rid) = request_id {
                                resp["request_id"] = json!(rid);
                            }
                            send_response_async(ws_sink, resp).await;
                            let mut broadcast = json!({
                                "type": "session.ai_status_changed",
                                "session_id": session_id,
                                "working": working,
                            });
                            if let Some(a) = activity {
                                broadcast["activity"] = json!(a);
                            }
                            if let Some(m) = message {
                                broadcast["message"] = json!(m);
                            }
                            let _ = state.session_events.send(broadcast);
                        }
                        Err(e) => {
                            let mut resp = json!({
                                "type": "error",
                                "code": "AI_NOT_ALLOWED",
                                "session_id": session_id,
                                "message": e,
                            });
                            if let Some(ref rid) = request_id {
                                resp["request_id"] = json!(rid);
                            }
                            send_response_async(ws_sink, resp).await;
                        }
                    }
                }
            }
        }
        "session.rename" => {
            let session_id = msg["session_id"].as_str().unwrap_or("");
            let name = msg["name"].as_str().unwrap_or("");
            if !session_id.is_empty() && !name.is_empty() {
                match state.session_manager.rename_session(session_id, name).await {
                    Ok(()) => {
                        let mut resp = json!({
                            "type": "session.rename.ack",
                            "session_id": session_id,
                            "name": name,
                        });
                        if let Some(ref rid) = request_id {
                            resp["request_id"] = json!(rid);
                        }
                        send_response_async(ws_sink, resp).await;
                        let _ = state.session_events.send(json!({
                            "type": "session.renamed",
                            "session_id": session_id,
                            "name": name,
                        }));
                    }
                    Err(e) => {
                        let mut resp = json!({
                            "type": "error",
                            "code": "SESSION_NOT_FOUND",
                            "session_id": session_id,
                            "message": e,
                        });
                        if let Some(ref rid) = request_id {
                            resp["request_id"] = json!(rid);
                        }
                        send_response_async(ws_sink, resp).await;
                    }
                }
            }
        }
        "shell.list" => {
            let shells = crate::shell::detect_shells();
            let mut resp = json!({
                "type": "shell.listed",
                "shells": shells,
                "default_shell": &state.config.shell.default_shell,
            });
            if let Some(ref rid) = request_id {
                resp["request_id"] = json!(rid);
            }
            send_response_async(ws_sink, resp).await;
        }
        _ => {
            warn!(msg_type, "Unknown forwarded session message type");
        }
    }
}

/// Convert an `OutputEntry` to a WS JSON message (same as `ws/mod.rs`).
fn entry_to_ws_message(session_id: &str, entry: &OutputEntry) -> Value {
    json!({
        "type": format!("session.{}", entry.stream.as_str()),
        "session_id": session_id,
        "data": entry.data,
        "seq": entry.seq,
        "timestamp_ms": entry.timestamp_ms,
    })
}

fn batch_output_entries(session_id: &str, entries: &[OutputEntry]) -> Vec<String> {
    let mut batched = Vec::new();
    let mut current_stream = None;
    let mut current_data = String::new();
    let mut current_seq = 0u64;
    let mut current_timestamp_ms = 0u64;
    let mut current_count = 0usize;

    let flush_current = |batched: &mut Vec<String>,
                         current_stream: &mut Option<crate::sessions::buffer::OutputStream>,
                         current_data: &mut String,
                         current_seq: &mut u64,
                         current_timestamp_ms: &mut u64,
                         current_count: &mut usize| {
        if let Some(stream) = current_stream.take() {
            let msg = json!({
                "type": format!("session.{}", stream.as_str()),
                "session_id": session_id,
                "data": std::mem::take(current_data),
                "seq": *current_seq,
                "timestamp_ms": *current_timestamp_ms,
            });
            let text = serde_json::to_string(&msg)
                .unwrap_or_else(|_| r#"{"type":"error","message":"serialize failed"}"#.to_string());
            batched.push(text);
            *current_count = 0;
        }
    };

    for entry in entries {
        let same_stream = current_stream == Some(entry.stream);
        let fits_bytes = current_data.len() + entry.data.len() <= TUNNEL_STREAM_BATCH_MAX_BYTES;
        let fits_count = current_count < TUNNEL_STREAM_BATCH_MAX_ENTRIES;
        if !same_stream || !fits_bytes || !fits_count {
            flush_current(
                &mut batched,
                &mut current_stream,
                &mut current_data,
                &mut current_seq,
                &mut current_timestamp_ms,
                &mut current_count,
            );
            current_stream = Some(entry.stream);
        }
        current_data.push_str(&entry.data);
        current_seq = entry.seq;
        current_timestamp_ms = entry.timestamp_ms;
        current_count += 1;
    }

    flush_current(
        &mut batched,
        &mut current_stream,
        &mut current_data,
        &mut current_seq,
        &mut current_timestamp_ms,
        &mut current_count,
    );
    batched
}

/// Handle `tunnel.playbooks.list`
async fn handle_tunnel_playbooks_list(
    state: &AppState,
    ws_sink: &WsSink,
    msg: &Value,
    request_id: Option<&str>,
) {
    match crate::routes::playbooks::list_playbooks(
        axum::extract::State(state.clone()),
        tunnel_headers(msg),
    )
    .await
    {
        Ok(axum::Json(body)) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.playbooks.list.result",
                    "request_id": request_id,
                    "status": 200,
                    "body": body,
                }),
            )
            .await;
        }
        Err((status, axum::Json(body))) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.playbooks.list.result",
                    "request_id": request_id,
                    "status": status.as_u16(),
                    "body": body,
                }),
            )
            .await;
        }
    }
}

/// Handle `tunnel.playbooks.get`
async fn handle_tunnel_playbooks_get(
    state: &AppState,
    ws_sink: &WsSink,
    msg: &Value,
    request_id: Option<&str>,
) {
    let name = msg["name"].as_str().unwrap_or("").to_string();
    match crate::routes::playbooks::get_playbook(
        axum::extract::State(state.clone()),
        axum::extract::Path(name),
        tunnel_headers(msg),
    )
    .await
    {
        Ok(axum::Json(body)) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.playbooks.get.result",
                    "request_id": request_id,
                    "status": 200,
                    "body": body,
                }),
            )
            .await;
        }
        Err((status, axum::Json(body))) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.playbooks.get.result",
                    "request_id": request_id,
                    "status": status.as_u16(),
                    "body": body,
                }),
            )
            .await;
        }
    }
}

/// Handle `tunnel.playbooks.put`
async fn handle_tunnel_playbooks_put(
    state: &AppState,
    ws_sink: &WsSink,
    msg: &Value,
    request_id: Option<&str>,
) {
    let name = msg["name"].as_str().unwrap_or("").to_string();
    let content = msg["content"].as_str().unwrap_or("").to_string();
    match crate::routes::playbooks::put_playbook(
        axum::extract::State(state.clone()),
        axum::extract::Path(name),
        tunnel_headers(msg),
        content,
    )
    .await
    {
        Ok(axum::Json(body)) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.playbooks.put.result",
                    "request_id": request_id,
                    "status": 200,
                    "body": body,
                }),
            )
            .await;
        }
        Err((status, axum::Json(body))) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.playbooks.put.result",
                    "request_id": request_id,
                    "status": status.as_u16(),
                    "body": body,
                }),
            )
            .await;
        }
    }
}

/// Handle `tunnel.playbooks.delete`
async fn handle_tunnel_playbooks_delete(
    state: &AppState,
    ws_sink: &WsSink,
    msg: &Value,
    request_id: Option<&str>,
) {
    let name = msg["name"].as_str().unwrap_or("").to_string();
    match crate::routes::playbooks::delete_playbook(
        axum::extract::State(state.clone()),
        axum::extract::Path(name),
        tunnel_headers(msg),
    )
    .await
    {
        Ok(axum::Json(body)) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.playbooks.delete.result",
                    "request_id": request_id,
                    "status": 200,
                    "body": body,
                }),
            )
            .await;
        }
        Err((status, axum::Json(body))) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.playbooks.delete.result",
                    "request_id": request_id,
                    "status": status.as_u16(),
                    "body": body,
                }),
            )
            .await;
        }
    }
}

/// Handle `tunnel.gps` — GPS location data.
async fn handle_tunnel_gps(state: &AppState, ws_sink: &WsSink, request_id: Option<&str>) {
    match crate::routes::gps::gps(axum::extract::State(state.clone())).await {
        Ok(axum::Json(body)) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.gps.result",
                    "request_id": request_id,
                    "status": 200,
                    "body": body,
                }),
            )
            .await;
        }
        Err((status, axum::Json(body))) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.gps.result",
                    "request_id": request_id,
                    "status": status.as_u16(),
                    "body": body,
                }),
            )
            .await;
        }
    }
}

/// Handle `tunnel.lte` — LTE signal and modem data.
async fn handle_tunnel_lte(state: &AppState, ws_sink: &WsSink, request_id: Option<&str>) {
    match crate::routes::lte::lte(
        axum::extract::State(state.clone()),
        axum::extract::Query(std::collections::HashMap::new()),
    )
    .await
    {
        Ok(axum::Json(body)) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.lte.result",
                    "request_id": request_id,
                    "status": 200,
                    "body": body,
                }),
            )
            .await;
        }
        Err((status, axum::Json(body))) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.lte.result",
                    "request_id": request_id,
                    "status": status.as_u16(),
                    "body": body,
                }),
            )
            .await;
        }
    }
}

/// Handle `tunnel.lte.bands` — set LTE band configuration.
async fn handle_tunnel_lte_bands(
    state: &AppState,
    ws_sink: &WsSink,
    msg: &Value,
    request_id: Option<&str>,
) {
    let body: Value = msg.clone();
    let req: crate::routes::lte::SetBandsRequest = match serde_json::from_value(body) {
        Ok(r) => r,
        Err(e) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.lte.bands.result",
                    "request_id": request_id,
                    "status": 400,
                    "body": {"error": format!("invalid request: {e}")},
                }),
            )
            .await;
            return;
        }
    };

    match crate::routes::lte::set_bands(axum::extract::State(state.clone()), axum::Json(req)).await
    {
        Ok(axum::Json(body)) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.lte.bands.result",
                    "request_id": request_id,
                    "status": 200,
                    "body": body,
                }),
            )
            .await;
        }
        Err((status, axum::Json(body))) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.lte.bands.result",
                    "request_id": request_id,
                    "status": status.as_u16(),
                    "body": body,
                }),
            )
            .await;
        }
    }
}

/// Handle `tunnel.lte.scan` — start a background band scan.
async fn handle_tunnel_lte_scan(
    state: &AppState,
    ws_sink: &WsSink,
    msg: &Value,
    request_id: Option<&str>,
) {
    let body: Value = msg.clone();
    let req: crate::routes::lte::StartScanRequest = match serde_json::from_value(body) {
        Ok(r) => r,
        Err(e) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.lte.scan.result",
                    "request_id": request_id,
                    "status": 400,
                    "body": {"error": format!("invalid request: {e}")},
                }),
            )
            .await;
            return;
        }
    };

    match crate::routes::lte::start_scan(axum::extract::State(state.clone()), axum::Json(req)).await
    {
        Ok(axum::Json(body)) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.lte.scan.result",
                    "request_id": request_id,
                    "status": 200,
                    "body": body,
                }),
            )
            .await;
        }
        Err((status, axum::Json(body))) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.lte.scan.result",
                    "request_id": request_id,
                    "status": status.as_u16(),
                    "body": body,
                }),
            )
            .await;
        }
    }
}

/// Handle `tunnel.lte.speedtest` — run a quick download+upload speed test.
async fn handle_tunnel_lte_speedtest(state: &AppState, ws_sink: &WsSink, request_id: Option<&str>) {
    match crate::routes::lte::speed_test(axum::extract::State(state.clone())).await {
        Ok(axum::Json(body)) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.lte.speedtest.result",
                    "request_id": request_id,
                    "status": 200,
                    "body": body,
                }),
            )
            .await;
        }
        Err((status, axum::Json(body))) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.lte.speedtest.result",
                    "request_id": request_id,
                    "status": status.as_u16(),
                    "body": body,
                }),
            )
            .await;
        }
    }
}

// ─── Infra Monitoring Tunnel Handlers ────────────────────────────────────────

/// Handle `tunnel.infra.results` — return latest monitoring results.
async fn handle_tunnel_infra_results(state: &AppState, ws_sink: &WsSink, request_id: Option<&str>) {
    match crate::infra::routes::get_results(axum::extract::State(state.clone())).await {
        Ok(axum::Json(body)) => {
            let body_value = serde_json::to_value(&body).unwrap_or_default();
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.infra.results.result",
                    "request_id": request_id,
                    "status": 200,
                    "body": body_value,
                }),
            )
            .await;
        }
        Err((status, axum::Json(body))) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.infra.results.result",
                    "request_id": request_id,
                    "status": status.as_u16(),
                    "body": body,
                }),
            )
            .await;
        }
    }
}

/// Handle `tunnel.infra.discover.progress` — return current scan progress.
async fn handle_tunnel_infra_discover_progress(
    state: &AppState,
    ws_sink: &WsSink,
    request_id: Option<&str>,
) {
    let body = crate::infra::routes::discover_progress(axum::extract::State(state.clone())).await;
    send_response_async(
        ws_sink,
        json!({
            "type": "tunnel.infra.discover.progress.result",
            "request_id": request_id,
            "status": 200,
            "body": body.0,
        }),
    )
    .await;
}

/// Handle `tunnel.infra.discover.subnets` — return auto-detected LAN subnets.
async fn handle_tunnel_infra_discover_subnets(ws_sink: &WsSink, request_id: Option<&str>) {
    let (status, body) = match crate::infra::discovery::auto_detect_subnets().await {
        Ok(subnets) => (200, json!({ "subnets": subnets })),
        Err(e) => (503, json!({ "error": e, "reason": "ip_command_failed" })),
    };
    send_response_async(
        ws_sink,
        json!({
            "type": "tunnel.infra.discover.subnets.result",
            "request_id": request_id,
            "status": status,
            "body": body,
        }),
    )
    .await;
}

/// Handle `tunnel.infra.discover` — trigger LAN discovery scan.
async fn handle_tunnel_infra_discover(
    state: &AppState,
    ws_sink: &WsSink,
    msg: &Value,
    request_id: Option<&str>,
) {
    match crate::infra::discovery::discover(
        axum::extract::State(state.clone()),
        axum::Json(msg.clone()),
    )
    .await
    {
        Ok(axum::Json(body)) => {
            let body_value = serde_json::to_value(&body).unwrap_or_default();
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.infra.discover.result",
                    "request_id": request_id,
                    "status": 200,
                    "body": body_value,
                }),
            )
            .await;
        }
        Err((status, axum::Json(body))) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.infra.discover.result",
                    "request_id": request_id,
                    "status": status.as_u16(),
                    "body": body,
                }),
            )
            .await;
        }
    }
}

/// Handle `tunnel.infra.config` — push monitoring config.
async fn handle_tunnel_infra_config(
    state: &AppState,
    ws_sink: &WsSink,
    msg: &Value,
    request_id: Option<&str>,
) {
    // Deserialize InfraConfig from the tunnel message (serde ignores extra fields like type/request_id)
    let config = match serde_json::from_value::<crate::infra::InfraConfig>(msg.clone()) {
        Ok(c) => c,
        Err(e) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.infra.config.result",
                    "request_id": request_id,
                    "status": 400,
                    "body": {"error": format!("Invalid infra config: {e}")},
                }),
            )
            .await;
            return;
        }
    };

    match crate::infra::routes::push_config(axum::extract::State(state.clone()), axum::Json(config))
        .await
    {
        Ok(axum::Json(body)) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.infra.config.result",
                    "request_id": request_id,
                    "status": 200,
                    "body": body,
                }),
            )
            .await;
        }
        Err((status, axum::Json(body))) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.infra.config.result",
                    "request_id": request_id,
                    "status": status.as_u16(),
                    "body": body,
                }),
            )
            .await;
        }
    }
}

/// Handle `tunnel.infra.config.delete` — stop monitoring, remove config.
async fn handle_tunnel_infra_config_delete(
    state: &AppState,
    ws_sink: &WsSink,
    request_id: Option<&str>,
) {
    match crate::infra::routes::delete_config(axum::extract::State(state.clone())).await {
        Ok(axum::Json(body)) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.infra.config.delete.result",
                    "request_id": request_id,
                    "status": 200,
                    "body": body,
                }),
            )
            .await;
        }
        Err((status, axum::Json(body))) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.infra.config.delete.result",
                    "request_id": request_id,
                    "status": status.as_u16(),
                    "body": body,
                }),
            )
            .await;
        }
    }
}

/// Handle `tunnel.infra.check` — on-demand check for one target.
async fn handle_tunnel_infra_check(
    state: &AppState,
    ws_sink: &WsSink,
    msg: &Value,
    request_id: Option<&str>,
) {
    let target_id = msg["target_id"].as_str().unwrap_or("").to_string();

    match crate::infra::routes::check_target(
        axum::extract::State(state.clone()),
        axum::extract::Path(target_id),
    )
    .await
    {
        Ok(axum::Json(body)) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.infra.check.result",
                    "request_id": request_id,
                    "status": 200,
                    "body": body,
                }),
            )
            .await;
        }
        Err((status, axum::Json(body))) => {
            send_response_async(
                ws_sink,
                json!({
                    "type": "tunnel.infra.check.result",
                    "request_id": request_id,
                    "status": status.as_u16(),
                    "body": body,
                }),
            )
            .await;
        }
    }
}

/// Background task that reads from a session's `OutputBuffer` and forwards
/// entries as WS messages through the tunnel. Similar to `ws/mod.rs` `subscriber_task`.
///
/// Uses feed+flush batching: holds the WS sink lock once per batch, feeds all
/// entries, and flushes periodically (every ~50 entries) to coalesce TCP writes.
/// This avoids per-entry lock acquisition + syscall overhead on the slow RISC-V CPU.
async fn tunnel_subscriber_task(
    state: AppState,
    session_id: String,
    buffer: Arc<tokio::sync::Mutex<OutputBuffer>>,
    ws_sink: WsSink,
    since: u64,
) {
    let mut cursor = since;
    let mut logged_first_output = false;
    let mut backpressure_active = false;
    loop {
        let (entries, notify) = {
            let buf = buffer.lock().await;
            if buf.has_entries_since(cursor) {
                let (entries, _dropped) = buf.read_since(cursor);
                (entries, None)
            } else {
                (vec![], Some(buf.notifier()))
            }
        };
        if !entries.is_empty() {
            if !logged_first_output {
                logged_first_output = true;
                info!(
                    session_id = %session_id,
                    entry_count = entries.len(),
                    "Tunnel: session subscriber emitting first output"
                );
            }
            let batched_messages = batch_output_entries(&session_id, &entries);
            for text in batched_messages {
                let stream_capacity = ws_sink.stream_tx.capacity();
                if stream_capacity == 0 && !backpressure_active {
                    backpressure_active = true;
                    state
                        .tunnel_stats
                        .stream_backpressure_events
                        .fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(
                        session_id = %session_id,
                        "Tunnel: session stream backpressure engaged"
                    );
                } else if stream_capacity > 0 && backpressure_active {
                    backpressure_active = false;
                    tracing::info!(
                        session_id = %session_id,
                        "Tunnel: session stream backpressure cleared"
                    );
                }
                if ws_sink
                    .stream_tx
                    .send(tokio_tungstenite::tungstenite::Message::Text(text.into()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            // Always advance cursor — dropped entries are gone (the relay
            // will send session.gap notifications for client-side recovery).
            if let Some(last) = entries.last() {
                cursor = last.seq;
            }
        }
        if let Some(n) = notify {
            n.notified().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_sha256_pin, tunnel_url_host};

    #[test]
    fn tunnel_url_host_extracts_host_without_port_or_path() {
        assert_eq!(
            tunnel_url_host("wss://relay.example.com:443/api/tunnel").unwrap(),
            "relay.example.com"
        );
        assert_eq!(
            tunnel_url_host("ws://192.168.1.10:1337/api/tunnel").unwrap(),
            "192.168.1.10"
        );
        assert_eq!(tunnel_url_host("wss://[::1]:443/api").unwrap(), "::1");
    }

    #[test]
    fn parse_sha256_pin_accepts_compact_and_colon_hex() {
        let compact = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
        let colon =
            "00:01:02:03:04:05:06:07:08:09:0a:0b:0c:0d:0e:0f:10:11:12:13:14:15:16:17:18:19:1a:1b:1c:1d:1e:1f";
        assert_eq!(
            parse_sha256_pin(compact).unwrap(),
            parse_sha256_pin(colon).unwrap()
        );
    }

    #[test]
    fn parse_sha256_pin_rejects_bad_input() {
        assert!(parse_sha256_pin("abc").is_err());
        assert!(parse_sha256_pin(&"g".repeat(64)).is_err());
    }
}

#[cfg(test)]
mod dns_fallback_tests {
    use std::net::{Ipv4Addr, SocketAddr};
    use std::sync::Arc;

    use super::{
        connect_tcp_ipv4_preferred, dial_addresses, own_relay_route, url_host_port, ConnectError,
    };
    use crate::netwatch::route::RTPROT_SCTL;
    use crate::netwatch::{self, HostRoute, NetState};

    fn transient(msg: &str) -> ConnectError {
        ConnectError::Transient(msg.to_string().into())
    }

    fn dialed(result: Result<Vec<SocketAddr>, ConnectError>) -> Vec<SocketAddr> {
        result.unwrap_or_else(|e| panic!("{e}"))
    }

    fn message(result: Result<Vec<SocketAddr>, ConnectError>) -> String {
        match result {
            Err(ConnectError::Transient(e)) => e.to_string(),
            Err(other) => panic!("not transient: {other}"),
            Ok(addrs) => panic!("resolved: {addrs:?}"),
        }
    }

    #[test]
    fn url_host_port_reads_the_authority_with_its_default_port() {
        assert_eq!(
            url_host_port("wss://relay-001.netage.ai/api/tunnel/register?token=k"),
            ("relay-001.netage.ai", 443)
        );
        assert_eq!(
            url_host_port("ws://10.0.0.1:1337/api/tunnel/register"),
            ("10.0.0.1", 1337)
        );
        assert_eq!(url_host_port("ws://relay"), ("relay", 80));
        assert_eq!(url_host_port("wss://[::1]:8443/x"), ("[::1]", 8443));
    }

    #[test]
    fn a_lookup_that_found_addresses_is_dialed_ipv4_first() {
        let v6: SocketAddr = "[2001:db8::1]:443".parse().unwrap();
        let v4: SocketAddr = "203.0.113.7:443".parse().unwrap();
        let fallback: SocketAddr = "174.138.114.209:443".parse().unwrap();
        let addrs = dialed(dial_addresses(Ok(vec![v6, v4]), "relay", Some(fallback)));
        assert_eq!(addrs, [v4, v6], "the fallback is only for a failed lookup");
    }

    #[test]
    fn without_a_known_address_a_failed_lookup_stays_transient() {
        assert_eq!(
            message(dial_addresses(
                Err(transient("DNS lookup timed out (10s) for relay")),
                "relay",
                None
            )),
            "DNS lookup timed out (10s) for relay"
        );
        assert_eq!(
            message(dial_addresses(Ok(Vec::new()), "relay", None)),
            "DNS resolution failed for relay"
        );
    }

    #[test]
    fn a_failed_lookup_dials_the_known_address() {
        let fallback: SocketAddr = "174.138.114.209:443".parse().unwrap();
        for lookup in [Err(transient("no such host")), Ok(Vec::new())] {
            assert_eq!(
                dialed(dial_addresses(lookup, "relay", Some(fallback))),
                [fallback]
            );
        }
    }

    #[tokio::test]
    async fn a_name_that_does_not_resolve_reaches_the_relay_by_its_known_address() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // An empty host fails the lookup without asking any resolver.
        let url = format!("ws://:{port}/api/tunnel/register");
        match connect_tcp_ipv4_preferred(&url, None, None).await {
            Err(ConnectError::Transient(_)) => {}
            Err(other) => panic!("a failed lookup is not a path failure: {other}"),
            Ok(_) => panic!("an empty host resolved"),
        }
        let stream = connect_tcp_ipv4_preferred(&url, None, Some(Ipv4Addr::LOCALHOST))
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(stream.peer_addr().unwrap().port(), port);

        // Nothing answers at the known address: the path failed.
        drop(listener);
        match connect_tcp_ipv4_preferred(&url, None, Some(Ipv4Addr::LOCALHOST)).await {
            Err(ConnectError::Path(_)) => {}
            Err(other) => panic!("not a path failure: {other}"),
            Ok(_) => panic!("connected to a closed port"),
        }
    }

    #[test]
    fn the_known_address_after_a_restart_is_sctls_own_route() {
        let relay = Ipv4Addr::new(174, 138, 114, 209);
        let host = |metric, protocol| HostRoute {
            dst: relay,
            dev: "eth1".into(),
            via: Some(Ipv4Addr::new(10, 42, 0, 1)),
            metric,
            protocol,
        };
        let (tx, rx) = netwatch::channel();
        assert_eq!(own_relay_route(None), None);
        assert_eq!(own_relay_route(Some(&rx)), None, "nothing published yet");
        // netifd's pin and a hand-added one are not sctl's.
        tx.send(Some(Arc::new(NetState {
            host_routes: vec![host(40, 4), host(0, 3)],
            ..NetState::default()
        })))
        .unwrap();
        assert_eq!(own_relay_route(Some(&rx)), None);
        tx.send(Some(Arc::new(NetState {
            host_routes: vec![host(40, 4), host(0, RTPROT_SCTL)],
            ..NetState::default()
        })))
        .unwrap();
        assert_eq!(own_relay_route(Some(&rx)), Some(relay));
    }
}

#[cfg(test)]
mod reconnect_delay_tests {
    use std::time::Duration;

    use super::{reconnect_delay, DelayClass};

    /// Draws that exercise the modulo edges plus a spread of arbitrary values.
    const DRAWS: [u64; 8] = [
        0,
        1,
        999,
        10_000,
        10_001,
        0x9E37_79B9_7F4A_7C15,
        u64::MAX - 1,
        u64::MAX,
    ];

    fn assert_bounds(class: DelayClass, lo: Duration, hi: Duration) {
        for draw in DRAWS {
            let d = reconnect_delay(class, draw);
            assert!(
                d >= lo && d <= hi,
                "{class:?} with draw {draw} gave {d:?}, outside [{lo:?}, {hi:?}]"
            );
        }
    }

    #[test]
    fn relay_shutdown_spreads_two_to_twelve_seconds() {
        assert_bounds(
            DelayClass::RelayShutdown,
            Duration::from_secs(2),
            Duration::from_secs(12),
        );
    }

    #[test]
    fn clean_close_has_a_floor_and_a_small_spread() {
        assert_bounds(
            DelayClass::CleanClose,
            Duration::from_secs(1),
            Duration::from_secs(4),
        );
    }

    #[test]
    fn bind_unavailable_keeps_its_five_second_cadence() {
        assert_bounds(
            DelayClass::BindUnavailable,
            Duration::from_secs(5),
            Duration::from_secs(7),
        );
    }

    #[test]
    fn flap_damping_stays_at_least_a_minute() {
        assert_bounds(
            DelayClass::Flap,
            Duration::from_mins(1),
            Duration::from_secs(90),
        );
    }

    #[test]
    fn auth_rejected_is_minutes_not_seconds() {
        assert_bounds(
            DelayClass::AuthRejected,
            Duration::from_mins(5),
            Duration::from_mins(15),
        );
    }

    #[test]
    fn transient_keeps_half_the_backoff_as_a_floor() {
        for backoff_secs in [2u64, 4, 8, 16, 30] {
            let backoff = Duration::from_secs(backoff_secs);
            assert_bounds(DelayClass::Transient(backoff), backoff / 2, backoff);
        }
    }

    #[test]
    fn distinct_draws_actually_spread() {
        // The anti-stampede property: two devices drawing different values
        // must not reconnect in the same instant.
        let a = reconnect_delay(DelayClass::RelayShutdown, 0);
        let b = reconnect_delay(DelayClass::RelayShutdown, 5_000);
        assert_ne!(a, b);
    }

    #[test]
    fn rehome_waits_exactly_what_it_carries() {
        assert_bounds(
            DelayClass::Rehome(Duration::ZERO),
            Duration::ZERO,
            Duration::ZERO,
        );
        let rest = Duration::from_millis(3_500);
        assert_bounds(DelayClass::Rehome(rest), rest, rest);
    }
}

#[cfg(test)]
mod rehome_tests {
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
    use std::sync::Arc;
    use std::time::Duration;

    use super::{
        moved_source, path_end, rehome_ends, rehome_wait, wait_to_dial, DelayClass, FlapWindow,
        EARLY_DIAL_FLOOR,
    };
    use crate::netwatch::{self, NetState};
    use crate::state::TunnelPath;

    const ETH: Ipv4Addr = Ipv4Addr::new(192, 168, 8, 20);
    const LTE: Ipv4Addr = Ipv4Addr::new(10, 64, 3, 2);

    #[test]
    fn a_moved_route_names_the_new_source() {
        assert_eq!(moved_source(Some(ETH), Some(LTE)), Some(LTE));
        assert_eq!(moved_source(Some(LTE), Some(ETH)), Some(ETH));
    }

    #[test]
    fn the_same_route_does_not_move() {
        assert_eq!(moved_source(Some(ETH), Some(ETH)), None);
    }

    #[test]
    fn no_route_at_all_is_not_a_move() {
        // Losing every route is left to the connection's own timeouts; a dial
        // with no route would only fail.
        assert_eq!(moved_source(Some(ETH), None), None);
        assert_eq!(moved_source(None, None), None);
    }

    #[test]
    fn a_route_appearing_is_a_move() {
        assert_eq!(moved_source(None, Some(LTE)), Some(LTE));
    }

    fn path(local: SocketAddr, remote: SocketAddr) -> TunnelPath {
        TunnelPath {
            local,
            remote,
            dev: None,
        }
    }

    #[test]
    fn only_an_ipv4_path_is_followed() {
        let relay = SocketAddrV4::new(Ipv4Addr::new(174, 138, 114, 209), 443);
        let local = SocketAddrV4::new(ETH, 40_022);
        assert_eq!(
            rehome_ends(&path(local.into(), relay.into())),
            Some((relay, ETH))
        );
        let v6: SocketAddr = "[2001:db8::1]:443".parse().unwrap();
        let v6_local: SocketAddr = "[2001:db8::2]:40022".parse().unwrap();
        assert_eq!(rehome_ends(&path(v6_local, v6)), None);
        assert_eq!(rehome_ends(&path(local.into(), v6)), None);
    }

    #[test]
    fn a_move_reads_dev_then_address() {
        assert_eq!(path_end(Some("eth1"), ETH), "eth1 192.168.8.20");
        assert_eq!(path_end(None, LTE), "10.64.3.2");
    }

    #[test]
    fn at_most_one_rehome_per_five_seconds() {
        assert_eq!(rehome_wait(None), Duration::ZERO);
        assert_eq!(rehome_wait(Some(Duration::ZERO)), Duration::from_secs(5));
        assert_eq!(
            rehome_wait(Some(Duration::from_secs(1))),
            Duration::from_secs(4)
        );
        assert_eq!(rehome_wait(Some(Duration::from_secs(5))), Duration::ZERO);
        assert_eq!(rehome_wait(Some(Duration::from_mins(1))), Duration::ZERO);
    }

    #[test]
    fn three_short_connections_engage_flap_damping() {
        let mut flap = FlapWindow::default();
        assert_eq!(
            flap.record(DelayClass::CleanClose, 2),
            DelayClass::CleanClose
        );
        assert_eq!(
            flap.record(DelayClass::CleanClose, 2),
            DelayClass::CleanClose
        );
        assert_eq!(flap.record(DelayClass::CleanClose, 2), DelayClass::Flap);
        // A connection that held breaks the run.
        assert_eq!(
            flap.record(DelayClass::CleanClose, 600),
            DelayClass::CleanClose
        );
        assert_eq!(
            flap.record(DelayClass::CleanClose, 2),
            DelayClass::CleanClose
        );
    }

    #[test]
    fn an_auth_rejected_loop_is_never_damped() {
        let mut flap = FlapWindow::default();
        for _ in 0..5 {
            assert_eq!(
                flap.record(DelayClass::AuthRejected, 0),
                DelayClass::AuthRejected
            );
        }
    }

    #[test]
    fn a_rehome_is_neither_damped_nor_counted() {
        let mut flap = FlapWindow::default();
        flap.record(DelayClass::CleanClose, 2);
        flap.record(DelayClass::CleanClose, 2);
        for _ in 0..12 {
            assert_eq!(
                flap.record(DelayClass::Rehome(Duration::ZERO), 1),
                DelayClass::Rehome(Duration::ZERO)
            );
        }
        assert_eq!(flap.durations.len(), 2);
        // The two short closes before the re-homes still count.
        assert_eq!(flap.record(DelayClass::CleanClose, 2), DelayClass::Flap);
    }

    #[test]
    fn only_a_wait_the_path_could_cause_ends_on_a_network_change() {
        assert!(DelayClass::CleanClose.wakes_on_network_change());
        assert!(DelayClass::BindUnavailable.wakes_on_network_change());
        assert!(DelayClass::Transient(Duration::from_secs(8)).wakes_on_network_change());
        assert!(DelayClass::Flap.wakes_on_network_change());
        assert!(!DelayClass::RelayShutdown.wakes_on_network_change());
        assert!(!DelayClass::AuthRejected.wakes_on_network_change());
        assert!(!DelayClass::Rehome(Duration::ZERO).wakes_on_network_change());
    }

    /// The loopback relay: the kernel always reaches it from 127.0.0.1.
    const LOOPBACK_RELAY: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9);

    fn state(seq: u64) -> Arc<NetState> {
        Arc::new(NetState {
            seq,
            ..NetState::default()
        })
    }

    #[tokio::test]
    async fn without_a_watch_the_wait_is_a_plain_sleep() {
        let started = tokio::time::Instant::now();
        let woke = wait_to_dial(
            Duration::from_millis(80),
            &mut None,
            Some(LOOPBACK_RELAY),
            Some(ETH),
        )
        .await;
        assert!(!woke);
        assert!(started.elapsed() >= Duration::from_millis(80));
    }

    #[tokio::test]
    async fn a_watch_that_never_published_is_a_plain_sleep() {
        // The listener could not open its socket: the route lookup is not
        // consulted, even though it would differ from `from`.
        let (_tx, rx) = netwatch::channel();
        let started = tokio::time::Instant::now();
        let woke = wait_to_dial(
            Duration::from_millis(80),
            &mut Some(rx),
            Some(LOOPBACK_RELAY),
            Some(ETH),
        )
        .await;
        assert!(!woke);
        assert!(started.elapsed() >= Duration::from_millis(80));
    }

    #[tokio::test]
    async fn a_change_that_leaves_the_route_alone_does_not_dial() {
        let (tx, rx) = netwatch::channel();
        tx.send(Some(state(1))).unwrap();
        let tx = Arc::new(tx);
        let later = tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            later.send(Some(state(2))).unwrap();
        });
        let woke = wait_to_dial(
            Duration::from_millis(200),
            &mut Some(rx),
            Some(LOOPBACK_RELAY),
            Some(Ipv4Addr::LOCALHOST),
        )
        .await;
        assert!(!woke);
    }

    #[tokio::test]
    async fn a_route_that_moved_during_the_attempt_dials_at_the_floor() {
        // Published before the wait: judged at once, not replayed.
        let (tx, rx) = netwatch::channel();
        tx.send(Some(state(1))).unwrap();
        let started = tokio::time::Instant::now();
        let woke = wait_to_dial(
            Duration::from_secs(30),
            &mut Some(rx),
            Some(LOOPBACK_RELAY),
            Some(ETH),
        )
        .await;
        assert!(woke);
        let waited = started.elapsed();
        assert!(waited >= EARLY_DIAL_FLOOR, "{waited:?}");
        assert!(waited < Duration::from_secs(5), "{waited:?}");
    }

    #[tokio::test]
    async fn any_change_dials_early_while_the_relay_is_unknown() {
        let (tx, rx) = netwatch::channel();
        tx.send(Some(state(1))).unwrap();
        let tx = Arc::new(tx);
        let later = tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            later.send(Some(state(2))).unwrap();
        });
        let started = tokio::time::Instant::now();
        let woke = wait_to_dial(Duration::from_secs(30), &mut Some(rx), None, None).await;
        assert!(woke);
        let waited = started.elapsed();
        assert!(waited >= EARLY_DIAL_FLOOR, "{waited:?}");
        assert!(waited < Duration::from_secs(5), "{waited:?}");
    }

    #[tokio::test]
    async fn the_floor_never_outlasts_the_delay() {
        let (tx, rx) = netwatch::channel();
        tx.send(Some(state(1))).unwrap();
        let started = tokio::time::Instant::now();
        let woke = wait_to_dial(
            Duration::from_millis(150),
            &mut Some(rx),
            Some(LOOPBACK_RELAY),
            Some(ETH),
        )
        .await;
        assert!(woke);
        assert!(started.elapsed() < EARLY_DIAL_FLOOR);
    }
}
