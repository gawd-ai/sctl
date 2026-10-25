//! The net tools' guard against a real kernel (ADR-012, TRD-9 3.4).
//!
//! Network namespaces stand in for a unit and what it is wired to:
//!
//! - the unit: `lan0` on the site's network (192.0.2.1/24); `lte9`, the LTE
//!   bearer its config names (`[lte] interface = "lte9"`, a name no bearer
//!   rule knows, so only the config makes it LTE), holding the default route
//!   through 198.51.100.1; and `wg9`, a `wg`-named veth with ARP off and a
//!   route of its own, standing for the mesh.
//! - a switch: a bridge joining `lan0` to the target and the twin.
//! - the target: 192.0.2.10 and 192.0.2.11 on MAC 02:00:00:00:00:0a,
//!   answering TCP on 7000, HTTP on 8080 and SNMP on 161.
//! - the twin: MAC 02:00:00:00:00:0b, which takes 192.0.2.10 when a case
//!   says so.
//! - the LTE side, behind `lte9`, and the mesh's far end, behind `wg9`.
//!
//! A capture in each namespace keeps every frame it sees, so a case can say
//! what reached the wire. Every command is an argument vector, and every
//! one acts only in the namespaces this test creates and removes.
//!
//! The cases are 1 to 8 of TRD-9 3.4; case 9, a scan, comes with 3.5.
//!
//! Needs root, so it is ignored by default. Build it as yourself, then run
//! the binary with sudo:
//!
//! ```sh
//! cargo test -p sctl --test net_tools_netns --no-run
//! sudo -n env SCTL_NETNS_TEST=1 target/debug/deps/net_tools_netns-<hash> --ignored --nocapture
//! ```
//!
//! Case 5 marks the unit's own traffic as mwan3 does, with `nft`, else
//! `iptables`; one of them must be on the PATH.

mod snmp_agent;

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, UdpSocket};
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use snmp_agent::Agent;

const SCTL: &str = env!("CARGO_BIN_EXE_sctl");
const PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

const UNIT_MAC: &str = "02:00:00:00:00:01";
const TARGET_MAC: &str = "02:00:00:00:00:0a";
const TWIN_MAC: &str = "02:00:00:00:00:0b";
const TARGET: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 10);
/// The target's second address: another host, as a URL names it.
const OTHER: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 11);
/// An address behind the LTE gateway.
const BEHIND_GATEWAY: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 50);
const TCP_PORT: u16 = 7000;
const HTTP_PORT: u16 = 8080;
const AGENT_PORT: u16 = 18_337;
const API_KEY: &str = "netns-test-key";
/// The community the cases send: it must never show anywhere but on the
/// wire to the target and in the tool's environment.
const COMMUNITY: &str = "c0mmunity-n3ver-in-argv";
const OID: &str = "1.3.6.1.4.1.99.1.0";

// ─── namespaces ─────────────────────────────────────────────────────────

struct Lab {
    unit: String,
    switch: String,
    target: String,
    twin: String,
    lte: String,
    mesh: String,
    dir: PathBuf,
}

impl Lab {
    fn all(&self) -> [&str; 6] {
        [
            &self.unit,
            &self.switch,
            &self.target,
            &self.twin,
            &self.lte,
            &self.mesh,
        ]
    }

    fn create() -> Self {
        let pid = std::process::id();
        let lab = Self {
            unit: format!("sctlnt-unit-{pid}"),
            switch: format!("sctlnt-sw-{pid}"),
            target: format!("sctlnt-tgt-{pid}"),
            twin: format!("sctlnt-twin-{pid}"),
            lte: format!("sctlnt-lte-{pid}"),
            mesh: format!("sctlnt-mesh-{pid}"),
            dir: std::env::temp_dir().join(format!("sctl-net-tools-netns-{pid}")),
        };
        std::fs::create_dir_all(&lab.dir).unwrap();
        for ns in lab.all() {
            ip(&["netns", "add", ns]);
            // No IPv6 chatter: every frame on the wire is one a case made.
            sysctl(ns, "net.ipv6.conf.all.disable_ipv6=1");
            sysctl(ns, "net.ipv6.conf.default.disable_ipv6=1");
            ip(&["-n", ns, "link", "set", "lo", "up"]);
        }
        let (unit, switch, target, twin, lte, mesh) = (
            lab.unit.as_str(),
            lab.switch.as_str(),
            lab.target.as_str(),
            lab.twin.as_str(),
            lab.lte.as_str(),
            lab.mesh.as_str(),
        );
        for (name, ns, mac, peer, peer_ns) in [
            ("lan0", unit, Some(UNIT_MAC), "lan0p", switch),
            ("t0", target, Some(TARGET_MAC), "t0p", switch),
            ("w0", twin, Some(TWIN_MAC), "w0p", switch),
            ("lte9", unit, None, "lte9p", lte),
            ("wg9", unit, None, "wg9p", mesh),
        ] {
            let mut args = vec!["link", "add", name, "netns", ns];
            if let Some(mac) = mac {
                args.extend(["address", mac]);
            }
            args.extend(["type", "veth", "peer", "name", peer, "netns", peer_ns]);
            ip(&args);
        }
        ip(&["-n", switch, "link", "add", "br0", "type", "bridge"]);
        for port in ["lan0p", "t0p", "w0p"] {
            ip(&["-n", switch, "link", "set", port, "master", "br0"]);
            ip(&["-n", switch, "link", "set", port, "up"]);
        }
        ip(&["-n", switch, "link", "set", "br0", "up"]);

        ip(&["-n", unit, "addr", "add", "192.0.2.1/24", "dev", "lan0"]);
        ip(&["-n", unit, "link", "set", "lan0", "up"]);
        ip(&["-n", unit, "addr", "add", "198.51.100.2/24", "dev", "lte9"]);
        ip(&["-n", unit, "link", "set", "lte9", "up"]);
        ip(&[
            "-n",
            unit,
            "route",
            "add",
            "default",
            "via",
            "198.51.100.1",
            "dev",
            "lte9",
        ]);
        ip(&["-n", unit, "link", "set", "wg9", "arp", "off"]);
        ip(&["-n", unit, "addr", "add", "10.99.0.2/24", "dev", "wg9"]);
        ip(&["-n", unit, "link", "set", "wg9", "up"]);

        ip(&["-n", target, "addr", "add", "192.0.2.10/24", "dev", "t0"]);
        ip(&["-n", target, "addr", "add", "192.0.2.11/24", "dev", "t0"]);
        ip(&["-n", target, "link", "set", "t0", "up"]);
        ip(&["-n", twin, "link", "set", "w0", "up"]);

        ip(&["-n", lte, "addr", "add", "198.51.100.1/24", "dev", "lte9p"]);
        ip(&["-n", lte, "addr", "add", "203.0.113.50/32", "dev", "lo"]);
        ip(&["-n", lte, "link", "set", "lte9p", "up"]);
        ip(&["-n", mesh, "link", "set", "wg9p", "arp", "off"]);
        ip(&["-n", mesh, "addr", "add", "10.99.0.1/24", "dev", "wg9p"]);
        ip(&["-n", mesh, "link", "set", "wg9p", "up"]);
        lab
    }

    /// The agent's config: the tools read its `[lte] interface`, and the
    /// agent of case 8 runs on it. The `[comms]` plugin does not exist: the
    /// agent starts without it, as it does on a unit whose plugin fails.
    fn config(&self) -> PathBuf {
        let path = self.dir.join("sctl.toml");
        let data = self.dir.join("data");
        let state = self.dir.join("state");
        std::fs::create_dir_all(&data).unwrap();
        let text = format!(
            "[server]\nlisten = \"127.0.0.1:{AGENT_PORT}\"\ndata_dir = \"{}\"\nstate_dir = \"{}\"\n\n\
             [auth]\napi_key = \"{API_KEY}\"\n\n\
             [comms]\nprovider = \"quectel-at\"\nlibrary = \"{}\"\n\n\
             [lte]\ninterface = \"lte9\"\nwatchdog = false\n",
            data.display(),
            state.display(),
            self.dir.join("no-such-plugin.so").display(),
        );
        std::fs::write(&path, text).unwrap();
        path
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        for ns in self.all() {
            let _ = Command::new("ip").args(["netns", "del", ns]).status();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
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

/// `ip` for a clean-up that may find nothing left to remove.
fn ip_quiet(args: &[&str]) {
    let _ = Command::new("ip").args(args).output();
}

/// Wait up to 3 s for `done`: a server thread records what it got a moment
/// after the tool that sent it returns.
fn eventually(what: &str, done: impl Fn() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < Duration::from_secs(3), "{what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Run `program args` inside namespace `ns` and require success.
fn in_ns(ns: &str, program: &str, args: &[&str]) {
    let out = Command::new("ip")
        .args(["netns", "exec", ns, program])
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("{program} runs: {e}"));
    assert!(
        out.status.success(),
        "{program} {} in {ns}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn sysctl(ns: &str, setting: &str) {
    in_ns(ns, "sysctl", &["-qw", setting]);
}

/// Move this thread into namespace `ns`, and prove it did.
fn enter(ns: &str) {
    let file = std::fs::File::open(format!("/var/run/netns/{ns}")).expect("netns file");
    let target = file.metadata().unwrap().ino();
    // SAFETY: a valid namespace fd; setns only changes this thread.
    let rc = unsafe { libc::setns(file.as_raw_fd(), libc::CLONE_NEWNET) };
    assert_eq!(rc, 0, "setns: {}", std::io::Error::last_os_error());
    let now = std::fs::metadata("/proc/thread-self/ns/net").unwrap().ino();
    assert_eq!(now, target, "this thread runs in {ns}");
}

// ─── captures ───────────────────────────────────────────────────────────

/// One frame a capture saw.
#[derive(Clone, Debug)]
struct Frame {
    dev: String,
    /// Sent by this namespace (`PACKET_OUTGOING`).
    outgoing: bool,
    /// Addressed to this host's MAC (`PACKET_HOST`).
    to_host: bool,
    bytes: Vec<u8>,
}

impl Frame {
    fn ethertype(&self) -> u16 {
        u16::from_be_bytes([self.bytes[12], self.bytes[13]])
    }

    fn dst_mac(&self) -> String {
        mac_text(&self.bytes[0..6])
    }

    fn ipv4_dst(&self) -> Option<Ipv4Addr> {
        (self.bytes.len() >= 34 && self.ethertype() == 0x0800).then(|| {
            Ipv4Addr::new(
                self.bytes[30],
                self.bytes[31],
                self.bytes[32],
                self.bytes[33],
            )
        })
    }

    fn ipv4_src(&self) -> Option<Ipv4Addr> {
        (self.bytes.len() >= 34 && self.ethertype() == 0x0800).then(|| {
            Ipv4Addr::new(
                self.bytes[26],
                self.bytes[27],
                self.bytes[28],
                self.bytes[29],
            )
        })
    }

    /// The address an ARP packet asks about or answers for.
    fn arp_about(&self) -> Option<Ipv4Addr> {
        if self.bytes.len() < 42 || self.ethertype() != 0x0806 {
            return None;
        }
        let op = u16::from_be_bytes([self.bytes[20], self.bytes[21]]);
        let at = if op == 1 { 38 } else { 28 };
        Some(Ipv4Addr::new(
            self.bytes[at],
            self.bytes[at + 1],
            self.bytes[at + 2],
            self.bytes[at + 3],
        ))
    }

    /// An IPv4 packet to `ip`, or an ARP packet about it.
    fn concerns(&self, ip: Ipv4Addr) -> bool {
        self.ipv4_dst() == Some(ip) || self.arp_about() == Some(ip)
    }

    fn carries(&self, needle: &[u8]) -> bool {
        self.bytes.windows(needle.len()).any(|w| w == needle)
    }
}

fn mac_text(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Every frame on every interface of one namespace.
struct Capture {
    ns: String,
    frames: Arc<Mutex<Vec<Frame>>>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Capture {
    fn start(ns: &str) -> Self {
        let frames = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (ready, started) = std::sync::mpsc::channel();
        let handle = {
            let (ns, frames, stop) = (ns.to_string(), frames.clone(), stop.clone());
            std::thread::spawn(move || {
                enter(&ns);
                let protocol = libc::c_int::from((libc::ETH_P_ALL as u16).to_be());
                // SAFETY: socket(2) with plain integer arguments; checked.
                let fd = unsafe { libc::socket(libc::AF_PACKET, libc::SOCK_RAW, protocol) };
                assert!(
                    fd >= 0,
                    "packet socket: {}",
                    std::io::Error::last_os_error()
                );
                let timeout = libc::timeval {
                    tv_sec: 0,
                    tv_usec: 50_000,
                };
                // SAFETY: a timeval and its size.
                unsafe {
                    libc::setsockopt(
                        fd,
                        libc::SOL_SOCKET,
                        libc::SO_RCVTIMEO,
                        (&raw const timeout).cast(),
                        std::mem::size_of::<libc::timeval>() as libc::socklen_t,
                    );
                }
                ready.send(()).unwrap();
                let mut buf = vec![0u8; 65_536];
                while !stop.load(Ordering::SeqCst) {
                    // SAFETY: sockaddr_ll is plain data.
                    let mut from: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
                    let mut len = std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t;
                    // SAFETY: buf and from are live with their sizes.
                    let n = unsafe {
                        libc::recvfrom(
                            fd,
                            buf.as_mut_ptr().cast(),
                            buf.len(),
                            0,
                            (&raw mut from).cast(),
                            &raw mut len,
                        )
                    };
                    if n < 14 {
                        continue;
                    }
                    let mut name = [0 as libc::c_char; libc::IF_NAMESIZE];
                    // SAFETY: the buffer is IF_NAMESIZE bytes.
                    let found = unsafe {
                        libc::if_indextoname(from.sll_ifindex.unsigned_abs(), name.as_mut_ptr())
                    };
                    let dev = if found.is_null() {
                        String::new()
                    } else {
                        // SAFETY: NUL-terminated on success.
                        unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }
                            .to_string_lossy()
                            .into_owned()
                    };
                    frames.lock().unwrap().push(Frame {
                        dev,
                        outgoing: from.sll_pkttype == libc::PACKET_OUTGOING,
                        to_host: from.sll_pkttype == libc::PACKET_HOST,
                        bytes: buf[..n.unsigned_abs()].to_vec(),
                    });
                }
                // SAFETY: fd is ours.
                unsafe { libc::close(fd) };
            })
        };
        started
            .recv_timeout(Duration::from_secs(5))
            .expect("the capture started");
        Self {
            ns: ns.to_string(),
            frames,
            stop,
            handle: Some(handle),
        }
    }

    fn mark(&self) -> usize {
        self.frames.lock().unwrap().len()
    }

    fn since(&self, mark: usize) -> Vec<Frame> {
        // Late frames of the step just run land first.
        std::thread::sleep(Duration::from_millis(250));
        self.so_far(mark)
    }

    /// The frames since `mark` as they stand, without waiting for late ones.
    fn so_far(&self, mark: usize) -> Vec<Frame> {
        self.frames.lock().unwrap()[mark..].to_vec()
    }

    fn all(&self) -> Vec<Frame> {
        self.frames.lock().unwrap().clone()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

// ─── the target's servers ───────────────────────────────────────────────

/// What the target's TCP and HTTP servers received.
#[derive(Default)]
struct Received {
    /// (local address reached, peer) of each TCP connection on 7000.
    tcp: Vec<(SocketAddr, SocketAddr)>,
    /// (local address reached, the request's head and body) on 8080.
    http: Vec<(SocketAddr, String)>,
}

fn serve_target(ns: &str, stop: &Arc<AtomicBool>) -> Arc<Mutex<Received>> {
    let received = Arc::new(Mutex::new(Received::default()));
    let (ready, started) = std::sync::mpsc::channel();
    for port in [TCP_PORT, HTTP_PORT] {
        let (ns, received, stop, ready) = (
            ns.to_string(),
            received.clone(),
            stop.clone(),
            ready.clone(),
        );
        std::thread::spawn(move || {
            enter(&ns);
            let listener = TcpListener::bind(("0.0.0.0", port)).expect("the target listens");
            listener.set_nonblocking(true).unwrap();
            ready.send(()).unwrap();
            while !stop.load(Ordering::SeqCst) {
                let Ok((mut stream, peer)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                };
                let local = stream.local_addr().unwrap();
                if port == TCP_PORT {
                    received.lock().unwrap().tcp.push((local, peer));
                    continue;
                }
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let request = read_request(&mut stream);
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                received.lock().unwrap().http.push((local, request));
                let reply = match path.as_str() {
                    "/redirect-other" => {
                        redirect(302, &format!("http://{OTHER}:{HTTP_PORT}/landed"))
                    }
                    "/redirect-307" => redirect(307, &format!("http://{OTHER}:{HTTP_PORT}/landed")),
                    "/redirect-gateway" => {
                        redirect(302, &format!("http://{BEHIND_GATEWAY}:{HTTP_PORT}/x"))
                    }
                    _ => "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
                        .to_string(),
                };
                let _ = stream.write_all(reply.as_bytes());
            }
        });
    }
    for _ in 0..2 {
        started
            .recv_timeout(Duration::from_secs(5))
            .expect("the target's servers started");
    }
    received
}

fn redirect(status: u16, location: &str) -> String {
    format!(
        "HTTP/1.1 {status} Moved\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
}

/// A request's head, then its body as Content-Length says.
fn read_request(stream: &mut std::net::TcpStream) -> String {
    let mut seen = Vec::new();
    let mut buf = [0u8; 4096];
    while let Ok(n @ 1..) = stream.read(&mut buf) {
        seen.extend_from_slice(&buf[..n]);
        let text = String::from_utf8_lossy(&seen).to_string();
        if let Some(end) = text.find("\r\n\r\n") {
            let want = text
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                })
                .unwrap_or(0);
            if seen.len() >= end + 4 + want {
                break;
            }
        }
    }
    String::from_utf8_lossy(&seen).to_string()
}

// ─── the tools ──────────────────────────────────────────────────────────

/// One run of `sctl net`.
#[derive(Debug)]
struct Run {
    code: i32,
    stdout: String,
    stderr: String,
    json: Value,
}

impl Run {
    fn of(out: &Output) -> Self {
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        Self {
            code: out.status.code().unwrap_or(-1),
            json: serde_json::from_str(stdout.trim()).unwrap_or(Value::Null),
            stdout,
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
        }
    }
}

/// `sctl net <args>` in namespace `ns`, with an environment of `PATH` and
/// `env` only.
fn net_command(ns: &str, args: &[&str], env: &[(&str, &str)]) -> Command {
    let mut cmd = Command::new("ip");
    cmd.args(["netns", "exec", ns, SCTL, "net"])
        .args(args)
        .env_clear()
        .env("PATH", PATH)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        cmd.env(key, value);
    }
    cmd
}

fn net(ns: &str, args: &[&str], env: &[(&str, &str)]) -> Run {
    let out = net_command(ns, args, env).output().expect("sctl net runs");
    Run::of(&out)
}

// ─── the agent of case 8 ────────────────────────────────────────────────

/// `sctl serve` in the unit's namespace, killed on drop.
struct RunningAgent {
    child: Child,
}

impl RunningAgent {
    fn start(ns: &str, config: &Path, dir: &Path) -> Self {
        let child = Command::new("ip")
            .args(["netns", "exec", ns, SCTL, "serve", "--config"])
            .arg(config)
            .env_clear()
            .env("PATH", PATH)
            .env("SCTL_INSTALL_JSON", dir.join("no-install.json"))
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(dir.join("agent.log")).unwrap())
            .stderr(std::fs::File::create(dir.join("agent.err")).unwrap())
            .spawn()
            .expect("the agent starts");
        Self { child }
    }
}

impl Drop for RunningAgent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One request to the agent's API from inside namespace `ns`: the status
/// and the body.
fn api(ns: &str, method: &str, path: &str, body: Option<&Value>) -> (u16, String) {
    try_api(ns, method, path, body).expect("the agent answers")
}

fn try_api(
    ns: &str,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> std::io::Result<(u16, String)> {
    let (ns, method, path) = (ns.to_string(), method.to_string(), path.to_string());
    let body = body.map(Value::to_string);
    std::thread::spawn(move || {
        enter(&ns);
        let mut stream = std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, AGENT_PORT))?;
        stream.set_read_timeout(Some(Duration::from_mins(1)))?;
        let body = body.unwrap_or_default();
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {API_KEY}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )?;
        let mut answer = String::new();
        stream.read_to_string(&mut answer)?;
        let status = answer
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let body = answer
            .split_once("\r\n\r\n")
            .map(|(_, b)| b.to_string())
            .unwrap_or_default();
        Ok::<_, std::io::Error>((status, body))
    })
    .join()
    .unwrap()
}

// ─── the scenario ───────────────────────────────────────────────────────

#[test]
#[ignore = "needs root: sudo -n env SCTL_NETNS_TEST=1 <test binary> --ignored"]
fn the_net_tools_send_only_on_the_lan_to_the_device_expected() {
    if std::env::var_os("SCTL_NETNS_TEST").is_none() {
        eprintln!("skipped: set SCTL_NETNS_TEST=1 and run the test binary as root");
        return;
    }
    // SAFETY: geteuid has no preconditions.
    assert_eq!(unsafe { libc::geteuid() }, 0, "SCTL_NETNS_TEST needs root");
    assert!(
        SCTL.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b)),
        "case 8 puts the binary's path in a shell command; this one needs quoting: {SCTL}"
    );

    let lab = Lab::create();
    let config = lab.config();
    let cfg = config.to_str().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let received = serve_target(&lab.target, &stop);
    let snmp = {
        let ns = lab.target.clone();
        Agent::start(
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 161)),
            Duration::ZERO,
            move || enter(&ns),
        )
    };
    let unit = Capture::start(&lab.unit);
    let lte = Capture::start(&lab.lte);
    let mesh = Capture::start(&lab.mesh);
    let twin = Capture::start(&lab.twin);
    let env = [("SCTL_AGENT_CONFIG", cfg)];
    let u = lab.unit.as_str();

    case_1_on_the_lan_is_sent(u, &env, &received, &snmp, &lte);
    case_2_behind_the_gateway_is_refused(u, &env, &unit, &lte);
    case_3_the_mesh_and_a_gatewayless_lte_route_are_refused(u, &env, &unit, &lte, &mesh);
    case_4_a_host_route_and_dev(u, &env, &received, &snmp, &unit, &lte);
    case_5_marks_do_not_move_a_bound_request(u, &env, &received, &snmp, &lte);
    case_6_expect_mac(&lab, &env, &received, &unit, &twin);
    case_7_redirects(u, &env, &received, &unit, &lte);
    case_8_the_community(&lab, &config, &env, &snmp);

    // On every wire, the community travels only to and from the target:
    // never by the LTE veth, never to the twin or the mesh. The unit's
    // loopback carries it to its own agent, which is not a wire.
    for capture in [&unit, &lte, &mesh, &twin] {
        let leaked: Vec<Frame> = capture
            .all()
            .into_iter()
            .filter(|f| {
                f.dev != "lo"
                    && f.carries(COMMUNITY.as_bytes())
                    && f.ipv4_dst() != Some(TARGET)
                    && f.ipv4_src() != Some(TARGET)
            })
            .collect();
        assert!(
            leaked.is_empty(),
            "the community went somewhere other than the target in {}: {leaked:?}",
            capture.ns
        );
    }
    stop.store(true, Ordering::SeqCst);
}

fn case_1_on_the_lan_is_sent(
    u: &str,
    env: &[(&str, &str)],
    received: &Arc<Mutex<Received>>,
    snmp: &Agent,
    lte: &Capture,
) {
    let mark = lte.mark();
    let tcp = net(u, &["tcp", "192.0.2.10", "7000"], env);
    assert_eq!(tcp.code, 0, "{tcp:?}");
    assert_eq!(tcp.json["dev"], "lan0", "{tcp:?}");
    eventually("the target accepted the connection", || {
        received
            .lock()
            .unwrap()
            .tcp
            .iter()
            .any(|(local, _)| local.ip() == TARGET)
    });

    let http = net(u, &["http", "http://192.0.2.10:8080/ok"], env);
    assert_eq!(http.code, 0, "{http:?}");
    assert_eq!(http.json["status"], 200, "{http:?}");

    let before = snmp.requests().len();
    let get = net(u, &["snmp", "get", "192.0.2.10", "1.3.6.1.2.1.1.1.0"], env);
    assert_eq!(get.code, 0, "{get:?}");
    assert_eq!(get.json["dev"], "lan0");
    let seen = snmp.requests();
    assert_eq!(seen.len(), before + 1);
    assert_eq!(seen[before].community, b"public", "unset: public for a get");
    assert_eq!(seen[before].kind, 0xA0, "a GET");
    assert_eq!(
        seen[before].from.ip(),
        Ipv4Addr::new(192, 0, 2, 1),
        "from the address the route gives"
    );
    assert!(lte.since(mark).is_empty(), "nothing by the LTE veth");
    eprintln!("case 1: tcp, http and snmp get reach the target by lan0");
}

fn case_2_behind_the_gateway_is_refused(
    u: &str,
    env: &[(&str, &str)],
    unit: &Capture,
    lte: &Capture,
) {
    let (mark, lte_mark) = (unit.mark(), lte.mark());
    let mut with_community = env.to_vec();
    with_community.push(("SCTL_SNMP_COMMUNITY", COMMUNITY));
    let set = net(
        u,
        &["snmp", "set", "203.0.113.50", OID, "i", "1", "-v", "1"],
        &with_community,
    );
    assert_eq!(set.code, 3, "{set:?}");
    assert_eq!(set.json["refused"], "not_on_lan");
    assert_eq!(set.json["ok"], false);
    assert_eq!(
        set.json["route"],
        json!({"dev": "lte9", "role": "lte", "role_by": "config", "gateway": "198.51.100.1", "type": "unicast"}),
        "the config's [lte] interface names the bearer"
    );
    assert_eq!(
        set.json["lan"],
        json!([{"dev": "lan0", "ipv4": ["192.0.2.1/24"]}])
    );
    assert!(!set.stdout.contains(COMMUNITY) && !set.stderr.contains(COMMUNITY));
    let wire: Vec<Frame> = unit
        .since(mark)
        .into_iter()
        .filter(|f| f.outgoing && (f.concerns(BEHIND_GATEWAY) || f.dev == "lte9"))
        .collect();
    assert!(wire.is_empty(), "nothing for it on any veth: {wire:?}");
    assert!(lte.since(lte_mark).is_empty(), "the LTE side got nothing");
    eprintln!("case 2: {}", set.json["reason"]);
}

fn case_3_the_mesh_and_a_gatewayless_lte_route_are_refused(
    u: &str,
    env: &[(&str, &str)],
    unit: &Capture,
    lte: &Capture,
    mesh: &Capture,
) {
    let (mark, mesh_mark) = (unit.mark(), mesh.mark());
    let wg = net(u, &["tcp", "10.99.0.1", "7000"], env);
    assert_eq!(wg.code, 3, "{wg:?}");
    assert_eq!(wg.json["refused"], "not_on_lan");
    assert_eq!(wg.json["route"]["role"], "tunnel", "{wg:?}");
    assert!(mesh.since(mesh_mark).is_empty(), "the mesh got nothing");
    let by_wg: Vec<Frame> = unit
        .since(mark)
        .into_iter()
        .filter(|f| f.outgoing && f.dev == "wg9")
        .collect();
    assert!(by_wg.is_empty(), "{by_wg:?}");

    ip(&["-n", u, "route", "replace", "default", "dev", "lte9"]);
    let (mark, lte_mark) = (unit.mark(), lte.mark());
    let bare = net(u, &["tcp", "203.0.113.60", "7000"], env);
    ip(&[
        "-n",
        u,
        "route",
        "replace",
        "default",
        "via",
        "198.51.100.1",
        "dev",
        "lte9",
    ]);
    assert_eq!(bare.code, 3, "{bare:?}");
    assert_eq!(bare.json["refused"], "not_on_lan");
    assert_eq!(bare.json["route"]["dev"], "lte9");
    assert_eq!(bare.json["route"]["gateway"], Value::Null);
    assert_eq!(bare.json["route"]["role"], "lte");
    assert!(lte.since(lte_mark).is_empty(), "nothing by the LTE veth");
    assert!(unit
        .since(mark)
        .iter()
        .all(|f| !(f.outgoing && f.dev == "lte9")));
    eprintln!("case 3: the mesh and a gatewayless LTE route are refused");
}

fn case_4_a_host_route_and_dev(
    u: &str,
    env: &[(&str, &str)],
    received: &Arc<Mutex<Received>>,
    snmp: &Agent,
    unit: &Capture,
    lte: &Capture,
) {
    // As on 2026-10-16: a /32 address on the port and a host route.
    ip(&["-n", u, "addr", "del", "192.0.2.1/24", "dev", "lan0"]);
    ip(&["-n", u, "addr", "add", "192.0.2.1/32", "dev", "lan0"]);
    ip(&["-n", u, "route", "add", "192.0.2.10/32", "dev", "lan0"]);
    ip(&["-n", u, "neigh", "flush", "dev", "lan0"]);
    let connections = received.lock().unwrap().tcp.len();
    let tcp = net(u, &["tcp", "192.0.2.10", "7000"], env);
    assert_eq!(tcp.code, 0, "{tcp:?}");
    assert_eq!(tcp.json["dev"], "lan0");
    eventually("the target accepted the connection", || {
        received.lock().unwrap().tcp.len() == connections + 1
    });

    // --dev, and the address removed mid-run, between the guard's approval
    // and the bind. With --expect-mac the tool waits 300 ms past the
    // target's ARP answer for a second one before it binds, so the address
    // goes in that wait. The bind to 192.0.2.1 then fails, and the request
    // is refused: it never leaves, neither by the LTE veth, where an
    // unbound socket would send it once the host route went with the
    // address, nor out lan0 from another source, where a socket bound to
    // the interface alone would.
    let (mark, lte_mark) = (unit.mark(), lte.mark());
    let requests = snmp.requests().len();
    let child = net_command(
        u,
        &[
            "snmp",
            "get",
            "--dev",
            "lan0",
            "--expect-mac",
            TARGET_MAC,
            "192.0.2.10",
            "1.3.6.1.2.1.1.1.0",
        ],
        env,
    )
    .spawn()
    .unwrap();
    let started = Instant::now();
    while !unit
        .so_far(mark)
        .iter()
        .any(|f| f.dev == "lan0" && !f.outgoing && f.to_host && f.arp_about() == Some(TARGET))
    {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the target answered the ARP request"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    ip(&["-n", u, "addr", "del", "192.0.2.1/32", "dev", "lan0"]);
    let gone = unit.mark();
    let run = Run::of(&child.wait_with_output().unwrap());
    assert_eq!(run.code, 3, "the bind fails, so it is refused: {run:?}");
    assert_eq!(run.json["refused"], "guard_failed", "{run:?}");
    assert_eq!(
        snmp.requests().len(),
        requests,
        "no request reached the agent"
    );
    let sent: Vec<Frame> = unit
        .since(gone)
        .into_iter()
        .filter(|f| {
            f.outgoing && (f.dev == "lte9" || (f.dev == "lan0" && f.ipv4_dst() == Some(TARGET)))
        })
        .collect();
    assert!(sent.is_empty(), "nothing after the address went: {sent:?}");
    assert!(
        lte.since(lte_mark).is_empty(),
        "never by the LTE veth after the address went"
    );

    // An address under no route through the named veth: refused, nothing
    // on the wire, ARP included.
    ip(&["-n", u, "addr", "add", "192.0.2.1/32", "dev", "lan0"]);
    let mark = unit.mark();
    let none = net(
        u,
        &[
            "tcp",
            "--dev",
            "lan0",
            "--expect-mac",
            TARGET_MAC,
            "192.0.2.99",
            "7000",
        ],
        env,
    );
    assert_eq!(none.code, 3, "{none:?}");
    assert_eq!(none.json["refused"], "not_on_lan");
    assert_eq!(none.json["dev"], "lan0");
    let wire: Vec<Frame> = unit
        .since(mark)
        .into_iter()
        .filter(|f| f.outgoing && f.concerns(Ipv4Addr::new(192, 0, 2, 99)))
        .collect();
    assert!(wire.is_empty(), "nothing on the wire: {wire:?}");

    // Removing the port's last address took the host route with it.
    ip_quiet(&["-n", u, "route", "del", "192.0.2.10/32", "dev", "lan0"]);
    ip(&["-n", u, "addr", "del", "192.0.2.1/32", "dev", "lan0"]);
    ip(&["-n", u, "addr", "add", "192.0.2.1/24", "dev", "lan0"]);
    eprintln!("case 4: a /32 and a host route pass; --dev holds to its routes");
}

/// The marking tool on this host: nft, else iptables.
fn mark_own_traffic(u: &str, on: bool) {
    let has = |tool: &str| {
        Command::new(tool)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    };
    if has("nft") {
        if on {
            in_ns(u, "nft", &["add", "table", "ip", "sctl_mangle"]);
            in_ns(
                u,
                "nft",
                &[
                    "add",
                    "chain",
                    "ip",
                    "sctl_mangle",
                    "output",
                    "{",
                    "type",
                    "route",
                    "hook",
                    "output",
                    "priority",
                    "mangle",
                    ";",
                    "}",
                ],
            );
            in_ns(
                u,
                "nft",
                &[
                    "add",
                    "rule",
                    "ip",
                    "sctl_mangle",
                    "output",
                    "meta",
                    "mark",
                    "set",
                    "0x100",
                ],
            );
        } else {
            in_ns(u, "nft", &["delete", "table", "ip", "sctl_mangle"]);
        }
    } else if has("iptables") {
        let verb = if on { "-A" } else { "-D" };
        in_ns(
            u,
            "iptables",
            &[
                "-t",
                "mangle",
                verb,
                "OUTPUT",
                "-j",
                "MARK",
                "--set-mark",
                "0x100",
            ],
        );
    } else {
        panic!("case 5 needs nft or iptables on the PATH to mark traffic as mwan3 does");
    }
}

fn case_5_marks_do_not_move_a_bound_request(
    u: &str,
    env: &[(&str, &str)],
    received: &Arc<Mutex<Received>>,
    snmp: &Agent,
    lte: &Capture,
) {
    // As mwan3 does: the unit's own traffic is marked, and the mark leads
    // to a table whose default route is the LTE veth.
    ip(&[
        "-n",
        u,
        "route",
        "add",
        "default",
        "via",
        "198.51.100.1",
        "dev",
        "lte9",
        "table",
        "100",
    ]);
    ip(&[
        "-n", u, "rule", "add", "fwmark", "0x100", "lookup", "100", "pref", "100",
    ]);
    mark_own_traffic(u, true);

    // The steering works: an unbound packet leaves by the LTE veth.
    let control_mark = lte.mark();
    {
        let ns = u.to_string();
        std::thread::spawn(move || {
            enter(&ns);
            let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
            socket.send_to(b"steered", (TARGET, 9)).unwrap();
        })
        .join()
        .unwrap();
    }
    let control = lte.since(control_mark);
    assert!(
        control.iter().any(|f| f.carries(b"steered")),
        "the marks steer unbound traffic to the LTE veth: {control:?}"
    );

    let mark = lte.mark();
    let (connections, pages) = {
        let r = received.lock().unwrap();
        (r.tcp.len(), r.http.len())
    };
    let requests = snmp.requests().len();
    let tcp = net(u, &["tcp", "192.0.2.10", "7000"], env);
    let http = net(u, &["http", "http://192.0.2.10:8080/ok"], env);
    let get = net(u, &["snmp", "get", "192.0.2.10", "1.3.6.1.2.1.1.1.0"], env);
    mark_own_traffic(u, false);
    ip(&[
        "-n", u, "rule", "del", "fwmark", "0x100", "lookup", "100", "pref", "100",
    ]);
    ip(&["-n", u, "route", "del", "default", "table", "100"]);
    assert_eq!(
        (tcp.code, http.code, get.code),
        (0, 0, 0),
        "{tcp:?} {http:?} {get:?}"
    );
    eventually("the target got the connection and the page", || {
        let r = received.lock().unwrap();
        r.tcp.len() == connections + 1 && r.http.len() == pages + 1
    });
    assert_eq!(snmp.requests().len(), requests + 1);
    let moved = lte.since(mark);
    assert!(moved.is_empty(), "no bound request moved to LTE: {moved:?}");
    eprintln!("case 5: mwan3's marks do not move a bound request");
}

fn case_6_expect_mac(
    lab: &Lab,
    env: &[(&str, &str)],
    received: &Arc<Mutex<Received>>,
    unit: &Capture,
    twin: &Capture,
) {
    let u = lab.unit.as_str();
    let only_arp = |frames: &[Frame], at: Ipv4Addr| {
        let sent: Vec<&Frame> = frames
            .iter()
            .filter(|f| f.outgoing && f.dev == "lan0" && f.concerns(at))
            .collect();
        assert!(
            sent.iter().all(|f| f.ethertype() == 0x0806),
            "only the ARP request: {sent:?}"
        );
        assert!(!sent.is_empty(), "the ARP request went out");
    };

    // The target's MAC, also after the neighbour table was flushed, in
    // either case and with hyphens.
    for mac in [TARGET_MAC, "02-00-00-00-00-0A"] {
        ip(&["-n", u, "neigh", "flush", "dev", "lan0"]);
        let ok = net(u, &["tcp", "--expect-mac", mac, "192.0.2.10", "7000"], env);
        assert_eq!(ok.code, 0, "{ok:?}");
    }

    // Another MAC.
    let mark = unit.mark();
    let other = net(
        u,
        &[
            "tcp",
            "--expect-mac",
            "02:00:00:00:00:99",
            "192.0.2.10",
            "7000",
        ],
        env,
    );
    assert_eq!(other.code, 3, "{other:?}");
    assert_eq!(other.json["refused"], "mac_mismatch");
    assert_eq!(other.json["macs"], json!([TARGET_MAC]));
    only_arp(&unit.since(mark), TARGET);

    // Two answers: the twin takes the target's address too.
    ip(&["-n", &lab.twin, "addr", "add", "192.0.2.10/24", "dev", "w0"]);
    let mark = unit.mark();
    let two = net(
        u,
        &["tcp", "--expect-mac", TARGET_MAC, "192.0.2.10", "7000"],
        env,
    );
    ip(&["-n", &lab.twin, "addr", "del", "192.0.2.10/24", "dev", "w0"]);
    assert_eq!(two.code, 3, "{two:?}");
    assert_eq!(two.json["refused"], "mac_mismatch");
    let macs = two.json["macs"].as_array().cloned().unwrap_or_default();
    assert!(
        macs.contains(&json!(TARGET_MAC)) && macs.contains(&json!(TWIN_MAC)),
        "{two:?}"
    );
    only_arp(&unit.since(mark), TARGET);

    // No answer.
    let mark = unit.mark();
    let silent = net(
        u,
        &["tcp", "--expect-mac", TARGET_MAC, "192.0.2.77", "7000"],
        env,
    );
    assert_eq!(silent.code, 3, "{silent:?}");
    assert_eq!(silent.json["refused"], "no_answer");
    only_arp(&unit.since(mark), Ipv4Addr::new(192, 0, 2, 77));

    // A STALE entry still naming the twin, as after an address shuffle:
    // nothing reaches the twin; the payload goes to the target's MAC, or
    // nothing is sent. Once at once (within the kernel's ARP locktime),
    // once past it.
    for wait in [Duration::ZERO, Duration::from_millis(1500)] {
        ip(&[
            "-n",
            u,
            "neigh",
            "replace",
            "192.0.2.10",
            "lladdr",
            TWIN_MAC,
            "dev",
            "lan0",
            "nud",
            "stale",
        ]);
        std::thread::sleep(wait);
        let (mark, twin_mark) = (unit.mark(), twin.mark());
        let connections = received.lock().unwrap().tcp.len();
        let run = net(
            u,
            &["tcp", "--expect-mac", TARGET_MAC, "192.0.2.10", "7000"],
            env,
        );
        let at_twin: Vec<Frame> = twin
            .since(twin_mark)
            .into_iter()
            .filter(|f| f.to_host && f.ethertype() == 0x0800)
            .collect();
        assert!(at_twin.is_empty(), "nothing reaches the twin: {at_twin:?}");
        let payload: Vec<Frame> = unit
            .since(mark)
            .into_iter()
            .filter(|f| f.outgoing && f.ipv4_dst() == Some(TARGET))
            .collect();
        assert!(
            payload.iter().all(|f| f.dst_mac() == TARGET_MAC),
            "the payload goes to the target's MAC: {payload:?}"
        );
        match run.code {
            0 => eventually("the target accepted the connection", || {
                received.lock().unwrap().tcp.len() == connections + 1
            }),
            3 => {
                assert_eq!(run.json["refused"], "mac_mismatch", "{run:?}");
                assert_eq!(run.json["neighbour"], TWIN_MAC, "{run:?}");
                assert!(payload.is_empty());
            }
            _ => panic!("{run:?}"),
        }
        if !wait.is_zero() {
            assert_eq!(
                run.code, 0,
                "past the locktime the answer updates the entry: {run:?}"
            );
        }
        eprintln!("case 6: a stale entry after {wait:?}: exit {}", run.code);
    }
}

fn case_7_redirects(
    u: &str,
    env: &[(&str, &str)],
    received: &Arc<Mutex<Received>>,
    unit: &Capture,
    lte: &Capture,
) {
    let landed = |since: usize| -> Vec<String> {
        received.lock().unwrap().http[since..]
            .iter()
            .filter(|(local, _)| local.ip() == OTHER)
            .map(|(_, request)| request.clone())
            .collect()
    };

    let since = received.lock().unwrap().http.len();
    let follow = net(
        u,
        &[
            "http",
            "http://192.0.2.10:8080/redirect-other",
            "-X",
            "POST",
            "-u",
            "admin:p4ssw0rd",
            "-H",
            "X-Custom: kept-at-home",
            "-d",
            "pass=b0dy-s3cret",
        ],
        env,
    );
    assert_eq!(follow.code, 0, "{follow:?}");
    assert_eq!(follow.json["status"], 200);
    let at_other = landed(since);
    assert_eq!(at_other.len(), 1, "{at_other:?}");
    let lower = at_other[0].to_ascii_lowercase();
    assert!(!lower.contains("authorization"), "{}", at_other[0]);
    assert!(!lower.contains("x-custom"), "{}", at_other[0]);
    assert!(!at_other[0].contains("b0dy-s3cret"), "{}", at_other[0]);

    let since = received.lock().unwrap().http.len();
    let form = net(
        u,
        &[
            "http",
            "http://192.0.2.10:8080/redirect-307",
            "-F",
            "pass=b0dy-s3cret",
        ],
        env,
    );
    assert_eq!(form.code, 1, "{form:?}");
    assert_eq!(form.json["status"], 307);
    assert!(form.json["not_followed"]["reason"].is_string(), "{form:?}");
    assert!(landed(since).is_empty(), "the other host got nothing");

    let (mark, lte_mark) = (unit.mark(), lte.mark());
    let away = net(u, &["http", "http://192.0.2.10:8080/redirect-gateway"], env);
    assert_eq!(away.code, 1, "{away:?}");
    assert_eq!(away.json["status"], 302);
    assert_eq!(
        away.json["not_followed"]["refused"], "not_on_lan",
        "{away:?}"
    );
    assert!(lte.since(lte_mark).is_empty(), "nothing by the LTE veth");
    assert!(unit
        .since(mark)
        .iter()
        .all(|f| !(f.outgoing && f.concerns(BEHIND_GATEWAY))));

    // With a MAC to prove, a redirect to another address is not followed.
    let since = received.lock().unwrap().http.len();
    let pinned = net(
        u,
        &[
            "http",
            "--expect-mac",
            TARGET_MAC,
            "http://192.0.2.10:8080/redirect-other",
        ],
        env,
    );
    assert_eq!(pinned.code, 1, "{pinned:?}");
    assert!(pinned.json["not_followed"]["reason"]
        .as_str()
        .is_some_and(|r| r.contains("192.0.2.11")));
    assert!(landed(since).is_empty());
    eprintln!("case 7: redirects keep credentials with their origin");
}

fn case_8_the_community(lab: &Lab, config: &Path, env: &[(&str, &str)], snmp: &Agent) {
    let u = lab.unit.as_str();
    let mut with_community = env.to_vec();
    with_community.push(("SCTL_SNMP_COMMUNITY", COMMUNITY));
    let clean = |what: &str, text: &str| {
        assert!(!text.contains(COMMUNITY), "the community in {what}: {text}");
    };

    // The set reaches the agent with the community, and while the agent
    // holds its answer the tool's argv does not carry it.
    snmp.set_delay(Duration::from_millis(1500));
    let before = snmp.requests().len();
    let child = net_command(
        u,
        &["snmp", "set", "192.0.2.10", OID, "i", "1"],
        &with_community,
    )
    .spawn()
    .unwrap();
    let started = Instant::now();
    while snmp.requests().len() == before {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the set arrived"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let cmdline = std::fs::read(format!("/proc/{}/cmdline", child.id())).unwrap();
    let argv = String::from_utf8_lossy(&cmdline).replace(char::from(0), " ");
    assert!(argv.contains("net snmp set"), "the tool's own argv: {argv}");
    clean("argv", &argv);
    let set = Run::of(&child.wait_with_output().unwrap());
    snmp.set_delay(Duration::ZERO);
    assert_eq!(set.code, 0, "{set:?}");
    assert_eq!(snmp.requests()[before].kind, 0xA3, "a SET");
    assert_eq!(snmp.requests()[before].community, COMMUNITY.as_bytes());
    clean("stdout", &set.stdout);
    clean("stderr", &set.stderr);

    // A refusal, the -c usage error and an empty variable.
    let refused = net(
        u,
        &["snmp", "set", "203.0.113.50", OID, "i", "1"],
        &with_community,
    );
    assert_eq!(refused.code, 3, "{refused:?}");
    clean(
        "a refusal",
        &format!("{}{}", refused.stdout, refused.stderr),
    );
    let requests = snmp.requests().len();
    let flag = net(
        u,
        &["snmp", "set", "192.0.2.10", OID, "i", "1", "-c", COMMUNITY],
        env,
    );
    assert_eq!(flag.code, 2, "{flag:?}");
    assert!(flag
        .stderr
        .contains("SCTL_SNMP_COMMUNITY=\"${SCTL_CRED_PASS_1}\""));
    clean(
        "the -c usage error",
        &format!("{}{}", flag.stdout, flag.stderr),
    );
    let mut empty = env.to_vec();
    empty.push(("SCTL_SNMP_COMMUNITY", ""));
    let unset = net(u, &["snmp", "get", "192.0.2.10", OID], &empty);
    assert_eq!(
        unset.code, 2,
        "set but empty is refused, never public: {unset:?}"
    );
    assert_eq!(snmp.requests().len(), requests, "neither was sent");

    // Through the agent: its exec gives the command SCTL_AGENT_CONFIG, the
    // activity log keeps the command as written, and the Infra SNMP check's
    // detail quotes no community.
    let agent = RunningAgent::start(u, config, &lab.dir);
    let started = Instant::now();
    loop {
        if matches!(try_api(u, "GET", "/api/health", None), Ok((200, _))) {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the agent answers: {}",
            std::fs::read_to_string(lab.dir.join("agent.err")).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    let exec = |host: &str| {
        let command = format!(
            "SCTL_SNMP_COMMUNITY=\"$SCTL_CRED_PASS_1\" '{SCTL}' net snmp set {host} {OID} i 1"
        );
        let (status, body) = api(
            u,
            "POST",
            "/api/exec",
            Some(&json!({
                "command": command,
                "env": {"SCTL_CRED_PASS_1": COMMUNITY},
                "timeout_ms": 20_000,
            })),
        );
        assert_eq!(status, 200, "{body}");
        let answer: Value = serde_json::from_str(&body).unwrap();
        clean("exec's answer", &body);
        answer
    };
    let before = snmp.requests().len();
    let delivered = exec("192.0.2.10");
    assert_eq!(delivered["exit_code"], 0, "{delivered}");
    assert_eq!(snmp.requests()[before].community, COMMUNITY.as_bytes());
    let refused = exec("203.0.113.50");
    assert_eq!(refused["exit_code"], 3, "{refused}");
    let tool: Value = serde_json::from_str(refused["stdout"].as_str().unwrap().trim()).unwrap();
    assert_eq!(
        (
            tool["route"]["dev"].as_str(),
            tool["route"]["role_by"].as_str()
        ),
        (Some("lte9"), Some("config")),
        "the exec child read [lte] interface through SCTL_AGENT_CONFIG: {tool}"
    );

    let (status, body) = api(
        u,
        "POST",
        "/api/infra/config",
        Some(&json!({
            "version": 7,
            "check_interval_secs": 3600,
            "targets": [
                {"id": "bar", "name": "bar", "check": {"method": "snmp", "host": "192.0.2.10", "community": COMMUNITY, "timeout_ms": 2000}},
                {"id": "far", "name": "far", "check": {"method": "snmp", "host": "203.0.113.50", "community": COMMUNITY, "timeout_ms": 2000}},
            ],
        })),
    );
    assert_eq!(status, 200, "{body}");
    for (id, sent) in [("bar", true), ("far", false)] {
        let (status, body) = api(u, "POST", &format!("/api/infra/check/{id}"), None);
        assert_eq!(status, 200, "{body}");
        clean("a check's detail", &body);
        let result: Value = serde_json::from_str(&body).unwrap();
        let detail = result["detail"].as_str().unwrap_or_default();
        if sent {
            assert_eq!(result["ok"], true, "{body}");
        } else {
            assert!(
                detail.starts_with("SNMP NOT SENT (not_on_lan)"),
                "off the LAN it reads down with the guard's reason: {body}"
            );
        }
    }
    let (status, results) = api(u, "GET", "/api/infra/results", None);
    assert_eq!(status, 200);
    clean("the results", &results);
    let (status, activity) = api(u, "GET", "/api/activity", None);
    assert_eq!(status, 200);
    assert!(
        activity.contains("SCTL_CRED_PASS_1"),
        "the log keeps the command as written: {activity}"
    );
    clean("the activity log", &activity);
    drop(agent);
    clean(
        "the agent's log",
        &std::fs::read_to_string(lab.dir.join("agent.log")).unwrap_or_default(),
    );
    eprintln!("case 8: the community reaches the agent and shows nowhere else");
}
