//! `POST /api/fetch` — perform an HTTP(S) request from the device, using sctl's
//! own TLS stack.
//!
//! WHY THIS EXISTS
//!   On the hardware sctl is deployed to, sctl is the only component that can
//!   speak modern TLS. A ZBT WE826-Q-WD ships curl 7.29.0 against OpenSSL 1.0.2n
//!   — 2013 and 2017 respectively — and any current server rejects its
//!   ClientHello outright (`tlsv1 alert protocol version`). It cannot reach the
//!   LAN's own management gear, and it cannot reach our relay either. sctl can,
//!   because it is a static Rust binary carrying rustls and webpki-roots.
//!
//!   So this endpoint is not new privilege — `/api/exec` already runs arbitrary
//!   commands as root, and every `/api/*` route sits behind the same API key.
//!   It is a capability the *shell* lacks, lifted to where the modern TLS lives.
//!
//! TRUST MODEL
//!   LAN management gear is almost always self-signed, so plain CA validation
//!   fails and the only meaningful question is whether the certificate is the
//!   one we were told to expect. Three paths, checked in order:
//!
//!   1. `pin_sha256` in the request — an operator-supplied fingerprint.
//!      Authoritative; a mismatch is a hard error and nothing else is tried.
//!   2. Normal webpki validation — anything with a real CA chain.
//!   3. The pin store — a previously recorded fingerprint for this host:port.
//!      A mismatch is a hard error.
//!   4. `allow_tofu` — record whatever is presented and continue.
//!
//!   TOFU is opt-in, never a default, and the response says so. It proves only
//!   that nothing changed since first contact; if an attacker was already in
//!   position when we first looked, it pins the attacker. That matters here
//!   specifically: these devices sit on networks shared with untrusted hosts —
//!   a coach AP that also serves passenger wifi, for instance.
//!
//!   The response ALWAYS reports the certificate actually presented — SHA-256
//!   plus the raw DER — so a pin can be confirmed out of band rather than taken
//!   on faith. Parsing subject/issuer/validity is deliberately left to the
//!   caller: an X.509 parser is a dependency this binary does not need, and it
//!   is re-downloaded into RAM on every boot.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use base64::Engine as _;
use http_body_util::BodyExt;
use hyper::body::Bytes;
use hyper::Uri;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error as TlsError, RootCertStore, SignatureScheme};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tracing::{info, warn};

use crate::error::{codes, ApiError};
use crate::pin_store::{fingerprint, fingerprints_match, PinOrigin, PinStore};
use crate::AppState;

const DEFAULT_TIMEOUT_MS: u64 = 10_000;
const MAX_TIMEOUT_MS: u64 = 120_000;
const DEFAULT_MAX_BYTES: usize = 1 << 20; // 1 MiB
const MAX_MAX_BYTES: usize = 8 << 20; // 8 MiB

type Resp<T> = Result<T, (StatusCode, Json<ApiError>)>;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct FetchRequest {
    pub url: String,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub headers: Option<HashMap<String, String>>,
    /// Request body. Interpreted as base64 when `body_base64` is set.
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub body_base64: bool,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub max_bytes: Option<usize>,
    /// Operator-supplied certificate fingerprint (hex SHA-256 of the DER).
    #[serde(default)]
    pub pin_sha256: Option<String>,
    /// Record and trust whatever certificate is presented, if nothing else
    /// establishes trust. Off by default; see the trust model above.
    #[serde(default)]
    pub allow_tofu: bool,
    /// Force HTTP/2. Required for cleartext h2c — a gRPC endpoint on the LAN,
    /// such as the Starlink dish's Device API, speaks h2 with no TLS and so
    /// cannot be negotiated via ALPN.
    #[serde(default)]
    pub http2: bool,
}

#[derive(Debug, Serialize)]
pub struct TlsReport {
    /// Hex SHA-256 of the DER end-entity certificate actually presented.
    pub sha256: String,
    /// How trust was established: `ca`, `configured`, `tofu_matched`,
    /// `tofu_recorded`.
    pub trust: &'static str,
    /// Base64 DER of the end-entity certificate, so the caller can parse
    /// subject/issuer/validity without sctl carrying an X.509 parser.
    pub cert_der_base64: String,
    pub alpn: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct FetchResponse {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: String,
    /// True when `body` is base64 because the response was not valid UTF-8.
    pub body_base64: bool,
    pub truncated: bool,
    pub elapsed_ms: u64,
    pub http_version: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls: Option<TlsReport>,
}

// ─── certificate verification ────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Observed {
    sha256: String,
    der: Vec<u8>,
    trust: &'static str,
}

/// Verifier implementing the layered trust model documented above.
#[derive(Debug)]
struct PinningVerifier {
    webpki: Arc<WebPkiServerVerifier>,
    expected: Option<String>,
    allow_tofu: bool,
    store: PinStore,
    host_port: String,
    observed: Arc<Mutex<Option<Observed>>>,
}

impl ServerCertVerifier for PinningVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let der = end_entity.as_ref().to_vec();
        let fp = fingerprint(&der);

        let record = |trust: &'static str| {
            if let Ok(mut slot) = self.observed.lock() {
                *slot = Some(Observed {
                    sha256: fp.clone(),
                    der: der.clone(),
                    trust,
                });
            }
        };

        // 1. An explicit pin is authoritative and exclusive: if the operator
        //    said which certificate to expect, a different one is a failure,
        //    not an invitation to look for some other way to trust it.
        if let Some(expected) = &self.expected {
            if fingerprints_match(expected, &fp) {
                record("configured");
                return Ok(ServerCertVerified::assertion());
            }
            warn!(
                "fetch: {} presented {fp}, expected {expected}",
                self.host_port
            );
            record("rejected");
            return Err(TlsError::General(format!(
                "certificate pin mismatch for {}: presented {fp}",
                self.host_port
            )));
        }

        // 2. A real CA chain needs no pinning.
        if self
            .webpki
            .verify_server_cert(end_entity, intermediates, server_name, ocsp, now)
            .is_ok()
        {
            record("ca");
            return Ok(ServerCertVerified::assertion());
        }

        // 3. A previously recorded pin for this exact host:port.
        if let Some(entry) = self.store.get(&self.host_port) {
            if fingerprints_match(&entry.sha256, &fp) {
                record(match entry.origin {
                    PinOrigin::Configured => "configured",
                    PinOrigin::Tofu => "tofu_matched",
                });
                return Ok(ServerCertVerified::assertion());
            }
            warn!(
                "fetch: {} presented {fp}, pinned {}",
                self.host_port, entry.sha256
            );
            record("rejected");
            return Err(TlsError::General(format!(
                "certificate pin mismatch for {}: presented {fp}, pinned {}",
                self.host_port, entry.sha256
            )));
        }

        // 4. Nothing established trust. Record only if asked.
        if self.allow_tofu {
            self.store
                .insert_if_absent(&self.host_port, &fp, PinOrigin::Tofu);
            info!(
                "fetch: recorded TOFU pin for {} = {fp} (proves no CHANGE, not authenticity)",
                self.host_port
            );
            record("tofu_recorded");
            return Ok(ServerCertVerified::assertion());
        }

        record("rejected");
        Err(TlsError::General(format!(
            "no CA path and no pin for {} (presented {fp}); \
             supply pin_sha256 or set allow_tofu",
            self.host_port
        )))
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.webpki.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.webpki.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.webpki.supported_verify_schemes()
    }
}

// ─── URL handling ────────────────────────────────────────────────────────────

struct Target {
    tls: bool,
    host: String,
    port: u16,
    /// path + query, as sent on the wire.
    path_and_query: String,
}

fn parse_target(raw: &str) -> Result<Target, String> {
    let uri: Uri = raw.parse().map_err(|e| format!("invalid url: {e}"))?;
    let scheme = uri.scheme_str().unwrap_or_default().to_ascii_lowercase();
    let tls = match scheme.as_str() {
        "https" => true,
        "http" => false,
        other => return Err(format!("unsupported scheme '{other}' (http or https only)")),
    };
    let host = uri
        .host()
        .ok_or_else(|| "url has no host".to_string())?
        .trim_matches(['[', ']'])
        .to_string();
    let port = uri.port_u16().unwrap_or(if tls { 443 } else { 80 });
    let path_and_query = uri
        .path_and_query()
        .map_or_else(|| "/".to_string(), |p| p.as_str().to_string());
    Ok(Target {
        tls,
        host,
        port,
        path_and_query,
    })
}

// ─── handler ─────────────────────────────────────────────────────────────────

fn bad(
    code: &'static str,
    msg: impl Into<String>,
    status: StatusCode,
) -> (StatusCode, Json<ApiError>) {
    ApiError::new(code, msg).into_response_with(status)
}

/// Why an in-process fetch did not produce a response.
///
/// The variants mirror the API error codes the route hands out, so a caller
/// inside the binary (the infra monitor's `http_api` check) can act on a pin
/// mismatch without parsing the message the route would have built from it.
#[derive(Debug)]
pub enum FetchError {
    /// The request itself was malformed (URL, method, body encoding).
    Invalid(String),
    /// No response within the request's timeout.
    Timeout(u64),
    /// The certificate presented is not the one pinned. `presented` is the
    /// hex SHA-256 of the DER actually offered, for re-pinning by a human.
    PinMismatch {
        presented: Option<String>,
        message: String,
    },
    /// No CA path, no pin, and TOFU not allowed.
    Untrusted {
        presented: Option<String>,
        message: String,
    },
    /// Everything else: connect, TLS, protocol.
    Failed(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(m) | Self::Failed(m) => write!(f, "{m}"),
            Self::Timeout(ms) => write!(f, "fetch timed out after {ms}ms"),
            Self::PinMismatch { message, .. } | Self::Untrusted { message, .. } => {
                write!(f, "{message}")
            }
        }
    }
}

impl FetchError {
    fn api(&self) -> (StatusCode, Json<ApiError>) {
        match self {
            Self::Invalid(m) => bad(codes::INVALID_REQUEST, m.clone(), StatusCode::BAD_REQUEST),
            Self::Timeout(_) => bad(
                codes::TIMEOUT,
                self.to_string(),
                StatusCode::GATEWAY_TIMEOUT,
            ),
            Self::PinMismatch { message, .. } => bad(
                codes::CERT_PIN_MISMATCH,
                message.clone(),
                StatusCode::BAD_GATEWAY,
            ),
            Self::Untrusted { message, .. } => bad(
                codes::CERT_UNTRUSTED,
                message.clone(),
                StatusCode::BAD_GATEWAY,
            ),
            Self::Failed(m) => bad(codes::FETCH_FAILED, m.clone(), StatusCode::BAD_GATEWAY),
        }
    }
}

/// Perform a fetch from inside the binary, with the same trust ladder the
/// route applies. `data_dir` locates the pin store.
pub(crate) async fn execute(
    data_dir: &str,
    req: &FetchRequest,
) -> Result<FetchResponse, FetchError> {
    let started = Instant::now();

    let target = parse_target(&req.url).map_err(FetchError::Invalid)?;

    let timeout = Duration::from_millis(
        req.timeout_ms
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .min(MAX_TIMEOUT_MS),
    );
    let max_bytes = req
        .max_bytes
        .unwrap_or(DEFAULT_MAX_BYTES)
        .min(MAX_MAX_BYTES);

    let method = req.method.as_deref().unwrap_or("GET").to_ascii_uppercase();
    let method: hyper::Method = method
        .parse()
        .map_err(|_| FetchError::Invalid(format!("invalid method '{method}'")))?;

    let body_bytes = match (&req.body, req.body_base64) {
        (None, _) => Bytes::new(),
        (Some(b), false) => Bytes::from(b.clone().into_bytes()),
        (Some(b), true) => Bytes::from(
            base64::engine::general_purpose::STANDARD
                .decode(b)
                .map_err(|e| {
                    FetchError::Invalid(format!("body_base64 is not valid base64: {e}"))
                })?,
        ),
    };

    let observed = Arc::new(Mutex::new(None));
    let outcome = tokio::time::timeout(
        timeout,
        perform(
            data_dir,
            &target,
            req,
            method,
            body_bytes,
            max_bytes,
            Arc::clone(&observed),
        ),
    )
    .await;

    let presented = || {
        observed
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|o| o.sha256.clone()))
    };

    let (status, headers, body, truncated, http_version, alpn) = match outcome {
        Err(_) => return Err(FetchError::Timeout(timeout.as_millis() as u64)),
        Ok(Err(e)) => {
            // rustls stringifies a custom verifier rejection as
            // "unexpected error: <ours>". These rejections are the opposite of
            // unexpected -- they are the feature working -- and the prefix makes
            // an operator read a deliberate refusal as an internal fault.
            let raw = e.to_string();
            let message = raw
                .strip_prefix("unexpected error: ")
                .map_or_else(|| raw.clone(), str::to_string);
            // Distinguish interception from an ordinary failure: a pin mismatch
            // must never look like a transient error a caller would retry past.
            return Err(if message.contains("pin mismatch") {
                FetchError::PinMismatch {
                    presented: presented(),
                    message,
                }
            } else if message.contains("no CA path and no pin") {
                FetchError::Untrusted {
                    presented: presented(),
                    message,
                }
            } else {
                FetchError::Failed(message)
            });
        }
        Ok(Ok(v)) => v,
    };

    let (body_str, body_base64) = match String::from_utf8(body.to_vec()) {
        Ok(s) => (s, false),
        Err(e) => (
            base64::engine::general_purpose::STANDARD.encode(e.as_bytes()),
            true,
        ),
    };

    let tls = observed
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .map(|o| TlsReport {
            sha256: o.sha256,
            trust: o.trust,
            cert_der_base64: base64::engine::general_purpose::STANDARD.encode(&o.der),
            alpn: alpn.clone(),
        });

    Ok(FetchResponse {
        status,
        headers,
        body: body_str,
        body_base64,
        truncated,
        elapsed_ms: started.elapsed().as_millis() as u64,
        http_version,
        tls,
    })
}

/// `POST /api/fetch`
pub async fn fetch(
    State(state): State<AppState>,
    Json(req): Json<FetchRequest>,
) -> Resp<Json<FetchResponse>> {
    execute(&state.config.server.data_dir, &req)
        .await
        .map(Json)
        .map_err(|e| e.api())
}

type PerformOk = (
    u16,
    HashMap<String, String>,
    Bytes,
    bool,
    &'static str,
    Option<String>,
);

async fn perform(
    data_dir: &str,
    target: &Target,
    req: &FetchRequest,
    method: hyper::Method,
    body: Bytes,
    max_bytes: usize,
    observed: Arc<Mutex<Option<Observed>>>,
) -> Result<PerformOk, Box<dyn std::error::Error + Send + Sync>> {
    let tcp = TcpStream::connect((target.host.as_str(), target.port)).await?;
    tcp.set_nodelay(true).ok();

    let mut request = hyper::Request::builder()
        .method(method)
        .uri(&target.path_and_query)
        .header(hyper::header::HOST, host_header(target));
    if let Some(hs) = &req.headers {
        for (k, v) in hs {
            request = request.header(k.as_str(), v.as_str());
        }
    }
    let request = request.body(http_body_util::Full::new(body))?;

    if target.tls {
        let host_port = host_header(target);
        let mut root_store = RootCertStore::empty();
        root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let webpki = WebPkiServerVerifier::builder(Arc::new(root_store)).build()?;

        let verifier = Arc::new(PinningVerifier {
            webpki,
            expected: req.pin_sha256.clone(),
            allow_tofu: req.allow_tofu,
            store: PinStore::new(data_dir),
            host_port,
            observed,
        });

        let mut cfg = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        // Offer h2 so a modern server can pick it; http/1.1 stays available.
        cfg.alpn_protocols = if req.http2 {
            vec![b"h2".to_vec()]
        } else {
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        };

        let server_name = ServerName::try_from(target.host.clone())?;
        let stream = tokio_rustls::TlsConnector::from(Arc::new(cfg))
            .connect(server_name, tcp)
            .await?;
        let alpn = stream
            .get_ref()
            .1
            .alpn_protocol()
            .map(|p| String::from_utf8_lossy(p).into_owned());
        let use_h2 = req.http2 || alpn.as_deref() == Some("h2");
        let (status, headers, body, truncated, version) =
            send(stream, request, max_bytes, use_h2).await?;
        Ok((status, headers, body, truncated, version, alpn))
    } else {
        // Cleartext. h2 here is prior-knowledge h2c — there is no ALPN and no
        // upgrade dance, which is exactly how LAN gRPC endpoints are reached.
        let (status, headers, body, truncated, version) =
            send(tcp, request, max_bytes, req.http2).await?;
        Ok((status, headers, body, truncated, version, None))
    }
}

fn host_header(t: &Target) -> String {
    let default_port = if t.tls { 443 } else { 80 };
    if t.port == default_port {
        t.host.clone()
    } else {
        format!("{}:{}", t.host, t.port)
    }
}

async fn send<S>(
    stream: S,
    request: hyper::Request<http_body_util::Full<Bytes>>,
    max_bytes: usize,
    use_h2: bool,
) -> Result<
    (u16, HashMap<String, String>, Bytes, bool, &'static str),
    Box<dyn std::error::Error + Send + Sync>,
>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let io = hyper_util::rt::TokioIo::new(stream);

    let response = if use_h2 {
        let (mut sender, conn) =
            hyper::client::conn::http2::handshake(hyper_util::rt::TokioExecutor::new(), io).await?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        sender.send_request(request).await?
    } else {
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        sender.send_request(request).await?
    };

    let version = if use_h2 { "HTTP/2" } else { "HTTP/1.1" };
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect();

    // Read frame by frame so an oversized or endless body is bounded. Collecting
    // first and truncating afterwards would defeat the point on a device with
    // 128 MB of RAM.
    let mut body = response.into_body();
    let mut buf: Vec<u8> = Vec::new();
    let mut truncated = false;
    while let Some(frame) = body.frame().await {
        let frame = frame?;
        if let Some(chunk) = frame.data_ref() {
            let room = max_bytes.saturating_sub(buf.len());
            if chunk.len() > room {
                buf.extend_from_slice(&chunk[..room]);
                truncated = true;
                break;
            }
            buf.extend_from_slice(chunk);
        }
    }

    Ok((status, headers, Bytes::from(buf), truncated, version))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scheme_host_port_and_path() {
        let t = parse_target("https://192.168.50.1/api/status.wan").unwrap();
        assert!(t.tls);
        assert_eq!(t.host, "192.168.50.1");
        assert_eq!(t.port, 443, "https defaults to 443");
        assert_eq!(t.path_and_query, "/api/status.wan");

        let t = parse_target("http://192.168.100.1:9200/SpaceX.API.Device.Device/Handle").unwrap();
        assert!(!t.tls);
        assert_eq!(t.port, 9200);
        assert_eq!(t.path_and_query, "/SpaceX.API.Device.Device/Handle");
    }

    #[test]
    fn defaults_and_query_strings_survive() {
        let t = parse_target("http://example.test").unwrap();
        assert_eq!(t.port, 80);
        assert_eq!(t.path_and_query, "/", "empty path becomes /");
        let t = parse_target("https://h/a/b?x=1&y=2").unwrap();
        assert_eq!(t.path_and_query, "/a/b?x=1&y=2");
    }

    #[test]
    fn rejects_non_http_schemes_and_hostless_urls() {
        for bad in ["ftp://h/x", "file:///etc/passwd", "gopher://h"] {
            assert!(parse_target(bad).is_err(), "{bad} must be rejected");
        }
        assert!(parse_target("/just/a/path").is_err());
        assert!(parse_target("not a url at all").is_err());
    }

    #[test]
    fn host_header_omits_the_default_port_only() {
        let t = parse_target("https://h").unwrap();
        assert_eq!(host_header(&t), "h");
        let t = parse_target("https://h:8443").unwrap();
        assert_eq!(host_header(&t), "h:8443");
        let t = parse_target("http://h:80").unwrap();
        assert_eq!(host_header(&t), "h");
        let t = parse_target("http://h:8080").unwrap();
        assert_eq!(host_header(&t), "h:8080");
    }

    #[test]
    fn ipv6_literals_lose_their_brackets_for_connect() {
        let t = parse_target("http://[::1]:9200/x").unwrap();
        assert_eq!(
            t.host, "::1",
            "brackets are URL syntax, not part of the host"
        );
        assert_eq!(t.port, 9200);
    }
}
