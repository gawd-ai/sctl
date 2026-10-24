//! `sctl net http`: one HTTP(S) request through the agent's own TLS stack
//! (`routes::fetch`, its pin and TOFU trust ladder), plus what a login flow on
//! LAN gear needs and a single fetch does not: a cookie jar kept in a file
//! between steps, and redirects followed (ADR-006).
//!
//! Every hop passes the guard on its own (ADR-012 decision 6), `--dev` and
//! `--expect-mac` included, its host looked up once, and connects bound
//! where the guard approved, to the address it judged; the URL's name stays
//! for `Host` and TLS. A redirect to another scheme, host or port drops the
//! Basic credentials, every header the caller set and the body, and is not
//! followed when it would send the body again (a 307 or 308 after a request
//! with one, or a 301 or 302 that keeps a PUT's), nor, with `--expect-mac`,
//! to another address. A redirect not followed ends the request: the answer
//! is that redirect, with `not_followed`.

use std::collections::HashMap;
use std::net::Ipv4Addr;

use base64::Engine as _;
use serde::Serialize;

use super::guard::{Approved, Code, Guard, Refusal, Stop};
use crate::routes::fetch::{execute, FetchError, FetchRequest, FetchResponse};

pub const DEFAULT_MAX_REDIRECTS: usize = 5;

#[derive(Debug, Default, Clone)]
pub struct HttpRequest {
    pub url: String,
    pub method: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    /// `user:password` for Basic auth.
    pub basic: Option<String>,
    pub cookie_jar: Option<String>,
    pub max_redirects: usize,
    pub timeout_ms: Option<u64>,
    pub max_bytes: Option<usize>,
    pub pin_sha256: Option<String>,
    pub allow_tofu: bool,
}

#[derive(Debug, Serialize)]
pub struct HttpOutcome {
    pub status: u16,
    /// The URL that answered, after redirects.
    pub url: String,
    pub redirects: Vec<String>,
    pub headers: HashMap<String, String>,
    pub body: String,
    pub body_base64: bool,
    pub truncated: bool,
    pub elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls_trust: Option<&'static str>,
    /// The answer is a redirect that was not followed, and why.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_followed: Option<NotFollowed>,
}

/// Why a redirect was not followed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NotFollowed {
    pub reason: String,
    /// The guard's code, when the redirect's hop was refused.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refused: Option<Code>,
}

/// Why `request` produced no answer.
#[derive(Debug)]
pub enum HttpError {
    /// The guard refused the first request: nothing was sent (exit 3).
    Refused(Box<Refusal>),
    /// The first request failed (exit 1).
    Failed(String),
}

/// `application/x-www-form-urlencoded` for `k=v` pairs.
pub fn form_body(pairs: &[(String, String)]) -> String {
    fn enc(s: &str) -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (b as char).to_string()
                }
                b' ' => "+".to_string(),
                _ => format!("%{b:02X}"),
            })
            .collect()
    }
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
        .collect::<Vec<_>>()
        .join("&")
}

// ─── cookie jar ──────────────────────────────────────────────────────────────

/// A cookie jar file: one `host<TAB>name<TAB>value` per line. Domain, path
/// and expiry are not modelled: a jar serves one device's login for the
/// length of a runbook.
#[derive(Debug, Default, PartialEq)]
pub struct Jar {
    cookies: Vec<(String, String, String)>,
}

impl Jar {
    pub fn load(path: &str) -> Self {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let cookies = text
            .lines()
            .filter_map(|l| {
                let mut f = l.splitn(3, '\t');
                Some((
                    f.next()?.to_string(),
                    f.next()?.to_string(),
                    f.next()?.to_string(),
                ))
            })
            .collect();
        Self { cookies }
    }

    pub fn save(&self, path: &str) -> std::io::Result<()> {
        let mut text = String::new();
        for (h, n, v) in &self.cookies {
            text.push_str(h);
            text.push('\t');
            text.push_str(n);
            text.push('\t');
            text.push_str(v);
            text.push('\n');
        }
        std::fs::write(path, text)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    pub fn header_for(&self, host: &str) -> Option<String> {
        let pairs: Vec<String> = self
            .cookies
            .iter()
            .filter(|(h, _, _)| h == host)
            .map(|(_, n, v)| format!("{n}={v}"))
            .collect();
        (!pairs.is_empty()).then(|| pairs.join("; "))
    }

    /// Fold one `Set-Cookie` value in; an expired or emptied cookie leaves.
    pub fn store(&mut self, host: &str, set_cookie: &str) {
        let mut parts = set_cookie.split(';');
        let Some((name, value)) = parts.next().and_then(|nv| nv.split_once('=')) else {
            return;
        };
        let (name, value) = (name.trim().to_string(), value.trim().to_string());
        let gone = value.is_empty()
            || parts.any(|a| {
                let a = a.trim().to_ascii_lowercase();
                a == "max-age=0" || a.starts_with("max-age=-")
            });
        self.cookies.retain(|(h, n, _)| !(h == host && *n == name));
        if !gone {
            self.cookies.push((host.to_string(), name, value));
        }
    }
}

/// The host of `url`, without an IPv6 literal's brackets; empty when it
/// has none.
pub fn host_of(url: &str) -> String {
    url.parse::<hyper::Uri>()
        .ok()
        .and_then(|u| u.host().map(|h| h.trim_matches(['[', ']']).to_string()))
        .unwrap_or_default()
}

/// A URL's origin: scheme, host (in lower case) and port.
fn origin_of(url: &str) -> Option<(String, String, u16)> {
    let uri = url.parse::<hyper::Uri>().ok()?;
    let scheme = uri.scheme_str()?.to_ascii_lowercase();
    let host = uri.host()?.to_ascii_lowercase();
    let port = uri
        .port_u16()
        .unwrap_or(if scheme == "https" { 443 } else { 80 });
    Some((scheme, host, port))
}

/// What a redirect does to the next request (ADR-012 decision 6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Redirect {
    Follow {
        method: String,
        /// The body goes again.
        keep_body: bool,
        /// The Basic credentials and the caller's headers go again.
        keep_caller: bool,
    },
    NotFollowed(String),
}

/// The redirect rules: 303, and 301 or 302 after a POST, go on as a GET
/// without a body; any other keeps the method and the body. To another
/// origin the credentials, the caller's headers and the body stay behind,
/// and a redirect that would send the body again (a 307 or 308 after a
/// request with one, or a 301 or 302 that keeps a PUT's) is not followed.
pub(crate) fn redirect(
    status: u16,
    method: &str,
    has_body: bool,
    from: &str,
    to: &str,
) -> Redirect {
    let to_get = status == 303 || (matches!(status, 301 | 302) && method == "POST");
    let (method, keep_body) = if to_get {
        ("GET".to_string(), false)
    } else {
        (method.to_string(), has_body)
    };
    let same_origin = origin_of(from).is_some() && origin_of(from) == origin_of(to);
    if same_origin {
        return Redirect::Follow {
            method,
            keep_body,
            keep_caller: true,
        };
    }
    if keep_body {
        return Redirect::NotFollowed(format!(
            "a {status} to another origin would send the body again"
        ));
    }
    Redirect::Follow {
        method,
        keep_body: false,
        keep_caller: false,
    }
}

/// `location` resolved against `base` (absolute, scheme-relative or path).
pub fn resolve(base: &str, location: &str) -> String {
    if location.starts_with("http://") || location.starts_with("https://") {
        return location.to_string();
    }
    let Ok(uri) = base.parse::<hyper::Uri>() else {
        return location.to_string();
    };
    let scheme = uri.scheme_str().unwrap_or("http");
    let authority = uri
        .authority()
        .map(|a| a.as_str().to_string())
        .unwrap_or_default();
    if let Some(rest) = location.strip_prefix("//") {
        return format!("{scheme}://{rest}");
    }
    if location.starts_with('/') {
        return format!("{scheme}://{authority}{location}");
    }
    let path = uri.path();
    let dir = &path[..path.rfind('/').map_or(0, |i| i + 1)];
    format!(
        "{scheme}://{authority}{}{location}",
        if dir.is_empty() { "/" } else { dir }
    )
}

/// Only an http or https URL with a host is sent.
fn sendable(url: &str) -> Result<(), Stop> {
    match origin_of(url) {
        Some((scheme, _, _)) if scheme == "http" || scheme == "https" => Ok(()),
        Some(_) => Err(Stop::Failed("only http and https URLs are sent".into())),
        None => Err(Stop::Failed("not a URL with a host".into())),
    }
}

/// The first request's approval: the guard's on its host, looked up once.
async fn approve_hop(guard: &Guard, url: &str) -> Result<Approved, Stop> {
    sendable(url)?;
    guard.approve_host(&host_of(url)).await
}

/// Why a hop after the first was not sent, as `not_followed` says it.
fn not_followed(stop: Stop) -> NotFollowed {
    match stop {
        Stop::Refused(refusal) => NotFollowed {
            reason: refusal.reason,
            refused: Some(refusal.refused),
        },
        Stop::Failed(reason) => NotFollowed {
            reason,
            refused: None,
        },
    }
}

/// The answer: the response that ended the request.
fn outcome(
    url: String,
    resp: FetchResponse,
    redirects: Vec<String>,
    not_followed: Option<NotFollowed>,
) -> HttpOutcome {
    HttpOutcome {
        status: resp.status,
        url,
        redirects,
        tls_sha256: resp.tls.as_ref().map(|t| t.sha256.clone()),
        tls_trust: resp.tls.as_ref().map(|t| t.trust),
        headers: resp.headers,
        body: resp.body,
        body_base64: resp.body_base64,
        truncated: resp.truncated,
        elapsed_ms: resp.elapsed_ms,
        not_followed,
    }
}

/// Send the request, following redirects; every hop passes `guard`.
pub async fn request(
    state_dir: &str,
    req: &HttpRequest,
    guard: &Guard,
) -> Result<HttpOutcome, HttpError> {
    let approved = match approve_hop(guard, &req.url).await {
        Ok(approved) => approved,
        Err(Stop::Refused(refusal)) => return Err(HttpError::Refused(refusal)),
        Err(Stop::Failed(reason)) => return Err(HttpError::Failed(reason)),
    };
    follow(state_dir, req, guard, approved).await
}

/// Send the request from where the guard approved its first hop, and
/// follow its redirects, each hop approved by `guard`.
async fn follow(
    state_dir: &str,
    req: &HttpRequest,
    guard: &Guard,
    mut approved: Approved,
) -> Result<HttpOutcome, HttpError> {
    let mut jar = req.cookie_jar.as_deref().map(Jar::load).unwrap_or_default();
    let save_jar = |jar: &Jar| match &req.cookie_jar {
        Some(path) => jar
            .save(path)
            .map_err(|e| HttpError::Failed(format!("cookie jar {path}: {e}"))),
        None => Ok(()),
    };
    let mut url = req.url.clone();
    let mut method = if req.method.is_empty() {
        "GET".to_string()
    } else {
        req.method.to_ascii_uppercase()
    };
    let mut body = req.body.clone();
    let mut keep_caller = true;
    let mut redirects = Vec::new();
    let first_address: Ipv4Addr = approved.address;
    // The redirect being followed, kept to answer with if its hop is not
    // sent after all.
    let mut following: Option<(String, FetchResponse)> = None;
    loop {
        let host = host_of(&url);
        let mut headers: HashMap<String, String> = HashMap::new();
        if keep_caller {
            headers.extend(req.headers.iter().cloned());
            if let Some(basic) = &req.basic {
                let token = base64::engine::general_purpose::STANDARD.encode(basic.as_bytes());
                headers.insert("authorization".into(), format!("Basic {token}"));
            }
        }
        if let Some(cookie) = jar.header_for(&host) {
            headers.insert("cookie".into(), cookie);
        }
        let fetch = FetchRequest {
            url: url.clone(),
            method: Some(method.clone()),
            headers: Some(headers),
            body: body.clone(),
            timeout_ms: req.timeout_ms,
            max_bytes: req.max_bytes,
            pin_sha256: req.pin_sha256.clone(),
            allow_tofu: req.allow_tofu,
            approved: Some(approved.clone()),
            ..FetchRequest::default()
        };
        let resp = match execute(state_dir, &fetch).await {
            Ok(resp) => resp,
            Err(FetchError::Unsent(reason)) => {
                let Some((redirect_url, redirect_resp)) = following.take() else {
                    // The first request: refused, with what the guard read.
                    return Err(HttpError::Refused(
                        approved.refusal(Code::GuardFailed, reason),
                    ));
                };
                redirects.pop();
                save_jar(&jar)?;
                return Ok(outcome(
                    redirect_url,
                    redirect_resp,
                    redirects,
                    Some(NotFollowed {
                        reason,
                        refused: Some(Code::GuardFailed),
                    }),
                ));
            }
            Err(e) => return Err(HttpError::Failed(e.to_string())),
        };
        for c in &resp.set_cookies {
            jar.store(&host, c);
        }
        let location = resp.headers.get("location").cloned();
        let is_redirect = matches!(resp.status, 301 | 302 | 303 | 307 | 308);
        let mut stopped: Option<NotFollowed> = None;
        if is_redirect && redirects.len() < req.max_redirects {
            if let Some(loc) = location {
                let next = resolve(&url, &loc);
                match redirect(resp.status, &method, body.is_some(), &url, &next) {
                    Redirect::NotFollowed(reason) => {
                        stopped = Some(NotFollowed {
                            reason,
                            refused: None,
                        });
                    }
                    Redirect::Follow {
                        method: next_method,
                        keep_body,
                        keep_caller: keep,
                    } => match approve_next(guard, &next, first_address).await {
                        Ok(next_approved) => {
                            method = next_method;
                            if !keep_body {
                                body = None;
                            }
                            keep_caller &= keep;
                            redirects.push(next.clone());
                            following = Some((std::mem::replace(&mut url, next), resp));
                            approved = next_approved;
                            continue;
                        }
                        Err(why) => stopped = Some(why),
                    },
                }
            }
        }
        save_jar(&jar)?;
        return Ok(outcome(url, resp, redirects, stopped));
    }
}

/// The next hop's approval, its host looked up once (ADR-012 decision 1).
/// With `--expect-mac` that address must be the one the first request was
/// judged at, since the MAC is that device's; it is then the address the
/// guard judges and the hop is sent to.
async fn approve_next(guard: &Guard, next: &str, first: Ipv4Addr) -> Result<Approved, NotFollowed> {
    sendable(next).map_err(not_followed)?;
    let (address, name) = guard.resolve(&host_of(next)).await.map_err(not_followed)?;
    if guard.expect_mac.is_some() && address != first {
        return Err(NotFollowed {
            reason: format!(
                "the redirect leads to {address}, not {first}: --expect-mac names the device at {first}"
            ),
            refused: None,
        });
    }
    guard
        .approve_resolved(address, name)
        .await
        .map_err(not_followed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forms_encode() {
        let body = form_body(&[
            ("user".into(), "cyber".into()),
            ("pass word".into(), "a&b=c d".into()),
        ]);
        assert_eq!(body, "user=cyber&pass+word=a%26b%3Dc+d");
    }

    #[test]
    fn the_jar_keeps_replaces_and_forgets() {
        let mut jar = Jar::default();
        jar.store("192.168.1.50", "SESSION=abc; Path=/; HttpOnly");
        jar.store("192.168.1.50", "lang=en");
        jar.store("10.0.0.1", "SESSION=other");
        assert_eq!(
            jar.header_for("192.168.1.50").as_deref(),
            Some("SESSION=abc; lang=en")
        );
        jar.store("192.168.1.50", "SESSION=def");
        jar.store("192.168.1.50", "lang=; Max-Age=0");
        assert_eq!(
            jar.header_for("192.168.1.50").as_deref(),
            Some("SESSION=def")
        );
        let dir = std::env::temp_dir().join(format!("sctl-jar-{}", std::process::id()));
        let path = dir.to_string_lossy().to_string();
        jar.save(&path).unwrap();
        assert_eq!(Jar::load(&path), jar);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn redirects_resolve() {
        assert_eq!(
            resolve("http://192.168.1.50/login.htm", "/index.htm"),
            "http://192.168.1.50/index.htm"
        );
        assert_eq!(
            resolve("http://192.168.1.50/a/login.htm", "next.htm"),
            "http://192.168.1.50/a/next.htm"
        );
        assert_eq!(
            resolve("http://192.168.1.50:8080/x", "https://other/y"),
            "https://other/y"
        );
        assert_eq!(
            resolve("https://192.168.1.50/x", "//10.0.0.1/z"),
            "https://10.0.0.1/z"
        );
    }

    /// A login that sets a cookie and redirects, then a page that needs it.
    #[tokio::test]
    async fn a_login_flow_keeps_its_cookie_across_the_redirect() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut seen = Vec::new();
            for _ in 0..2 {
                let (mut s, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 4096];
                let n = s.read(&mut buf).await.unwrap();
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let reply = if req.starts_with("POST /login") {
                    "HTTP/1.1 302 Found\r\nLocation: /home\r\nSet-Cookie: SID=s3ss10n; Path=/\r\nSet-Cookie: lang=en\r\nContent-Length: 0\r\n\r\n".to_string()
                } else if req
                    .to_ascii_lowercase()
                    .contains("cookie: sid=s3ss10n; lang=en")
                {
                    "HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\nwelcome".to_string()
                } else {
                    "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n".to_string()
                };
                s.write_all(reply.as_bytes()).await.unwrap();
                seen.push(req);
            }
            seen
        });
        let jar = std::env::temp_dir().join(format!("sctl-jar-flow-{}", std::process::id()));
        let req = HttpRequest {
            url: format!("http://{addr}/login"),
            method: "POST".into(),
            headers: vec![(
                "content-type".into(),
                "application/x-www-form-urlencoded".into(),
            )],
            body: Some(form_body(&[
                ("user".into(), "u".into()),
                ("pass".into(), "p".into()),
            ])),
            cookie_jar: Some(jar.to_string_lossy().to_string()),
            max_redirects: DEFAULT_MAX_REDIRECTS,
            timeout_ms: Some(3000),
            ..HttpRequest::default()
        };
        let out = request("/tmp", &req, &Guard::default()).await.unwrap();
        assert_eq!(out.status, 200);
        assert_eq!(out.body, "welcome");
        assert_eq!(out.redirects, vec![format!("http://{addr}/home")]);
        let seen = server.await.unwrap();
        assert!(
            seen[1].starts_with("GET /home"),
            "a 302 after a POST continues as a GET: {}",
            seen[1]
        );
        assert!(std::fs::read_to_string(&jar)
            .unwrap()
            .contains("SID\ts3ss10n"));
        std::fs::remove_file(&jar).ok();
    }

    #[test]
    fn redirect_rules_keep_credentials_with_their_origin() {
        let follow = |method: &str, keep_body: bool, keep_caller: bool| Redirect::Follow {
            method: method.into(),
            keep_body,
            keep_caller,
        };
        let a = "http://192.0.2.10/login";
        // Same origin: as before.
        assert_eq!(
            redirect(302, "POST", true, a, "http://192.0.2.10/home"),
            follow("GET", false, true)
        );
        assert_eq!(
            redirect(303, "PUT", true, a, "http://192.0.2.10/x"),
            follow("GET", false, true)
        );
        assert_eq!(
            redirect(307, "POST", true, a, "http://192.0.2.10/x"),
            follow("POST", true, true)
        );
        assert_eq!(
            redirect(301, "GET", false, a, "http://192.0.2.10/x"),
            follow("GET", false, true)
        );
        assert_eq!(
            redirect(302, "GET", false, a, "http://192.0.2.10:80/x"),
            follow("GET", false, true),
            "the default port is the same origin"
        );
        assert_eq!(
            redirect(
                302,
                "GET",
                false,
                "HTTP://Router.Local/a",
                "http://router.local/b"
            ),
            follow("GET", false, true),
            "scheme and host compare without case"
        );
        // Another host, scheme or port: the caller's part stays behind.
        for to in [
            "http://192.0.2.11/home",
            "https://192.0.2.10/home",
            "http://192.0.2.10:8080/home",
        ] {
            assert_eq!(
                redirect(302, "POST", true, a, to),
                follow("GET", false, false),
                "{to}"
            );
            assert_eq!(
                redirect(307, "GET", false, a, to),
                follow("GET", false, false),
                "{to}"
            );
            assert!(
                matches!(redirect(307, "POST", true, a, to), Redirect::NotFollowed(_)),
                "{to}"
            );
            assert!(matches!(
                redirect(308, "PUT", true, a, to),
                Redirect::NotFollowed(_)
            ));
        }
        // A 301 or 302 keeps a PUT's method and so its body: to another
        // origin that would send the body again, so it is not followed.
        assert!(matches!(
            redirect(301, "PUT", true, a, "http://192.0.2.11/x"),
            Redirect::NotFollowed(_)
        ));
        assert_eq!(
            redirect(302, "PUT", false, a, "http://192.0.2.11/x"),
            follow("PUT", false, false)
        );
    }

    /// A server on `addr` that answers every request with `reply`, and
    /// hands back each request it read, head and body.
    async fn server(
        addr: &str,
        reply: String,
    ) -> (
        std::net::SocketAddr,
        tokio::sync::mpsc::UnboundedReceiver<String>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let local = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                let mut buf = vec![0u8; 8192];
                let mut seen = Vec::new();
                // The head, then as much body as Content-Length says.
                while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
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
                let _ = tx.send(String::from_utf8_lossy(&seen).to_string());
                let _ = s.write_all(reply.as_bytes()).await;
            }
        });
        (local, rx)
    }

    #[tokio::test]
    async fn a_redirect_to_another_host_leaves_the_credentials_headers_and_body_behind() {
        // Another host on loopback: 127.0.0.2 is the unit's own too.
        let (other, mut at_other) = server(
            "127.0.0.2:0",
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".into(),
        )
        .await;
        let (first, mut at_first) = server(
            "127.0.0.1:0",
            format!(
                "HTTP/1.1 302 Found\r\nLocation: http://{other}/next\r\nContent-Length: 0\r\n\r\n"
            ),
        )
        .await;
        let req = HttpRequest {
            url: format!("http://{first}/login"),
            method: "POST".into(),
            headers: vec![("x-custom".into(), "kept-at-home".into())],
            body: Some("user=u&pass=p4ss".into()),
            basic: Some("admin:p4ssw0rd".into()),
            max_redirects: DEFAULT_MAX_REDIRECTS,
            timeout_ms: Some(3000),
            ..HttpRequest::default()
        };
        let out = request("/tmp", &req, &Guard::default()).await.unwrap();
        assert_eq!(out.status, 200);
        assert!(out.not_followed.is_none());
        let sent_first = at_first.recv().await.unwrap();
        assert!(
            sent_first
                .to_ascii_lowercase()
                .contains("authorization: basic"),
            "{sent_first}"
        );
        assert!(
            sent_first.contains("p4ss"),
            "the first request carries its body"
        );
        let sent_other = at_other.recv().await.unwrap();
        assert!(sent_other.starts_with("GET /next"), "{sent_other}");
        let lower = sent_other.to_ascii_lowercase();
        assert!(!lower.contains("authorization"), "{sent_other}");
        assert!(!lower.contains("x-custom"), "{sent_other}");
        assert!(!sent_other.contains("p4ss"), "{sent_other}");
    }

    #[tokio::test]
    async fn a_307_to_another_host_after_a_form_is_not_followed() {
        let (other, mut at_other) = server(
            "127.0.0.2:0",
            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".into(),
        )
        .await;
        let (first, _at_first) = server(
            "127.0.0.1:0",
            format!("HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{other}/next\r\nContent-Length: 0\r\n\r\n"),
        )
        .await;
        let req = HttpRequest {
            url: format!("http://{first}/login"),
            method: "POST".into(),
            headers: vec![(
                "content-type".into(),
                "application/x-www-form-urlencoded".into(),
            )],
            body: Some(form_body(&[("pass".into(), "p4ss".into())])),
            max_redirects: DEFAULT_MAX_REDIRECTS,
            timeout_ms: Some(3000),
            ..HttpRequest::default()
        };
        let out = request("/tmp", &req, &Guard::default()).await.unwrap();
        assert_eq!(out.status, 307, "the answer is the redirect");
        assert_eq!(out.url, format!("http://{first}/login"));
        assert!(out.redirects.is_empty());
        let why = out.not_followed.expect("not followed");
        assert!(
            why.reason.contains("would send the body again"),
            "{}",
            why.reason
        );
        assert_eq!(why.refused, None);
        assert_eq!(
            out.headers.get("location").map(String::as_str),
            Some(format!("http://{other}/next").as_str())
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), at_other.recv())
                .await
                .is_err(),
            "the other host received nothing"
        );
    }

    /// ADR-012 decisions 1 and 6: a hop's host is looked up once, and with
    /// `--expect-mac` the address compared with the first request's is the
    /// address judged and sent to. The name here answers the first
    /// request's address, then another: the hop is judged at the first
    /// answer (the unit's own, so `--expect-mac` refuses it there), and the
    /// second is never asked for.
    #[tokio::test]
    async fn a_redirect_s_host_is_looked_up_once_and_judged_where_it_was_compared() {
        use crate::net_tools::guard::{steer, Mac};
        let first = Ipv4Addr::LOCALHOST;
        let other = Ipv4Addr::new(127, 0, 0, 2);
        let guard = Guard {
            expect_mac: Some(Mac([0x02, 0, 0, 0, 0, 0x0a])),
            ..Guard::default()
        };
        let next = "http://rotating.test:8080/next";

        steer::set("rotating.test", &[first, other]);
        let why = approve_next(&guard, next, first).await.unwrap_err();
        assert_eq!(why.refused, Some(Code::OwnAddress), "{}", why.reason);
        assert!(
            why.reason
                .starts_with("127.0.0.1 is this unit's own address"),
            "judged where it was compared: {}",
            why.reason
        );
        assert_eq!(steer::left("rotating.test"), 1, "one lookup");

        // Another address on the one lookup: not followed, nothing judged.
        let why = approve_next(&guard, next, first).await.unwrap_err();
        assert_eq!(why.refused, None, "{}", why.reason);
        assert!(
            why.reason.contains("leads to 127.0.0.2, not 127.0.0.1"),
            "{}",
            why.reason
        );
        assert_eq!(steer::left("rotating.test"), 0);

        // The scheme first: a hop that is never sent costs no lookup.
        steer::set("rotating.test", &[first]);
        let why = approve_next(&guard, "ftp://rotating.test/next", first)
            .await
            .unwrap_err();
        assert_eq!(why.reason, "only http and https URLs are sent");
        assert_eq!(steer::left("rotating.test"), 1, "no lookup");
        steer::clear();
    }

    /// The first request approved, then its socket cannot be bound: refused
    /// (exit 3), with the route and the LAN side the guard read.
    #[tokio::test]
    async fn a_first_request_that_cannot_be_bound_is_refused_with_what_the_guard_read() {
        let approved = Guard::default().approve(Ipv4Addr::LOCALHOST).await.unwrap();
        let gone = Approved {
            dev: "sctl-none0".into(),
            ..approved
        };
        let req = HttpRequest {
            url: "http://127.0.0.1:9/".into(),
            max_redirects: DEFAULT_MAX_REDIRECTS,
            timeout_ms: Some(3000),
            ..HttpRequest::default()
        };
        match follow("/tmp", &req, &Guard::default(), gone).await {
            Err(HttpError::Refused(r)) => {
                assert_eq!(r.refused, Code::GuardFailed);
                assert!(
                    r.reason
                        .starts_with("cannot bind to sctl-none0 from 127.0.0.1: "),
                    "{}",
                    r.reason
                );
                assert_eq!(r.route.as_ref().map(|v| v.kind), Some("local"), "{r:?}");
                assert!(r.lan.is_some(), "{r:?}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_redirect_the_guard_refuses_ends_the_request_with_its_reason() {
        let (first, _at_first) = server(
            "127.0.0.1:0",
            "HTTP/1.1 302 Found\r\nLocation: http://255.255.255.255/x\r\nContent-Length: 0\r\n\r\n"
                .into(),
        )
        .await;
        let req = HttpRequest {
            url: format!("http://{first}/"),
            max_redirects: DEFAULT_MAX_REDIRECTS,
            timeout_ms: Some(3000),
            ..HttpRequest::default()
        };
        let out = request("/tmp", &req, &Guard::default()).await.unwrap();
        assert_eq!(out.status, 302);
        let why = out.not_followed.expect("not followed");
        assert_eq!(why.refused, Some(Code::NotUnicast), "{}", why.reason);
        // The first request itself refused: nothing sent, exit 3.
        let req = HttpRequest {
            url: "http://255.255.255.255/".into(),
            ..req
        };
        match request("/tmp", &req, &Guard::default()).await {
            Err(HttpError::Refused(r)) => assert_eq!(r.refused, Code::NotUnicast),
            other => panic!("{other:?}"),
        }
    }
}
