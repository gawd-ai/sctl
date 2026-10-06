//! `sctl net http`: one HTTP(S) request through the agent's own TLS stack
//! (`routes::fetch`, its pin and TOFU trust ladder), plus what a login flow on
//! LAN gear needs and a single fetch does not: a cookie jar kept in a file
//! between steps, and redirects followed (ADR-006).

use std::collections::HashMap;

use base64::Engine as _;
use serde::Serialize;

use crate::routes::fetch::{execute, FetchError, FetchRequest};

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

fn host_of(url: &str) -> String {
    url.parse::<hyper::Uri>()
        .ok()
        .and_then(|u| u.host().map(str::to_string))
        .unwrap_or_default()
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

pub async fn request(state_dir: &str, req: &HttpRequest) -> Result<HttpOutcome, FetchError> {
    let mut jar = req.cookie_jar.as_deref().map(Jar::load).unwrap_or_default();
    let mut url = req.url.clone();
    let mut method = if req.method.is_empty() {
        "GET".to_string()
    } else {
        req.method.to_ascii_uppercase()
    };
    let mut body = req.body.clone();
    let mut redirects = Vec::new();
    loop {
        let host = host_of(&url);
        let mut headers: HashMap<String, String> = req.headers.iter().cloned().collect();
        if let Some(basic) = &req.basic {
            let token = base64::engine::general_purpose::STANDARD.encode(basic.as_bytes());
            headers.insert("authorization".into(), format!("Basic {token}"));
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
            ..FetchRequest::default()
        };
        let resp = execute(state_dir, &fetch).await?;
        for c in &resp.set_cookies {
            jar.store(&host, c);
        }
        let location = resp.headers.get("location").cloned();
        let redirect = matches!(resp.status, 301 | 302 | 303 | 307 | 308);
        if redirect && redirects.len() < req.max_redirects {
            if let Some(loc) = location {
                let next = resolve(&url, &loc);
                // 303, and 301/302 after a POST, continue as a GET without a body.
                if resp.status == 303 || (matches!(resp.status, 301 | 302) && method == "POST") {
                    method = "GET".into();
                    body = None;
                }
                redirects.push(next.clone());
                url = next;
                continue;
            }
        }
        if let Some(path) = &req.cookie_jar {
            jar.save(path)
                .map_err(|e| FetchError::Failed(format!("cookie jar {path}: {e}")))?;
        }
        return Ok(HttpOutcome {
            status: resp.status,
            url,
            redirects,
            headers: resp.headers,
            body: resp.body,
            body_base64: resp.body_base64,
            truncated: resp.truncated,
            elapsed_ms: resp.elapsed_ms,
            tls_sha256: resp.tls.as_ref().map(|t| t.sha256.clone()),
            tls_trust: resp.tls.as_ref().map(|t| t.trust),
        });
    }
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
        let out = request("/tmp", &req).await.unwrap();
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
}
