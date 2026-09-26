//! The relay route owner against a real kernel.
//!
//! Two network namespaces stand in for the device and the internet, joined by
//! two veth "uplinks": wan0 with its default route at metric 0 (the wire) and
//! wan1 at metric 40 (LTE), where netifd's pin for the relay also sits. LTE's
//! gateway lies outside its prefix (`onlink`), as some carriers hand out, so
//! the owner's route through it must be written onlink too. A TCP
//! listener stands in for the relay, and a small loop stands in for the
//! tunnel with the client's own timings: a 10 s dial, a ping every 5 s, 15 s
//! without an answer ends the connection, and a redial 1 s later. It follows
//! the kernel's route to the relay and reports to the owner exactly what the
//! tunnel client reports (registrations, and failures of the path).
//!
//! The scenario: the route lands on the wire; the relay goes down and comes
//! back, which moves nothing; the wire's upstream goes dark and the route
//! moves to LTE; the upstream returns and so does the route; with
//! `relay_route = "off"` nothing is written.
//!
//! Needs root, so it is ignored by default. Build it as yourself, then run the
//! binary with sudo:
//!
//! ```sh
//! cargo test -p sctl --test relay_route_netns --no-run
//! sudo -n env SCTL_NETNS_TEST=1 target/debug/deps/relay_route_netns-<hash> --ignored --nocapture
//! ```

use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4, TcpListener};
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use sctl::config::RelayRouteMode;
use sctl::netwatch::owner::{self, Probe, RelayRoute, Report, Timing, TunnelSignal};
use sctl::netwatch::route::{self, OwnedRoute, RTPROT_SCTL};
use sctl::netwatch::{self, source, HostRoute};
use sctl::state::{TunnelPath, TunnelStats};

const RELAY: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(10, 99, 0, 1), 7443);
const WAN0: Ipv4Addr = Ipv4Addr::new(10, 10, 1, 2);
const WAN0_GW: Ipv4Addr = Ipv4Addr::new(10, 10, 1, 1);
/// LTE's default route: `default via 10.10.9.1 dev wan1 onlink`, a gateway
/// outside wan1's 10.10.2.0/24 that the upstream side answers ARP for.
const WAN1_GW: Ipv4Addr = Ipv4Addr::new(10, 10, 9, 1);
/// netifd's pin for the relay, through an ordinary gateway on wan1.
const WAN1_PIN_GW: Ipv4Addr = Ipv4Addr::new(10, 10, 2, 1);
/// The owner's probe rounds while an uplink is suspect, shortened.
const PROBE_EVERY: Duration = Duration::from_secs(3);
/// The tunnel client's own timings: its TCP connect timeout, heartbeat,
/// pong timeout (3 heartbeats, at least 15 s) and shortest redial.
const DIAL_TIMEOUT: Duration = Duration::from_secs(10);
const HEARTBEAT: Duration = Duration::from_secs(5);
const PONG_TIMEOUT: Duration = Duration::from_secs(15);
const REDIAL: Duration = Duration::from_secs(1);

/// The two namespaces, deleted on drop (and with them the veths).
struct Namespaces {
    dev: String,
    up: String,
}

impl Namespaces {
    fn create() -> Self {
        let pid = std::process::id();
        let ns = Self {
            dev: format!("sctlrr-dev-{pid}"),
            up: format!("sctlrr-up-{pid}"),
        };
        ip(&["netns", "add", &ns.dev]);
        ip(&["netns", "add", &ns.up]);
        let (dev, up) = (ns.dev.as_str(), ns.up.as_str());
        for (link, peer, addr, gw) in [
            ("wan0", "wan0p", "10.10.1.2/24", "10.10.1.1/24"),
            ("wan1", "wan1p", "10.10.2.2/24", "10.10.2.1/24"),
        ] {
            ip(&[
                "link", "add", link, "netns", dev, "type", "veth", "peer", "name", peer, "netns",
                up,
            ]);
            ip(&["-n", dev, "addr", "add", addr, "dev", link]);
            ip(&["-n", up, "addr", "add", gw, "dev", peer]);
            ip(&["-n", dev, "link", "set", link, "up"]);
            ip(&["-n", up, "link", "set", peer, "up"]);
        }
        ip(&["-n", dev, "link", "set", "lo", "up"]);
        ip(&["-n", up, "link", "set", "lo", "up"]);
        ip(&["-n", up, "addr", "add", "10.99.0.1/32", "dev", "lo"]);
        ip(&["-n", up, "addr", "add", "10.10.9.1/32", "dev", "wan1p"]);
        ip(&[
            "-n",
            dev,
            "route",
            "add",
            "default",
            "via",
            "10.10.1.1",
            "dev",
            "wan0",
            "metric",
            "0",
        ]);
        ip(&[
            "-n",
            dev,
            "route",
            "add",
            "default",
            "via",
            "10.10.9.1",
            "dev",
            "wan1",
            "onlink",
            "metric",
            "40",
        ]);
        // netifd's pin for the WireGuard endpoint, on LTE.
        ip(&[
            "-n",
            dev,
            "route",
            "add",
            "10.99.0.1/32",
            "via",
            "10.10.2.1",
            "dev",
            "wan1",
            "metric",
            "40",
            "proto",
            "static",
        ]);
        // A probe's answer arrives on the uplink it left by while the relay
        // route points at the other one; as with bind_address, that needs
        // reverse-path filtering loose (2) or off (0), never strict (1).
        sysctl(dev, "net.ipv4.conf.all.rp_filter=2");
        ns
    }
}

/// Set one sysctl inside namespace `ns`.
fn sysctl(ns: &str, setting: &str) {
    let status = Command::new("ip")
        .args(["netns", "exec", ns, "sysctl", "-qw", setting])
        .status()
        .expect("sysctl runs");
    assert!(status.success(), "sysctl {setting} in {ns}");
}

impl Drop for Namespaces {
    fn drop(&mut self) {
        for ns in [&self.dev, &self.up] {
            let _ = Command::new("ip").args(["netns", "del", ns]).status();
        }
    }
}

fn ip(args: &[&str]) {
    let out = Command::new("ip").args(args).output().expect("ip runs");
    assert!(
        out.status.success(),
        "ip {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Move this thread into namespace `ns`, and prove it did: the owner writes
/// routes, and they must never land in the host's own table.
fn enter(ns: &str) {
    let file = std::fs::File::open(format!("/var/run/netns/{ns}")).expect("netns file");
    let target = file.metadata().unwrap().ino();
    // SAFETY: a valid namespace fd; setns only changes this thread.
    let rc = unsafe { libc::setns(file.as_raw_fd(), libc::CLONE_NEWNET) };
    assert_eq!(rc, 0, "setns: {}", std::io::Error::last_os_error());
    let now = std::fs::metadata("/proc/thread-self/ns/net").unwrap().ino();
    assert_eq!(
        now, target,
        "this thread runs in {ns}, not the host's namespace"
    );
}

/// The relay: an echo server on 10.99.0.1, inside the upstream namespace.
/// Storing false in the flag it returns takes it down as a restart does: it
/// closes every connection and stops listening, so dials are refused on
/// every uplink. Storing true brings it back.
fn start_relay(up: &str) -> Arc<AtomicBool> {
    let up = up.to_string();
    let running = Arc::new(AtomicBool::new(true));
    let flag = running.clone();
    let (ready, listening) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        enter(&up);
        let mut ready = Some(ready);
        loop {
            while !flag.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(50));
            }
            let listener = TcpListener::bind(RELAY).expect("relay listens");
            listener.set_nonblocking(true).unwrap();
            if let Some(ready) = ready.take() {
                ready.send(()).unwrap();
            }
            let mut open: Vec<std::net::TcpStream> = Vec::new();
            while flag.load(Ordering::SeqCst) {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                };
                stream.set_nonblocking(false).unwrap();
                open.push(stream.try_clone().unwrap());
                std::thread::spawn(move || {
                    let mut buf = [0u8; 64];
                    while let Ok(n @ 1..) = stream.read(&mut buf) {
                        if stream.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                });
            }
            drop(listener);
            for stream in open {
                let _ = stream.shutdown(Shutdown::Both);
            }
        }
    });
    listening.recv().expect("relay started");
    running
}

/// Failures of the path the stand-in tunnel has reported.
static PATH_FAILURES: AtomicUsize = AtomicUsize::new(0);

/// The tunnel, reduced to what the owner hears from it.
async fn tunnel(route: Arc<RelayRoute>, stats: Arc<TunnelStats>) {
    loop {
        let from = source::source_for(RELAY).ok();
        let dialed = tokio::time::timeout(DIAL_TIMEOUT, TcpStream::connect(RELAY)).await;
        let Ok(Ok(mut stream)) = dialed else {
            PATH_FAILURES.fetch_add(1, Ordering::SeqCst);
            route.signal(TunnelSignal::Failed {
                from,
                relay: Some(RELAY),
            });
            tokio::time::sleep(REDIAL).await;
            continue;
        };
        let Ok(SocketAddr::V4(ends)) = stream.local_addr() else {
            continue;
        };
        let local = *ends.ip();
        stats.set_path(Some(TunnelPath {
            local: SocketAddr::V4(ends),
            remote: SocketAddr::V4(RELAY),
            dev: source::interface_with_ipv4(local),
        }));
        stats.connected.store(true, Ordering::Relaxed);
        route.signal(TunnelSignal::Registered {
            relay: RELAY,
            local,
        });
        let mut last_pong = Instant::now();
        let mut next_ping = Instant::now();
        let failed = loop {
            tokio::time::sleep(Duration::from_millis(200)).await;
            // The route moved: re-home, which is not a failure.
            if source::source_for(RELAY).ok() != Some(local) {
                break false;
            }
            if Instant::now() >= next_ping {
                if stream.write_all(b"p").await.is_err() {
                    break true;
                }
                next_ping += HEARTBEAT;
            }
            let mut pong = [0u8; 16];
            match stream.try_read(&mut pong) {
                Ok(0) => break true,
                Ok(_) => last_pong = Instant::now(),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => break true,
            }
            if last_pong.elapsed() > PONG_TIMEOUT {
                break true;
            }
        };
        stats.connected.store(false, Ordering::Relaxed);
        stats.set_path(None);
        if failed {
            PATH_FAILURES.fetch_add(1, Ordering::SeqCst);
            route.signal(TunnelSignal::Failed {
                from: Some(local),
                relay: Some(RELAY),
            });
            tokio::time::sleep(REDIAL).await;
        }
    }
}

#[derive(Debug)]
struct Seen {
    routes: Vec<HostRoute>,
    report: Report,
    tunnel: Option<String>,
}

impl Seen {
    /// sctl's route to the relay, as (dev, via), when there is exactly one.
    fn ours(&self) -> Option<(&str, Option<Ipv4Addr>)> {
        let ours: Vec<&HostRoute> = self
            .routes
            .iter()
            .filter(|r| r.protocol == RTPROT_SCTL)
            .collect();
        match ours.as_slice() {
            [r] if r.metric == 0 => Some((r.dev.as_str(), r.via)),
            _ => None,
        }
    }

    fn netifd_pin_intact(&self) -> bool {
        self.routes.iter().any(|r| {
            r.dev == "wan1" && r.via == Some(WAN1_PIN_GW) && r.metric == 40 && r.protocol == 4
        })
    }
}

async fn seen(route: &RelayRoute, stats: &TunnelStats) -> Seen {
    let net = netwatch::dump().await.expect("route dump");
    Seen {
        routes: net.host_routes_to(*RELAY.ip()).cloned().collect(),
        report: route.report(),
        tunnel: stats
            .connected
            .load(Ordering::Relaxed)
            .then(|| stats.path().and_then(|p| p.dev))
            .flatten(),
    }
}

async fn wait_until(
    what: &str,
    within: Duration,
    route: &RelayRoute,
    stats: &TunnelStats,
    done: impl Fn(&Seen) -> bool,
) -> Duration {
    let started = Instant::now();
    loop {
        let now = seen(route, stats).await;
        if done(&now) {
            return started.elapsed();
        }
        assert!(
            started.elapsed() < within,
            "{what}: not within {within:?}; last seen {now:#?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn events(stats: &TunnelStats) -> Vec<String> {
    stats
        .events
        .lock()
        .await
        .iter()
        .filter(|e| e.event_type.as_str() == "relay_route")
        .map(|e| e.detail.clone())
        .collect()
}

#[test]
#[ignore = "needs root: sudo -n env SCTL_NETNS_TEST=1 <test binary> --ignored"]
fn the_relay_route_follows_the_uplink_that_reaches_the_relay() {
    if std::env::var_os("SCTL_NETNS_TEST").is_none() {
        eprintln!("skipped: set SCTL_NETNS_TEST=1 and run the test binary as root");
        return;
    }
    // SAFETY: geteuid has no preconditions.
    assert_eq!(unsafe { libc::geteuid() }, 0, "SCTL_NETNS_TEST needs root");

    let ns = Namespaces::create();
    let relay = start_relay(&ns.up);
    enter(&ns.dev);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(scenario(&ns, &relay));
}

async fn scenario(ns: &Namespaces, relay: &AtomicBool) {
    let (publisher, net) = netwatch::channel();
    tokio::spawn(netwatch::run(Arc::new(publisher), 1));
    // Only the wait between probe rounds is shortened.
    let timing = Timing {
        return_schedule: vec![PROBE_EVERY],
        ..Timing::default()
    };

    // follow_default: the /32 lands on the metric-0 link, beside netifd's pin.
    let route = Arc::new(RelayRoute::new(RelayRouteMode::FollowDefault));
    let stats = Arc::new(TunnelStats::new());
    // The stand-in relay speaks plain TCP, as a ws:// relay would; the TLS
    // handshake a wss:// probe adds is tested against a TLS server on
    // loopback in the owner's unit tests.
    let owner_task = tokio::spawn(owner::run(
        route.clone(),
        net.clone(),
        stats.clone(),
        timing,
        Probe::Connect,
    ));
    let tunnel_task = tokio::spawn(tunnel(route.clone(), stats.clone()));
    let took = wait_until(
        "the owner's route lands on the metric-0 link and the tunnel follows",
        Duration::from_secs(15),
        &route,
        &stats,
        |s| {
            s.ours() == Some(("wan0", Some(WAN0_GW)))
                && s.netifd_pin_intact()
                && s.tunnel.as_deref() == Some("wan0")
                && s.report.dev.as_deref() == Some("wan0")
        },
    )
    .await;
    eprintln!("route on wan0 after {took:?}");

    // The relay goes down (a restart, a deploy) while both uplinks work. Every
    // dial is refused, on both links: each pair of failures is a fail-over
    // round that finds no uplink answering, so nothing moves, nothing becomes
    // suspect, and the tunnel comes back on the wire.
    let before = PATH_FAILURES.load(Ordering::SeqCst);
    relay.store(false, Ordering::SeqCst);
    let outage = Instant::now();
    while outage.elapsed() < Duration::from_secs(8) {
        let now = seen(&route, &stats).await;
        assert_eq!(
            now.ours(),
            Some(("wan0", Some(WAN0_GW))),
            "the relay is down, not the wire: {now:#?}"
        );
        assert!(now.report.suspect.is_empty(), "{now:#?}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let failures = PATH_FAILURES.load(Ordering::SeqCst) - before;
    assert!(
        failures >= 4,
        "at least two fail-over rounds ran during the outage: {failures} failures"
    );
    relay.store(true, Ordering::SeqCst);
    let took = wait_until(
        "the tunnel comes back on the wire after the relay outage",
        Duration::from_secs(10),
        &route,
        &stats,
        |s| {
            s.ours() == Some(("wan0", Some(WAN0_GW)))
                && s.report.suspect.is_empty()
                && s.tunnel.as_deref() == Some("wan0")
        },
    )
    .await;
    eprintln!(
        "tunnel back on wan0 {took:?} after the relay, {failures} failures during the outage"
    );
    assert_eq!(
        events(&stats).await,
        ["none -> wan0 (lowest-metric default route)"],
        "a relay outage moves nothing"
    );

    // The wire's upstream goes dark while its link stays up.
    ip(&[
        "-n",
        &ns.up,
        "route",
        "add",
        "blackhole",
        &format!("{WAN0}/32"),
    ]);
    let took = wait_until(
        "the route moves to the metric-40 link",
        Duration::from_mins(1),
        &route,
        &stats,
        |s| {
            s.ours() == Some(("wan1", Some(WAN1_GW)))
                && s.report.suspect == ["wan0"]
                && s.tunnel.as_deref() == Some("wan1")
        },
    )
    .await;
    eprintln!("route on wan1 {took:?} after the blackhole");
    assert!(events(&stats)
        .await
        .contains(&"wan0 -> wan1 (wan0: no answer from the relay)".to_string()));

    // Restored upstream: back after two good probes, a round apart.
    ip(&[
        "-n",
        &ns.up,
        "route",
        "del",
        "blackhole",
        &format!("{WAN0}/32"),
    ]);
    let took = wait_until(
        "the route returns to the metric-0 link",
        Duration::from_secs(30),
        &route,
        &stats,
        |s| {
            s.ours() == Some(("wan0", Some(WAN0_GW)))
                && s.report.suspect.is_empty()
                && s.tunnel.as_deref() == Some("wan0")
        },
    )
    .await;
    eprintln!("route back on wan0 {took:?} after the restore");
    assert!(
        took + Duration::from_millis(100) >= PROBE_EVERY,
        "one good probe is not enough: {took:?}"
    );
    assert!(events(&stats)
        .await
        .contains(&"wan1 -> wan0 (wan0 answers the relay again)".to_string()));

    // Strict reverse-path filtering on an uplink the owner may probe shows
    // in its report, read from this namespace's /proc when asked for.
    assert!(route.report().rp_filter_strict.is_empty());
    sysctl(&ns.dev, "net.ipv4.conf.all.rp_filter=0");
    sysctl(&ns.dev, "net.ipv4.conf.wan1.rp_filter=1");
    assert_eq!(route.report().rp_filter_strict, ["wan1"]);
    sysctl(&ns.dev, "net.ipv4.conf.all.rp_filter=2");
    assert!(
        route.report().rp_filter_strict.is_empty(),
        "conf/all 2 makes every interface loose"
    );
    sysctl(&ns.dev, "net.ipv4.conf.wan1.rp_filter=0");

    // relay_route = "off": the same tunnel, and nothing changes. Stopping the
    // owner leaves its route; take it out to start from netifd's pin alone.
    owner_task.abort();
    tunnel_task.abort();
    let any = OwnedRoute {
        dst: *RELAY.ip(),
        via: None,
        oif: 0,
        metric: 0,
        onlink: false,
    };
    let left = seen(&route, &stats).await;
    assert_eq!(
        left.ours(),
        Some(("wan0", Some(WAN0_GW))),
        "the route outlives the owner"
    );
    route::delete(&any).await.expect("sctl's route removed");

    let off = Arc::new(RelayRoute::new(RelayRouteMode::Off));
    let stats = Arc::new(TunnelStats::new());
    let before = seen(&off, &stats).await.routes;
    let owner_task = tokio::spawn(owner::run(
        off.clone(),
        net.clone(),
        stats.clone(),
        Timing::default(),
        Probe::Connect,
    ));
    let tunnel_task = tokio::spawn(tunnel(off.clone(), stats.clone()));
    wait_until(
        "the tunnel connects over netifd's pin",
        Duration::from_secs(10),
        &off,
        &stats,
        |s| s.tunnel.as_deref() == Some("wan1"),
    )
    .await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    let after = seen(&off, &stats).await;
    assert_eq!(after.routes, before, "relay_route = off writes nothing");
    assert!(after.ours().is_none());
    assert_eq!(after.tunnel.as_deref(), Some("wan1"));
    assert_eq!(after.report.mode, RelayRouteMode::Off);
    assert!(events(&stats).await.is_empty());
    owner_task.abort();
    tunnel_task.abort();
}
