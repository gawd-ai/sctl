//! Tunnel client — outbound WS connection from device to relay.
//!
//! Spawned on startup when `[tunnel] url` is configured. Maintains a persistent
//! WebSocket to the relay with exponential-backoff reconnect, heartbeat, and
//! handles proxied requests by calling local route handlers.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{FutureExt, SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch, Mutex};
use tracing::{error, info, warn};

use crate::activity::{self, ActivityType, CachedExecResult};
use crate::config::TunnelConfig;
use crate::sessions::buffer::{OutputBuffer, OutputEntry};
use crate::state::TunnelEventType;
use crate::AppState;

use super::{decode_binary_frame, encode_binary_frame};

/// Static heartbeat message — avoids serde allocation on every heartbeat tick.
const PING_TEXT: &str = r#"{"type":"tunnel.ping"}"#;

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

    unsafe {
        let mut ifaddrs: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&raw mut ifaddrs) != 0 {
            return None;
        }

        let mut current = ifaddrs;
        let mut result = None;

        while !current.is_null() {
            let ifa = &*current;
            if !ifa.ifa_name.is_null() && !ifa.ifa_addr.is_null() {
                let name = std::ffi::CStr::from_ptr(ifa.ifa_name);
                if let Ok(name_str) = name.to_str() {
                    if name_str == value && i32::from((*ifa.ifa_addr).sa_family) == libc::AF_INET {
                        #[allow(clippy::cast_ptr_alignment)]
                        let addr = &*(ifa.ifa_addr.cast::<libc::sockaddr_in>());
                        let ip = std::net::Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr));
                        result = Some(std::net::IpAddr::V4(ip));
                        break;
                    }
                }
            }
            current = ifa.ifa_next;
        }

        libc::freeifaddrs(ifaddrs);
        result
    }
}

/// Probe whether a local IP address is currently available for binding.
async fn is_local_address_available(addr: &std::net::IpAddr) -> bool {
    tokio::net::UdpSocket::bind(SocketAddr::new(*addr, 0))
        .await
        .is_ok()
}

/// Channel-based WS sender — eliminates mutex contention between subscriber
/// tasks, heartbeat, and response handlers.  All writers push messages into
/// this channel; a dedicated writer task drains it to the actual WS sink.
type WsSink = mpsc::Sender<tokio_tungstenite::tungstenite::Message>;

/// Spawn the tunnel client task. Returns a `JoinHandle` that runs until cancelled.
pub fn spawn(state: AppState, tunnel_config: TunnelConfig) -> tokio::task::JoinHandle<()> {
    tokio::spawn(tunnel_client_loop(state, tunnel_config))
}

/// Main loop: connect, handle messages, reconnect on failure.
async fn tunnel_client_loop(state: AppState, config: TunnelConfig) {
    // Flap detection: track last N connection durations. If recent connections
    // are all short-lived, extend backoff to avoid hammering the relay.
    const FLAP_WINDOW: usize = 10;
    const FLAP_THRESHOLD_SECS: u64 = 30;
    const FLAP_CHECK_COUNT: usize = 3;

    let relay_url = config
        .url
        .as_deref()
        .expect("tunnel.url must be set for client mode");
    let mut delay = Duration::from_secs(config.reconnect_delay_secs);
    let max_delay = Duration::from_secs(config.reconnect_max_delay_secs);
    let mut reconnects: u64 = 0;
    let mut connection_durations: VecDeque<u64> = VecDeque::with_capacity(FLAP_WINDOW);

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
        match connect_and_run(&state, &config, relay_url).await {
            Ok(DisconnectReason::RelayShutdown) => {
                info!("Tunnel: relay shutting down, reconnecting immediately...");
                state
                    .tunnel_stats
                    .push_event(TunnelEventType::Disconnected, "relay shutdown".into())
                    .await;
                delay = Duration::ZERO;
            }
            Ok(DisconnectReason::Clean) => {
                info!("Tunnel: connection closed cleanly, reconnecting...");
                state
                    .tunnel_stats
                    .push_event(TunnelEventType::Disconnected, "clean close".into())
                    .await;
                delay = Duration::ZERO;
            }
            Err(ConnectError::Permanent(msg)) => {
                error!("Tunnel: permanent error: {msg} — stopping tunnel client");
                state
                    .tunnel_stats
                    .push_event(TunnelEventType::Disconnected, format!("permanent: {msg}"))
                    .await;
                state
                    .tunnel_stats
                    .connected
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                return;
            }
            Err(ConnectError::Transient(e)) => {
                let msg = e.to_string();
                state
                    .tunnel_stats
                    .push_event(TunnelEventType::Disconnected, msg.clone())
                    .await;
                if msg.contains("bind_address") && msg.contains("not available")
                    || msg.contains("Address not available")
                    || msg.contains("os error 99")
                {
                    // Interface is down (EADDRNOTAVAIL) — use fixed 5s retry, no escalation.
                    warn!("Tunnel: bind address unavailable ({msg}), retrying in 5s");
                    delay = Duration::from_secs(5);
                } else {
                    warn!(
                        "Tunnel: connection error: {msg}, reconnecting in {}s",
                        delay.as_secs()
                    );
                    escalate_backoff = true;
                }
            }
        }
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
        let duration_secs = connect_start.elapsed().as_secs();
        if connection_durations.len() >= FLAP_WINDOW {
            connection_durations.pop_front();
        }
        connection_durations.push_back(duration_secs);

        // Flap detection: if last N connections all lasted < threshold, extend backoff
        if connection_durations.len() >= FLAP_CHECK_COUNT {
            let recent: Vec<&u64> = connection_durations
                .iter()
                .rev()
                .take(FLAP_CHECK_COUNT)
                .collect();
            let all_short = recent.iter().all(|&&d| d < FLAP_THRESHOLD_SECS);
            if all_short {
                warn!(
                    "Tunnel: flap detected ({FLAP_CHECK_COUNT} connections lasted <{FLAP_THRESHOLD_SECS}s), extending backoff to 60s"
                );
                delay = Duration::from_secs(60);
                escalate_backoff = false; // don't double-escalate
            }
        }

        tokio::time::sleep(delay).await;
        if escalate_backoff {
            delay = (delay * 2).min(max_delay);
        } else {
            delay = Duration::from_secs(config.reconnect_delay_secs);
        }
    }
}

/// Reason the tunnel connection ended.
enum DisconnectReason {
    /// Relay sent `tunnel.relay_shutdown` — intentional, skip backoff.
    RelayShutdown,
    /// Normal close frame or EOF.
    Clean,
}

/// Classification of connection errors for backoff strategy.
enum ConnectError {
    /// Auth rejected, invalid tunnel key — stop retrying entirely.
    Permanent(String),
    /// DNS timeout, TCP timeout, TLS failure — exponential backoff.
    Transient(Box<dyn std::error::Error + Send + Sync>),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::Permanent(msg) => write!(f, "{msg}"),
            ConnectError::Transient(e) => write!(f, "{e}"),
        }
    }
}

/// Configure TCP keepalive on a connected stream.
///
/// LTE carriers commonly have NAT timeouts of 30-60s. Without keepalive,
/// a silent NAT expiry kills the connection and the relay won't see heartbeats.
/// Parameters: start probing after `idle` seconds, probe every `interval` seconds,
/// give up after `count` failed probes.
#[cfg(unix)]
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
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
    }
}

/// Resolve DNS for a `wss://` URL and connect TCP, preferring IPv4 addresses.
///
/// Many embedded devices (LTE/CGNAT) have broken IPv6 routes that cause ~4 minute
/// TCP connect timeouts before falling back to IPv4. By sorting IPv4 first we
/// avoid the delay.
async fn connect_tcp_ipv4_preferred(
    url: &str,
    bind_address: Option<&str>,
) -> Result<TcpStream, Box<dyn std::error::Error + Send + Sync>> {
    // Parse host:port from wss:// or ws:// URL
    let without_scheme = url
        .strip_prefix("wss://")
        .or_else(|| url.strip_prefix("ws://"))
        .unwrap_or(url);
    let authority = without_scheme.split('/').next().unwrap_or(without_scheme);
    let (host, port) = if let Some(colon) = authority.rfind(':') {
        let port_str = &authority[colon + 1..];
        if let Ok(p) = port_str.parse::<u16>() {
            (&authority[..colon], p)
        } else {
            (authority, if url.starts_with("wss://") { 443 } else { 80 })
        }
    } else {
        (authority, if url.starts_with("wss://") { 443 } else { 80 })
    };
    let host_port = format!("{host}:{port}");

    // Resolve with timeout — DNS can hang on broken resolvers
    let mut addrs: Vec<SocketAddr> =
        tokio::time::timeout(Duration::from_secs(10), tokio::net::lookup_host(&host_port))
            .await
            .map_err(|_| -> Box<dyn std::error::Error + Send + Sync> {
                format!("DNS lookup timed out (10s) for {host}").into()
            })??
            .collect();

    // Sort: IPv4 first, then IPv6
    addrs.sort_by_key(|a| i32::from(!a.is_ipv4()));

    if addrs.is_empty() {
        return Err(format!("DNS resolution failed for {host}").into());
    }

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
                    return Err(format!(
                        "bind_address '{s}' not available (interface down or no IPv4?)"
                    )
                    .into());
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
            .transpose()?;
        (addr, None)
    };

    if let Some(ref ba) = bind_addr {
        if !is_local_address_available(ba).await {
            return Err(format!("bind_address {ba} not available (interface down?)").into());
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
                    let fd = socket.as_raw_fd();
                    let c_iface = std::ffi::CString::new(iface.as_str())
                        .map_err(|_| std::io::Error::other("invalid interface name"))?;
                    let ret = unsafe {
                        libc::setsockopt(
                            fd,
                            libc::SOL_SOCKET,
                            libc::SO_BINDTODEVICE,
                            c_iface.as_ptr().cast(),
                            c_iface.as_bytes_with_nul().len() as libc::socklen_t,
                        )
                    };
                    if ret != 0 {
                        let err = std::io::Error::last_os_error();
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

    Err(last_err.unwrap_or_else(|| "all addresses failed".into()))
}

/// A single connection attempt: connect, register, handle messages until disconnect.
#[allow(clippy::too_many_lines)]
async fn connect_and_run(
    state: &AppState,
    config: &TunnelConfig,
    relay_url: &str,
) -> Result<DisconnectReason, ConnectError> {
    // Build the URL with auth query params
    let url = format!(
        "{}?token={}&serial={}",
        relay_url, config.tunnel_key, state.config.device.serial
    );

    let connect_start = Instant::now();

    // DNS + TCP with IPv4 preference (avoids long IPv6 timeouts on LTE/CGNAT)
    let tcp_stream = connect_tcp_ipv4_preferred(&url, config.bind_address.as_deref())
        .await
        .map_err(ConnectError::Transient)?;
    let tcp_elapsed = connect_start.elapsed();

    // TLS + WebSocket handshake with timeout (can hang on riscv64/slow networks)
    let tls_start = Instant::now();
    let (ws_stream, _response) = tokio::time::timeout(
        Duration::from_secs(15),
        tokio_tungstenite::client_async_tls(url.as_str(), tcp_stream),
    )
    .await
    .map_err(|_| ConnectError::Transient("TLS/WS handshake timed out (15s)".into()))?
    .map_err(|e| ConnectError::Transient(e.into()))?;
    let tls_elapsed = tls_start.elapsed();

    let (mut raw_ws_sink, mut ws_stream) = ws_stream.split();

    // Send registration directly on the raw sink (before spawning writer task)
    let reg_start = Instant::now();
    {
        let reg = json!({
            "type": "tunnel.register",
            "serial": state.config.device.serial,
            "api_key": state.config.auth.api_key,
        });
        raw_ws_sink
            .send(tokio_tungstenite::tungstenite::Message::Text(
                serde_json::to_string(&reg)
                    .map_err(|e| ConnectError::Transient(e.into()))?
                    .into(),
            ))
            .await
            .map_err(|e| ConnectError::Transient(e.into()))?;
    }

    // Wait for registration ack with timeout
    match tokio::time::timeout(Duration::from_secs(10), ws_stream.next()).await {
        Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text)))) => {
            match serde_json::from_str::<Value>(&text) {
                Ok(msg) => {
                    let msg_type = msg["type"].as_str().unwrap_or("");
                    match msg_type {
                        "tunnel.register.ack" => {
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
                        }
                        "error" => {
                            let code = msg["code"].as_str().unwrap_or("");
                            let message =
                                msg["message"].as_str().unwrap_or("registration rejected");
                            if code == "FORBIDDEN" {
                                return Err(ConnectError::Permanent(format!(
                                    "Registration rejected: {message}"
                                )));
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
            return Err(ConnectError::Transient(e.into()));
        }
        Ok(None) => {
            return Err(ConnectError::Transient(
                "Connection closed during registration".into(),
            ));
        }
        Err(_) => {
            return Err(ConnectError::Transient(
                "Registration ack timed out (10s)".into(),
            ));
        }
    }

    // Channel-based writer: all tasks send through ws_sink (channel sender),
    // a dedicated writer task drains to the real WS sink. This eliminates
    // mutex contention between subscriber tasks, heartbeat, and responses.
    let (ws_sink, mut ws_out_rx) = mpsc::channel::<tokio_tungstenite::tungstenite::Message>(512);
    let (writer_exit_tx, mut writer_exit_rx) = oneshot::channel::<()>();
    let writer_stats = state.tunnel_stats.clone();
    let writer_task = tokio::spawn(async move {
        while let Some(msg) = ws_out_rx.recv().await {
            if raw_ws_sink.send(msg).await.is_err() {
                warn!("Tunnel: writer task WS send failed, exiting");
                writer_stats
                    .push_event(TunnelEventType::WriterFailed, "WS send error".into())
                    .await;
                break;
            }
            writer_stats.messages_sent.fetch_add(1, Ordering::Relaxed);
        }
        warn!("Tunnel: writer task exited");
        let _ = writer_exit_tx.send(());
    });

    // Subscribe to session lifecycle broadcasts so we can forward them
    let mut broadcast_rx = state.session_events.subscribe();

    // Track subscriber tasks for session output forwarding
    let subscriber_tasks: Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>> =
        Arc::new(Mutex::new(HashMap::new()));

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
    let heartbeat_sink = ws_sink.clone();
    let heartbeat_interval = Duration::from_secs(config.heartbeat_interval_secs);
    let pong_timeout_ms = config.heartbeat_interval_secs * 3 * 1000;
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
            #[allow(clippy::cast_possible_truncation)]
            let uptime_ms = heartbeat_epoch.elapsed().as_millis() as u64;
            heartbeat_stats
                .current_uptime_ms
                .store(uptime_ms, Ordering::Relaxed);

            // Pong watchdog: check if relay is actually responding
            let last = heartbeat_last_pong.load(Ordering::Relaxed);
            // Update pong age for health endpoint
            #[allow(clippy::cast_possible_truncation)]
            let pong_age = heartbeat_epoch.elapsed().as_millis() as u64;
            heartbeat_stats
                .last_pong_age_ms
                .store(pong_age.saturating_sub(last), Ordering::Relaxed);
            #[allow(clippy::cast_possible_truncation)]
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
            let capacity = heartbeat_ws_sink.capacity();
            if capacity < 128 {
                warn!("Tunnel: writer channel backpressure ({capacity}/512 slots free)");
            }

            if heartbeat_sink
                .send(tokio_tungstenite::tungstenite::Message::Text(
                    PING_TEXT.into(),
                ))
                .await
                .is_err()
            {
                warn!("Tunnel: heartbeat send failed, triggering reconnect");
                let _ = heartbeat_cancel_tx.send(true);
                break;
            }
            // Record ping timestamp for RTT calculation on pong
            #[allow(clippy::cast_possible_truncation)]
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

    // Periodic reaping of finished subscriber tasks (30s interval)
    let mut reap_interval = tokio::time::interval(Duration::from_secs(30));
    reap_interval.tick().await; // consume the immediate first tick

    let mut disconnect_reason = DisconnectReason::Clean;

    // Re-subscribe to all running sessions after reconnect
    {
        let sessions = state.session_manager.list_sessions().await;
        for s in &sessions {
            if s.status == "running" {
                if let Some(buffer) = state.session_manager.get_buffer(&s.session_id).await {
                    let last_seq = {
                        let buf = buffer.lock().await;
                        buf.next_seq().saturating_sub(1)
                    };
                    let sink_clone = ws_sink.clone();
                    let sid = s.session_id.clone();
                    let task = tokio::spawn(tunnel_subscriber_task(
                        sid.clone(),
                        buffer,
                        sink_clone,
                        last_seq,
                    ));
                    subscriber_tasks.lock().await.insert(sid, task);
                }
            }
        }
        let count = subscriber_tasks.lock().await.len();
        if count > 0 {
            info!("Tunnel: re-subscribed to {count} running sessions");
        }
    }

    loop {
        tokio::select! {
            msg = ws_stream.next() => {
                let Some(msg) = msg else { break };
                let msg = match msg {
                    Ok(m) => m,
                    Err(e) => {
                        warn!("Tunnel: WS read error: {e}");
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
                        let msg_type = parsed["type"].as_str().unwrap_or("");
                        match msg_type {
                            "tunnel.relay_shutdown" => {
                                info!("Tunnel: relay sent shutdown notification");
                                disconnect_reason = DisconnectReason::RelayShutdown;
                                break;
                            }
                            // Pong: handle inline — must never be blocked by slow handlers
                            "tunnel.pong" => {
                                #[allow(clippy::cast_possible_truncation)]
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
                            // Everything else: spawn as task to keep the read loop responsive.
                            // This prevents slow handlers (exec, file I/O) from blocking
                            // pong reads, which would trigger the pong watchdog.
                            _ => {
                                let st = state.clone();
                                let tx = ws_sink.clone();
                                let tasks = subscriber_tasks.clone();
                                tokio::spawn(async move {
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
                            let payload = payload.to_vec();
                            tokio::spawn(async move {
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
                    let _ = ws_sink.send(tokio_tungstenite::tungstenite::Message::Text(
                        text.into(),
                    )).await;
                }
            }
            _ = reap_interval.tick() => {
                subscriber_tasks.lock().await.retain(|_, h| !h.is_finished());
            }
            _ = heartbeat_cancel_rx.changed() => {
                warn!("Tunnel: heartbeat failure detected, disconnecting");
                break;
            }
            _ = &mut writer_exit_rx => {
                warn!("Tunnel: writer task exited, disconnecting");
                break;
            }
        }
    }

    // Cleanup
    heartbeat_task.abort();
    writer_task.abort();
    let tasks = subscriber_tasks.lock().await;
    for (_, task) in tasks.iter() {
        task.abort();
    }

    // Pause all active transfers on tunnel disconnect
    state.transfer_manager.pause_all().await;

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
            handle_tunnel_info(state, ws_sink, request_id.as_deref()).await;
        }
        "tunnel.health" => {
            handle_tunnel_health(state, ws_sink, request_id.as_deref()).await;
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
        // Forwarded session.* and shell.* messages from clients via relay.
        // GUARD: Any new WS message prefix (e.g. "foo.*") requires adding it here,
        // otherwise tunnel clients won't handle those messages and they'll fall
        // through to the "Unknown tunnel message type" warning below.
        t if t.starts_with("session.") || t.starts_with("shell.") => {
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
async fn send_response(ws_sink: &WsSink, msg: Value) {
    let text = serde_json::to_string(&msg)
        .unwrap_or_else(|_| r#"{"type":"error","message":"serialize failed"}"#.to_string());
    let _ = ws_sink
        .send(tokio_tungstenite::tungstenite::Message::Text(text.into()))
        .await;
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

    send_response(ws_sink, result).await;
}

/// Handle `tunnel.exec_batch` — batch command execution
async fn handle_tunnel_exec_batch(
    state: &AppState,
    ws_sink: &WsSink,
    msg: &Value,
    request_id: Option<&str>,
) {
    let Some(commands) = msg["commands"].as_array() else {
        send_response(
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
        send_response(ws_sink, json!({
            "type": "tunnel.exec_batch.result",
            "request_id": request_id,
            "status": 400,
            "body": {
                "error": format!("Too many commands (max {})", state.config.server.max_batch_size),
                "code": "BATCH_TOO_LARGE"
            }
        }))
        .await;
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

    send_response(
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
async fn handle_tunnel_info(state: &AppState, ws_sink: &WsSink, request_id: Option<&str>) {
    // Call the info handler directly — it returns JSON
    match crate::routes::info::info(axum::extract::State(state.clone())).await {
        Ok(axum::Json(body)) => {
            send_response(
                ws_sink,
                json!({
                    "type": "tunnel.info.result",
                    "request_id": request_id,
                    "status": 200,
                    "body": body,
                }),
            )
            .await;
        }
        Err(status) => {
            send_response(
                ws_sink,
                json!({
                    "type": "tunnel.info.result",
                    "request_id": request_id,
                    "status": status.as_u16(),
                    "body": {"error": "Failed to get info"},
                }),
            )
            .await;
        }
    }
}

/// Handle tunnel.health — health check
async fn handle_tunnel_health(state: &AppState, ws_sink: &WsSink, request_id: Option<&str>) {
    let axum::Json(body) = crate::routes::health::health(axum::extract::State(state.clone())).await;
    send_response(
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
    #[allow(clippy::cast_possible_truncation)]
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
            send_response(
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
            send_response(
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
            send_response(
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
            send_response(
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

    send_response(
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
            send_response(
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
            send_response(
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
    send_response(
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
    send_response(
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
    #[allow(clippy::cast_possible_truncation)]
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
            send_response(
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
            send_response(
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
            send_response(
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
            send_response(
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
            send_response(
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
            send_response(
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
            send_response(
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
            send_response(
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
    #[allow(clippy::cast_possible_truncation)]
    let chunk_size = msg["chunk_size"].as_u64().map(|v| v as u32);

    match state.transfer_manager.init_download(path, chunk_size).await {
        Ok(result) => {
            send_response(
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
            send_response(
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
        #[allow(clippy::cast_possible_truncation)]
        chunk_size: msg["chunk_size"].as_u64().unwrap_or(0) as u32,
        #[allow(clippy::cast_possible_truncation)]
        total_chunks: msg["total_chunks"].as_u64().unwrap_or(0) as u32,
        mode: msg["mode"].as_str().map(ToString::to_string),
    };

    match state.transfer_manager.init_upload(req).await {
        Ok(result) => {
            send_response(
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
            send_response(
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
    #[allow(clippy::cast_possible_truncation)]
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
            let _ = ws_sink
                .send(tokio_tungstenite::tungstenite::Message::Binary(
                    frame.into(),
                ))
                .await;
        }
        Err(e) => {
            send_response(ws_sink, gx_error_response("gx.chunk.ack", request_id, &e)).await;
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
    #[allow(clippy::cast_possible_truncation)]
    let chunk_index = header["chunk_index"].as_u64().unwrap_or(0) as u32;
    let chunk_hash = header["chunk_hash"].as_str().unwrap_or("");

    match state
        .transfer_manager
        .receive_chunk(transfer_id, chunk_index, chunk_hash, payload)
        .await
    {
        Ok(ack) => {
            send_response(
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
            send_response(ws_sink, gx_error_response("gx.chunk.ack", request_id, &e)).await;
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
            send_response(
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
            send_response(
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
            send_response(
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
            send_response(
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
            send_response(
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
            send_response(
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
    send_response(
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
#[allow(clippy::too_many_lines)]
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
            #[allow(clippy::cast_possible_truncation)]
            let rows = msg["rows"]
                .as_u64()
                .unwrap_or(u64::from(state.config.server.default_terminal_rows))
                as u16;
            #[allow(clippy::cast_possible_truncation)]
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
                    send_response(ws_sink, resp).await;

                    // Now start subscriber for output forwarding
                    if let Some(buffer) = state.session_manager.get_buffer(&session_id).await {
                        let sink_clone = ws_sink.clone();
                        let sid = session_id.clone();
                        let task = tokio::spawn(tunnel_subscriber_task(
                            sid.clone(),
                            buffer,
                            sink_clone,
                            0,
                        ));
                        subscriber_tasks.lock().await.insert(sid, task);
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
                    let mut resp = json!({
                        "type": "error",
                        "code": "SESSION_LIMIT",
                        "message": e,
                    });
                    if let Some(ref rid) = request_id {
                        resp["request_id"] = json!(rid);
                    }
                    send_response(ws_sink, resp).await;
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
                send_response(ws_sink, resp).await;
            } else {
                let mut resp = json!({
                    "type": "session.exec.ack",
                    "session_id": session_id,
                });
                if let Some(ref rid) = request_id {
                    resp["request_id"] = json!(rid);
                }
                send_response(ws_sink, resp).await;
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
                    send_response(
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
                    send_response(ws_sink, resp).await;
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
                    send_response(ws_sink, resp).await;
                }
            }
        }
        "session.signal" => {
            let session_id = msg["session_id"].as_str().unwrap_or("");
            let signal = msg["signal"].as_i64().unwrap_or(0);
            if !session_id.is_empty() && signal != 0 {
                #[allow(clippy::cast_possible_truncation)]
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
                        send_response(ws_sink, resp).await;
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
                        send_response(ws_sink, resp).await;
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

                if let Some(buffer) = state.session_manager.attach(session_id).await {
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
                    send_response(ws_sink, resp).await;

                    // Start subscriber
                    let sink_clone = ws_sink.clone();
                    let sid = session_id.to_string();
                    let task = tokio::spawn(tunnel_subscriber_task(
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
                    send_response(ws_sink, resp).await;
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
            send_response(ws_sink, resp).await;
        }
        "session.resize" => {
            let session_id = msg["session_id"].as_str().unwrap_or("");
            #[allow(clippy::cast_possible_truncation)]
            let rows = msg["rows"].as_u64().unwrap_or(0) as u16;
            #[allow(clippy::cast_possible_truncation)]
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
                        send_response(ws_sink, resp).await;
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
                        send_response(ws_sink, resp).await;
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
                            send_response(ws_sink, resp).await;
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
                            send_response(ws_sink, resp).await;
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
                            send_response(ws_sink, resp).await;
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
                            send_response(ws_sink, resp).await;
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
                        send_response(ws_sink, resp).await;
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
                        send_response(ws_sink, resp).await;
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
            send_response(ws_sink, resp).await;
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
            send_response(
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
            send_response(
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
            send_response(
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
            send_response(
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
            send_response(
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
            send_response(
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
            send_response(
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
            send_response(
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
            send_response(
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
            send_response(
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

/// Background task that reads from a session's `OutputBuffer` and forwards
/// entries as WS messages through the tunnel. Similar to `ws/mod.rs` `subscriber_task`.
///
/// Uses feed+flush batching: holds the WS sink lock once per batch, feeds all
/// entries, and flushes periodically (every ~50 entries) to coalesce TCP writes.
/// This avoids per-entry lock acquisition + syscall overhead on the slow RISC-V CPU.
async fn tunnel_subscriber_task(
    session_id: String,
    buffer: Arc<tokio::sync::Mutex<OutputBuffer>>,
    ws_sink: WsSink,
    since: u64,
) {
    let mut cursor = since;
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
            for entry in &entries {
                let msg = entry_to_ws_message(&session_id, entry);
                let text = serde_json::to_string(&msg).unwrap_or_else(|_| {
                    r#"{"type":"error","message":"serialize failed"}"#.to_string()
                });
                if ws_sink
                    .send(tokio_tungstenite::tungstenite::Message::Text(text.into()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            if let Some(last) = entries.last() {
                cursor = last.seq;
            }
        }
        if let Some(n) = notify {
            n.notified().await;
        }
    }
}
