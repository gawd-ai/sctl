//! HTTP(S) downloads for the upgrade: small bodies to memory (the manifest),
//! artifacts streamed to a file with `Range` resume. TLS trust is the
//! tunnel's (public roots plus `tls_ca_file`, then the pin), and the socket
//! is dialed the way the tunnel dials (IPv4 first, `bind_address`), so an
//! artifact fetch takes the path the relay is reached by.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use http_body_util::BodyExt;
use hyper::body::Bytes;
use rustls::pki_types::ServerName;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tracing::{info, warn};

use crate::config::TunnelConfig;
use crate::tunnel::client::{build_tunnel_tls_config, connect_tcp_ipv4_preferred, tls_handshake};

/// Attempts per download (a resume continues where the last one stopped).
pub const ATTEMPTS: u32 = 4;
/// Between attempts.
const RETRY_DELAY: Duration = Duration::from_secs(3);
/// No byte for this long ends an attempt.
const IDLE_TIMEOUT: Duration = Duration::from_mins(1);

/// A parsed `http(s)://host[:port]/path`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    pub path: String,
}

impl Target {
    /// Parse a URL; `Err` names what is wrong.
    pub fn parse(url: &str) -> Result<Self, String> {
        let (tls, rest) = if let Some(r) = url.strip_prefix("https://") {
            (true, r)
        } else if let Some(r) = url.strip_prefix("http://") {
            (false, r)
        } else {
            return Err(format!("'{url}' is not an http(s) URL"));
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        if authority.is_empty() {
            return Err(format!("'{url}' has no host"));
        }
        let default_port = if tls { 443 } else { 80 };
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) if !h.contains(']') || h.ends_with(']') => match p.parse::<u16>() {
                Ok(port) => (h, port),
                Err(_) => (authority, default_port),
            },
            _ => (authority, default_port),
        };
        let host = host.trim_start_matches('[').trim_end_matches(']');
        Ok(Self {
            tls,
            host: host.to_string(),
            port,
            path: path.to_string(),
        })
    }

    fn host_header(&self) -> String {
        let default_port = if self.tls { 443 } else { 80 };
        if self.port == default_port {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// The URL this target came from, with `path` replaced.
    pub fn with_path(&self, path: &str) -> String {
        let scheme = if self.tls { "https" } else { "http" };
        format!("{scheme}://{}{path}", self.host_header())
    }

    /// The directory of `path` (through the last `/`).
    pub fn dir(&self) -> String {
        match self.path.rfind('/') {
            Some(i) => self.path[..=i].to_string(),
            None => "/".to_string(),
        }
    }
}

/// How to reach the server: the tunnel's dial and trust when the agent has
/// a tunnel, plain otherwise.
#[derive(Clone)]
pub struct Fetcher {
    tunnel: Option<TunnelConfig>,
    bearer: Option<String>,
}

/// A response's status and body, for small bodies.
pub struct Small {
    pub status: u16,
    pub body: Vec<u8>,
}

enum Io {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl Fetcher {
    pub fn new(tunnel: Option<TunnelConfig>, bearer: Option<String>) -> Self {
        Self { tunnel, bearer }
    }

    async fn connect(&self, target: &Target) -> Result<Io, String> {
        let scheme = if target.tls { "wss" } else { "ws" };
        let url = format!("{scheme}://{}:{}/", target.host, target.port);
        let tcp = match &self.tunnel {
            Some(tc) => connect_tcp_ipv4_preferred(&url, tc.bind_address.as_deref(), None)
                .await
                .map_err(|e| e.to_string())?,
            None => tokio::time::timeout(
                Duration::from_secs(15),
                TcpStream::connect((target.host.as_str(), target.port)),
            )
            .await
            .map_err(|_| format!("connect to {}:{} timed out", target.host, target.port))?
            .map_err(|e| format!("connect to {}:{}: {e}", target.host, target.port))?,
        };
        tcp.set_nodelay(true).ok();
        if !target.tls {
            return Ok(Io::Plain(tcp));
        }
        let stream = if let Some(tc) = &self.tunnel {
            tls_handshake(&url, tcp, tc)
                .await
                .map_err(|e| format!("TLS to {}: {e}", target.host))?
        } else {
            let config =
                build_tunnel_tls_config(&TunnelConfig::plain()).map_err(|e| e.to_string())?;
            let name = ServerName::try_from(target.host.clone())
                .map_err(|_| format!("'{}' is not a TLS server name", target.host))?;
            tokio_rustls::TlsConnector::from(config)
                .connect(name, tcp)
                .await
                .map_err(|e| format!("TLS to {}: {e}", target.host))?
        };
        Ok(Io::Tls(Box::new(stream)))
    }

    fn request(
        &self,
        target: &Target,
        range_from: Option<u64>,
        etag: Option<&str>,
    ) -> Result<hyper::Request<http_body_util::Empty<Bytes>>, String> {
        let mut request = hyper::Request::builder()
            .method(hyper::Method::GET)
            .uri(&target.path)
            .header(hyper::header::HOST, target.host_header())
            .header(
                hyper::header::USER_AGENT,
                format!("sctl/{}", crate::VERSION),
            );
        if let Some(bearer) = &self.bearer {
            request = request.header(hyper::header::AUTHORIZATION, format!("Bearer {bearer}"));
        }
        if let Some(from) = range_from {
            request = request.header(hyper::header::RANGE, format!("bytes={from}-"));
            // A resume continues only the file we started: a server holding
            // another file under the same name answers 200 and we start over.
            if let Some(etag) = etag {
                request = request.header(hyper::header::IF_RANGE, format!("\"{etag}\""));
            }
        }
        request
            .body(http_body_util::Empty::new())
            .map_err(|e| e.to_string())
    }

    async fn send(
        &self,
        target: &Target,
        range_from: Option<u64>,
        etag: Option<&str>,
    ) -> Result<hyper::Response<hyper::body::Incoming>, String> {
        let request = self.request(target, range_from, etag)?;
        match self.connect(target).await? {
            Io::Plain(s) => send_on(s, request).await,
            Io::Tls(s) => send_on(*s, request).await,
        }
    }

    /// GET a small body (up to `max` bytes) into memory; one attempt.
    pub async fn get_small(&self, url: &str, max: usize) -> Result<Small, String> {
        let target = Target::parse(url)?;
        let response = self.send(&target, None, None).await?;
        let status = response.status().as_u16();
        let mut body = response.into_body();
        let mut buf = Vec::new();
        loop {
            let frame = tokio::time::timeout(IDLE_TIMEOUT, body.frame())
                .await
                .map_err(|_| format!("{url}: no data for {}s", IDLE_TIMEOUT.as_secs()))?;
            let Some(frame) = frame else { break };
            let frame = frame.map_err(|e| format!("{url}: {e}"))?;
            if let Some(chunk) = frame.data_ref() {
                if buf.len() + chunk.len() > max {
                    return Err(format!("{url}: body over {max} bytes"));
                }
                buf.extend_from_slice(chunk);
            }
        }
        Ok(Small { status, body: buf })
    }

    /// GET `url` into `dest`, resuming a partial `dest` with `Range` (and
    /// `If-Range: "<etag>"`, the file's sha256, when given), up to
    /// [`ATTEMPTS`] attempts. `expected_size` bounds the download and says
    /// when a partial file is already complete.
    pub async fn get_to_file(
        &self,
        url: &str,
        dest: &Path,
        expected_size: u64,
        etag: Option<&str>,
    ) -> Result<(), String> {
        let target = Target::parse(url)?;
        let mut last = String::new();
        for attempt in 1..=ATTEMPTS {
            match self.attempt(&target, dest, expected_size, etag).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    warn!(attempt, "upgrade: download {url}: {e}");
                    last = e;
                    if attempt < ATTEMPTS {
                        tokio::time::sleep(RETRY_DELAY).await;
                    }
                }
            }
        }
        Err(format!("{url} after {ATTEMPTS} attempts: {last}"))
    }

    async fn attempt(
        &self,
        target: &Target,
        dest: &Path,
        expected_size: u64,
        etag: Option<&str>,
    ) -> Result<(), String> {
        let have = tokio::fs::metadata(dest).await.map_or(0, |m| m.len());
        if have >= expected_size && expected_size > 0 {
            return Ok(());
        }
        let range_from = (have > 0).then_some(have);
        let response = self.send(target, range_from, etag).await?;
        let status = response.status().as_u16();
        let (append, mut file) = match (status, range_from) {
            (206, Some(_)) => (
                true,
                tokio::fs::OpenOptions::new()
                    .append(true)
                    .open(dest)
                    .await
                    .map_err(|e| format!("{}: {e}", dest.display()))?,
            ),
            (200, _) => (
                false,
                tokio::fs::File::create(dest)
                    .await
                    .map_err(|e| format!("{}: {e}", dest.display()))?,
            ),
            (416, Some(_)) => {
                // The server says our offset is at or past the end: the
                // file is what we have, or is not what we think.
                let _ = tokio::fs::remove_file(dest).await;
                return Err("range not satisfiable; restarting the download".to_string());
            }
            (s, _) => return Err(format!("HTTP {s}")),
        };
        let mut written = if append { have } else { 0 };
        let mut body = response.into_body();
        loop {
            let frame = tokio::time::timeout(IDLE_TIMEOUT, body.frame())
                .await
                .map_err(|_| format!("no data for {}s", IDLE_TIMEOUT.as_secs()))?;
            let Some(frame) = frame else { break };
            let frame = frame.map_err(|e| e.to_string())?;
            if let Some(chunk) = frame.data_ref() {
                written += chunk.len() as u64;
                if written > expected_size {
                    let _ = tokio::fs::remove_file(dest).await;
                    return Err(format!("body over the manifest's {expected_size} bytes"));
                }
                file.write_all(chunk).await.map_err(|e| e.to_string())?;
            }
        }
        file.flush().await.map_err(|e| e.to_string())?;
        file.sync_all().await.map_err(|e| e.to_string())?;
        if written != expected_size {
            return Err(format!("got {written} of {expected_size} bytes"));
        }
        info!(bytes = written, "upgrade: downloaded {}", dest.display());
        Ok(())
    }
}

async fn send_on<S>(
    stream: S,
    request: hyper::Request<http_body_util::Empty<Bytes>>,
) -> Result<hyper::Response<hyper::body::Incoming>, String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let io = hyper_util::rt::TokioIo::new(stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
        .await
        .map_err(|e| e.to_string())?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    tokio::time::timeout(Duration::from_secs(30), sender.send_request(request))
        .await
        .map_err(|_| "no response in 30s".to_string())?
        .map_err(|e| e.to_string())
}

impl TunnelConfig {
    /// A client config with nothing but the public roots: what a box with
    /// no `[tunnel]` section trusts.
    pub(crate) fn plain() -> Self {
        Self {
            relay: false,
            tunnel_key: String::new(),
            url: None,
            reconnect_delay_secs: 2,
            reconnect_max_delay_secs: 30,
            heartbeat_interval_secs: 5,
            heartbeat_timeout_secs: 45,
            tunnel_proxy_timeout_secs: 60,
            bind_address: None,
            relay_route: crate::config::RelayRouteMode::Off,
            relay_route_prefer: Vec::new(),
            tls_ca_file: None,
            tls_server_cert_sha256: None,
        }
    }
}

/// `Arc` the TLS config once for a fetcher's life; kept for callers that
/// build their own connector.
pub fn tls_config(tunnel: Option<&TunnelConfig>) -> Result<Arc<rustls::ClientConfig>, String> {
    let plain = TunnelConfig::plain();
    build_tunnel_tls_config(tunnel.unwrap_or(&plain)).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_parse_with_defaults_and_ports() {
        let t = Target::parse("https://relay.example/api/tunnel/artifacts/0.6.7.1/release.json")
            .unwrap();
        assert!(t.tls);
        assert_eq!(t.host, "relay.example");
        assert_eq!(t.port, 443);
        assert_eq!(t.path, "/api/tunnel/artifacts/0.6.7.1/release.json");
        assert_eq!(t.dir(), "/api/tunnel/artifacts/0.6.7.1/");
        assert_eq!(t.with_path("/x"), "https://relay.example/x");
        let t = Target::parse("http://10.0.0.1:8080").unwrap();
        assert!(!t.tls);
        assert_eq!(
            (t.host.as_str(), t.port, t.path.as_str()),
            ("10.0.0.1", 8080, "/")
        );
        assert_eq!(t.with_path("/a/b"), "http://10.0.0.1:8080/a/b");
        assert!(Target::parse("ftp://x/").is_err());
        assert!(Target::parse("https:///x").is_err());
    }

    /// A tiny HTTP/1.1 server that serves `body` with Range support.
    async fn serve(body: Vec<u8>, fail_first_after: Option<usize>) -> std::net::SocketAddr {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body = Arc::new(body);
        let cut = Arc::new(std::sync::Mutex::new(fail_first_after));
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let body = body.clone();
                let cut = cut.clone();
                tokio::spawn(async move {
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    let mut range: Option<u64> = None;
                    loop {
                        line.clear();
                        if reader.read_line(&mut line).await.unwrap_or(0) == 0 || line == "\r\n" {
                            break;
                        }
                        if let Some(r) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                            range = r.trim().trim_end_matches('-').parse().ok();
                        }
                    }
                    let mut stream = reader.into_inner();
                    let (status, from) = match range {
                        Some(f) if f < body.len() as u64 => ("206 Partial Content", f as usize),
                        Some(_) => ("416 Range Not Satisfiable", 0),
                        None => ("200 OK", 0),
                    };
                    let mut slice = body[from..].to_vec();
                    let cut_at = cut.lock().unwrap().take();
                    if let Some(n) = cut_at {
                        slice.truncate(n);
                    }
                    let head = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        if cut_at.is_some() {
                            body.len() - from
                        } else {
                            slice.len()
                        }
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(&slice).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        addr
    }

    #[tokio::test]
    async fn a_download_resumes_where_it_was_cut() {
        let body: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let addr = serve(body.clone(), Some(70_000)).await;
        let dir = std::env::temp_dir().join(format!("sctl-fetch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("artifact");
        let fetcher = Fetcher::new(None, Some("k".into()));
        fetcher
            .get_to_file(
                &format!("http://{addr}/artifact"),
                &dest,
                body.len() as u64,
                None,
            )
            .await
            .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        // A second call with the file complete does not download again.
        fetcher
            .get_to_file(
                &format!("http://{addr}/artifact"),
                &dest,
                body.len() as u64,
                None,
            )
            .await
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_small_get_returns_status_and_body() {
        let addr = serve(b"{\"v\":1}".to_vec(), None).await;
        let fetcher = Fetcher::new(None, None);
        let small = fetcher
            .get_small(&format!("http://{addr}/release.json"), 1024)
            .await
            .unwrap();
        assert_eq!(small.status, 200);
        assert_eq!(small.body, b"{\"v\":1}");
        assert!(fetcher
            .get_small(&format!("http://{addr}/x"), 3)
            .await
            .is_err());
    }
}
