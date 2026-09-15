//! Health check implementations.
//!
//! Each check validates its inputs, then runs the check using direct process
//! execution (no shell interpretation) via `exec_args`. The only exception is
//! `custom_script`, which intentionally uses shell execution (`exec_simple`)
//! since the command is operator-configured.

use std::process::Stdio;
use std::time::Instant;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use super::{profiles, ApiProfile, CheckSpec, Credential};

/// Result of a single health check.
#[derive(Debug, Clone, Default)]
pub struct CheckResult {
    /// Whether the check succeeded (target responded).
    pub ok: bool,
    /// Round-trip time in milliseconds (None if check failed).
    pub latency_ms: Option<u64>,
    /// Human-readable detail (e.g., "PING OK 12ms", "HTTP 200 OK 45ms").
    pub detail: String,
    /// HTTP status code if applicable.
    pub http_status: Option<u16>,
    /// Structured snapshot from an `http_api` profile.
    pub data: Option<serde_json::Value>,
    /// A login session the profile established or renewed, for the monitor
    /// to keep for the next tick. `None` leaves the cached one alone.
    pub session: Option<String>,
    /// The certificate fingerprint a TLS target presented when the pin did
    /// not match or none was configured, so an operator can pin it.
    pub presented_sha256: Option<String>,
}

impl CheckResult {
    pub fn failed(detail: impl Into<String>) -> Self {
        Self {
            ok: false,
            detail: detail.into(),
            ..Self::default()
        }
    }
}

/// What a check may need beyond its spec: where the pin store lives, the
/// credential its spec names, and the session cached from the last run.
#[derive(Debug, Clone, Default)]
pub struct CheckContext {
    pub data_dir: String,
    pub credential: Option<Credential>,
    pub session: Option<String>,
}

/// Run a health check according to the spec. Never panics — errors are
/// captured in the `CheckResult`. A kind that needs context (`http_api`)
/// fails honestly when called this way.
pub async fn run_check(spec: &CheckSpec) -> CheckResult {
    run_check_with(spec, &CheckContext::default()).await
}

/// Run a health check with the context an `http_api` target needs.
pub async fn run_check_with(spec: &CheckSpec, ctx: &CheckContext) -> CheckResult {
    match spec {
        CheckSpec::HttpApi {
            base_url,
            profile,
            pin_sha256,
            credential_id,
            timeout_ms,
        } => {
            if credential_id.is_some() && ctx.credential.is_none() {
                return CheckResult::failed(format!(
                    "API NO CREDENTIAL: {} is not in the credentials store",
                    credential_id.as_deref().unwrap_or("")
                ));
            }
            match profile {
                ApiProfile::Peplink => {
                    // Boxed: a seven-request profile is a large future, and it
                    // would otherwise be inlined into every caller's stack frame.
                    Box::pin(profiles::peplink::check(
                        ctx,
                        base_url,
                        pin_sha256.as_deref(),
                        *timeout_ms,
                    ))
                    .await
                }
            }
        }
        CheckSpec::Ping { host, timeout_ms } => check_ping(host, *timeout_ms).await,
        // One path for both schemes: the URL carries the scheme and curl
        // is told to accept the LAN gear's self-signed certificates.
        CheckSpec::Http {
            url,
            expected_status,
            timeout_ms,
        }
        | CheckSpec::Https {
            url,
            expected_status,
            timeout_ms,
        } => check_http(url, expected_status.unwrap_or(200), *timeout_ms).await,
        CheckSpec::TcpPort {
            host,
            port,
            timeout_ms,
        } => check_tcp(host, *port, *timeout_ms).await,
        CheckSpec::Snmp {
            host,
            community,
            timeout_ms,
        } => check_snmp(host, community.as_deref().unwrap_or("public"), *timeout_ms).await,
        CheckSpec::CustomScript {
            command,
            timeout_ms,
        } => check_custom(command, *timeout_ms).await,
    }
}

// ─── Input validation ───────────────────────────────────────────────

/// Validate that a host string contains only safe characters (IP or hostname).
fn validate_host(host: &str) -> Result<(), String> {
    if host.is_empty() || host.len() > 253 {
        return Err(format!("invalid host length: {}", host.len()));
    }
    if !host
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '.' | ':' | '-' | '_'))
    {
        return Err(format!("host contains invalid characters: {host}"));
    }
    Ok(())
}

/// Validate that a URL starts with http(s):// and has a reasonable length.
fn validate_url(url: &str) -> Result<(), String> {
    if url.is_empty() || url.len() > 2048 {
        return Err(format!("invalid URL length: {}", url.len()));
    }
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("URL must start with http:// or https://".into());
    }
    Ok(())
}

/// Validate SNMP community string (alphanumeric + basic punctuation).
fn validate_community(community: &str) -> Result<(), String> {
    if community.is_empty() || community.len() > 64 {
        return Err(format!("invalid community length: {}", community.len()));
    }
    if !community
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '!' | '@' | '#'))
    {
        return Err(format!(
            "community contains invalid characters: {community}"
        ));
    }
    Ok(())
}

// ─── Check implementations ──────────────────────────────────────────

/// ICMP ping check using the system `ping` command (args-based, no shell).
async fn check_ping(host: &str, timeout_ms: Option<u64>) -> CheckResult {
    if let Err(e) = validate_host(host) {
        return CheckResult {
            ok: false,
            latency_ms: None,
            detail: format!("PING INVALID: {e}"),
            http_status: None,
            ..CheckResult::default()
        };
    }
    let timeout_secs = timeout_ms.unwrap_or(2000) / 1000;
    let timeout_secs = timeout_secs.max(1);
    let start = Instant::now();

    let ts = timeout_secs.to_string();
    let output = exec_args(
        "ping",
        &["-c", "1", "-W", &ts, host],
        timeout_ms.unwrap_or(5000),
    )
    .await;

    let elapsed = start.elapsed().as_millis() as u64;

    match output {
        Ok((0, stdout, _stderr)) => {
            // Parse RTT from "time=12.3 ms" in ping output
            let rtt = parse_ping_rtt(&stdout).unwrap_or(elapsed);
            CheckResult {
                ok: true,
                latency_ms: Some(rtt),
                detail: format!("PING OK {rtt}ms"),
                http_status: None,
                ..CheckResult::default()
            }
        }
        Ok((_exit, _stdout, stderr)) => CheckResult {
            ok: false,
            latency_ms: None,
            detail: format!("PING FAIL: {}", first_line(&stderr).unwrap_or("timeout")),
            http_status: None,
            ..CheckResult::default()
        },
        Err(e) => CheckResult {
            ok: false,
            latency_ms: None,
            detail: format!("PING ERROR: {e}"),
            http_status: None,
            ..CheckResult::default()
        },
    }
}

/// HTTP/HTTPS check using curl (args-based, no shell interpretation).
///
/// HEAD first: the status line and headers are the whole answer, and the
/// body never crosses the wire. Before 2026-09-15 this was a plain GET with
/// the body discarded after transfer, which made a 60 s check of a gateway's
/// 82 KB page cost 118 MB/day on the site's WAN. Only a server that refuses
/// HEAD (405, 501) gets a GET, and that GET asks for one byte
/// (`Range: bytes=0-0`); a 206 to that request is the resource saying 200.
/// A server that ignores Range still sends its page, the old cost, paid only
/// by HEAD-refusing servers. Latency is time to first byte, not full transfer.
async fn check_http(url: &str, expected_status: u16, timeout_ms: Option<u64>) -> CheckResult {
    if let Err(e) = validate_url(url) {
        return CheckResult {
            ok: false,
            latency_ms: None,
            detail: format!("HTTP INVALID: {e}"),
            http_status: None,
            ..CheckResult::default()
        };
    }
    let connect_timeout = (timeout_ms.unwrap_or(5000) / 1000).max(1);
    let ct = connect_timeout.to_string();
    let budget = timeout_ms.unwrap_or(10000);
    let start = Instant::now();

    let mut attempt = curl_status(url, &ct, budget, &["-I"]).await;
    let mut ranged = false;
    if let Ok((code, _)) = attempt {
        if code == 405 || code == 501 {
            attempt = curl_status(url, &ct, budget, &["-r", "0-0"]).await;
            ranged = true;
        }
    }

    let elapsed = start.elapsed().as_millis() as u64;

    match attempt {
        Ok((status_code, time_secs)) => {
            #[allow(clippy::cast_sign_loss)]
            let latency = (time_secs * 1000.0) as u64;
            let latency = if latency == 0 { elapsed } else { latency };
            let matched = status_code == expected_status
                || (ranged && status_code == 206 && expected_status == 200);

            if matched {
                CheckResult {
                    ok: true,
                    latency_ms: Some(latency),
                    detail: format!("HTTP {status_code} OK {latency}ms"),
                    http_status: Some(status_code),
                    ..CheckResult::default()
                }
            } else {
                CheckResult {
                    ok: false,
                    latency_ms: Some(latency),
                    detail: format!("HTTP {status_code} (expected {expected_status}) {latency}ms"),
                    http_status: Some(status_code),
                    ..CheckResult::default()
                }
            }
        }
        Err(CurlError::Exit(reason)) => CheckResult {
            ok: false,
            latency_ms: None,
            detail: format!("HTTP FAIL: {reason}"),
            http_status: None,
            ..CheckResult::default()
        },
        Err(CurlError::Spawn(e)) => CheckResult {
            ok: false,
            latency_ms: None,
            detail: format!("HTTP ERROR: {e}"),
            http_status: None,
            ..CheckResult::default()
        },
    }
}

enum CurlError {
    /// curl ran and reported a failure (its first stderr line).
    Exit(String),
    /// curl could not be spawned or timed out.
    Spawn(String),
}

/// One curl request that reads status and time-to-first-byte only. `extra`
/// selects the method shape (`-I` for HEAD, `-r 0-0` for a one-byte GET).
async fn curl_status(
    url: &str,
    connect_timeout_secs: &str,
    budget_ms: u64,
    extra: &[&str],
) -> Result<(u16, f64), CurlError> {
    let mut args: Vec<&str> = vec![
        "-s",
        "-o",
        "/dev/null",
        "-w",
        "%{http_code} %{time_starttransfer}",
        "--connect-timeout",
        connect_timeout_secs,
        "-k",
    ];
    args.extend_from_slice(extra);
    args.push(url);

    match exec_args("curl", &args, budget_ms).await {
        Ok((0, stdout, _stderr)) => {
            // "200 0.045123"
            let parts: Vec<&str> = stdout.split_whitespace().collect();
            let status_code: u16 = parts.first().and_then(|s| s.parse().ok()).unwrap_or(0);
            let time_secs: f64 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            Ok((status_code, time_secs))
        }
        Ok((_exit, _stdout, stderr)) => Err(CurlError::Exit(
            first_line(&stderr)
                .unwrap_or("connection refused")
                .to_string(),
        )),
        Err(e) => Err(CurlError::Spawn(e)),
    }
}

/// TCP port reachability check: a native connect with a deadline.
///
/// This used to shell out to `nc -z -w`, and the busybox nc on RUTOS
/// (RUT241) accepts only `nc IPADDR PORT`, so every tcp_port target on that
/// unit failed. A connect needs no helper binary and its duration is the
/// latency.
async fn check_tcp(host: &str, port: u16, timeout_ms: Option<u64>) -> CheckResult {
    if let Err(e) = validate_host(host) {
        return CheckResult {
            ok: false,
            latency_ms: None,
            detail: format!("TCP INVALID: {e}"),
            http_status: None,
            ..CheckResult::default()
        };
    }
    let budget = std::time::Duration::from_millis(timeout_ms.unwrap_or(5000).max(1));
    let start = Instant::now();

    let attempt = tokio::time::timeout(budget, tokio::net::TcpStream::connect((host, port))).await;

    let elapsed = start.elapsed().as_millis() as u64;

    match attempt {
        Ok(Ok(_stream)) => CheckResult {
            ok: true,
            latency_ms: Some(elapsed),
            detail: format!("TCP {host}:{port} OK {elapsed}ms"),
            http_status: None,
            ..CheckResult::default()
        },
        Ok(Err(e)) => CheckResult {
            ok: false,
            latency_ms: None,
            detail: format!("TCP {host}:{port} FAIL: {e}"),
            http_status: None,
            ..CheckResult::default()
        },
        Err(_) => CheckResult {
            ok: false,
            latency_ms: None,
            detail: format!(
                "TCP {host}:{port} FAIL: no connection within {}ms",
                budget.as_millis()
            ),
            http_status: None,
            ..CheckResult::default()
        },
    }
}

/// SNMP check using snmpget (args-based, no shell interpretation).
async fn check_snmp(host: &str, community: &str, timeout_ms: Option<u64>) -> CheckResult {
    if let Err(e) = validate_host(host) {
        return CheckResult {
            ok: false,
            latency_ms: None,
            detail: format!("SNMP INVALID: {e}"),
            http_status: None,
            ..CheckResult::default()
        };
    }
    if let Err(e) = validate_community(community) {
        return CheckResult {
            ok: false,
            latency_ms: None,
            detail: format!("SNMP INVALID: {e}"),
            http_status: None,
            ..CheckResult::default()
        };
    }
    let timeout_secs = timeout_ms.unwrap_or(5000) / 1000;
    let timeout_secs = timeout_secs.max(1);
    let start = Instant::now();

    let ts = timeout_secs.to_string();
    let output = exec_args(
        "snmpget",
        &[
            "-v2c",
            "-c",
            community,
            "-t",
            &ts,
            "-r",
            "0",
            host,
            ".1.3.6.1.2.1.1.1.0",
        ],
        timeout_ms.unwrap_or(10000),
    )
    .await;

    let elapsed = start.elapsed().as_millis() as u64;

    match output {
        Ok((0, stdout, _stderr)) => CheckResult {
            ok: true,
            latency_ms: Some(elapsed),
            detail: format!("SNMP OK {elapsed}ms: {}", truncate(&stdout, 100)),
            http_status: None,
            ..CheckResult::default()
        },
        Ok((_exit, stdout, stderr)) => {
            let err = if stderr.is_empty() { &stdout } else { &stderr };
            CheckResult {
                ok: false,
                latency_ms: None,
                detail: format!("SNMP FAIL: {}", first_line(err).unwrap_or("timeout")),
                http_status: None,
                ..CheckResult::default()
            }
        }
        Err(e) => CheckResult {
            ok: false,
            latency_ms: None,
            detail: format!("SNMP ERROR: {e}"),
            http_status: None,
            ..CheckResult::default()
        },
    }
}

/// Custom script check — run a user-provided command via shell and check exit code.
/// This intentionally uses shell execution since the command is operator-configured.
async fn check_custom(command: &str, timeout_ms: Option<u64>) -> CheckResult {
    let start = Instant::now();
    let output = exec_simple(command, timeout_ms.unwrap_or(30000)).await;
    let elapsed = start.elapsed().as_millis() as u64;

    match output {
        Ok((0, stdout, _stderr)) => CheckResult {
            ok: true,
            latency_ms: Some(elapsed),
            detail: format!("SCRIPT OK {elapsed}ms: {}", truncate(stdout.trim(), 100)),
            http_status: None,
            ..CheckResult::default()
        },
        Ok((exit, stdout, stderr)) => {
            let out = if stderr.is_empty() { &stdout } else { &stderr };
            CheckResult {
                ok: false,
                latency_ms: None,
                detail: format!(
                    "SCRIPT FAIL (exit {exit}): {}",
                    first_line(out).unwrap_or("no output")
                ),
                http_status: None,
                ..CheckResult::default()
            }
        }
        Err(e) => CheckResult {
            ok: false,
            latency_ms: None,
            detail: format!("SCRIPT ERROR: {e}"),
            http_status: None,
            ..CheckResult::default()
        },
    }
}

// ─── Execution helpers ──────────────────────────────────────────────

/// Execute a command with explicit args (no shell interpretation).
/// Safe for use with user-controlled inputs like hostnames and URLs.
async fn exec_args(
    program: &str,
    args: &[&str],
    timeout_ms: u64,
) -> Result<(i32, String, String), String> {
    let mut child = Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("spawn failed: {e}"))?;

    read_child_output(&mut child, timeout_ms).await
}

/// Public variant of `exec_args` for use by the discovery module.
pub async fn exec_args_pub(
    program: &str,
    args: &[&str],
    timeout_ms: u64,
) -> Result<(i32, String, String), String> {
    exec_args(program, args, timeout_ms).await
}

/// Execute a shell command with timeout, returning (exit_code, stdout, stderr).
/// Public variant for use by the recovery action executor and discovery module.
pub async fn exec_simple_pub(cmd: &str, timeout_ms: u64) -> Result<(i32, String, String), String> {
    exec_simple(cmd, timeout_ms).await
}

/// Execute a shell command with timeout, returning (exit_code, stdout, stderr).
/// Uses `sh -c` — only safe with trusted or pre-validated input.
async fn exec_simple(cmd: &str, timeout_ms: u64) -> Result<(i32, String, String), String> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("spawn failed: {e}"))?;

    read_child_output(&mut child, timeout_ms).await
}

/// Read stdout/stderr from a child process with timeout.
async fn read_child_output(
    child: &mut tokio::process::Child,
    timeout_ms: u64,
) -> Result<(i32, String, String), String> {
    let mut stdout_handle = child.stdout.take().ok_or("no stdout")?;
    let mut stderr_handle = child.stderr.take().ok_or("no stderr")?;

    let timeout = tokio::time::Duration::from_millis(timeout_ms);
    if let Ok(result) = tokio::time::timeout(timeout, async {
        let mut stdout_buf = Vec::with_capacity(4096);
        let mut stderr_buf = Vec::with_capacity(4096);
        let (r1, r2) = tokio::join!(
            stdout_handle.read_to_end(&mut stdout_buf),
            stderr_handle.read_to_end(&mut stderr_buf),
        );
        r1.map_err(|e| format!("stdout read: {e}"))?;
        r2.map_err(|e| format!("stderr read: {e}"))?;
        drop(stdout_handle);
        drop(stderr_handle);
        let status = child.wait().await.map_err(|e| format!("wait: {e}"))?;
        Ok::<_, String>((
            status.code().unwrap_or(-1),
            String::from_utf8_lossy(&stdout_buf).to_string(),
            String::from_utf8_lossy(&stderr_buf).to_string(),
        ))
    })
    .await
    {
        result
    } else {
        let _ = child.kill().await;
        Err("timeout".to_string())
    }
}

// ─── String helpers ─────────────────────────────────────────────────

/// Parse RTT from ping output (e.g., "time=12.3 ms").
#[allow(clippy::cast_sign_loss)]
fn parse_ping_rtt(stdout: &str) -> Option<u64> {
    stdout
        .find("time=")
        .and_then(|i| {
            let rest = &stdout[i + 5..];
            let end = rest.find(|c: char| !c.is_ascii_digit() && c != '.')?;
            rest[..end].parse::<f64>().ok()
        })
        .map(|ms| ms as u64)
}

/// Get the first non-empty line of a string.
fn first_line(s: &str) -> Option<&str> {
    s.lines().find(|l| !l.trim().is_empty()).map(str::trim)
}

/// Truncate a string to max chars (UTF-8 safe), appending "..." if truncated.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{truncated}...")
    }
}

// ─── Input validation (public, for discovery module) ────────────────

/// Validate that a string looks like a valid IPv4 address.
pub fn validate_ipv4(ip: &str) -> bool {
    let octets: Vec<&str> = ip.split('.').collect();
    octets.len() == 4 && octets.iter().all(|o| o.parse::<u8>().is_ok())
}

/// Validate that a string looks like a valid CIDR subnet (e.g., "192.168.1.0/24").
pub fn validate_cidr(s: &str) -> bool {
    let Some((ip, prefix)) = s.split_once('/') else {
        return false;
    };
    validate_ipv4(ip) && prefix.parse::<u8>().is_ok_and(|p| p <= 32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_ping_rtt() {
        assert_eq!(
            parse_ping_rtt("64 bytes from 192.168.1.1: icmp_seq=1 ttl=64 time=12.3 ms"),
            Some(12)
        );
        assert_eq!(
            parse_ping_rtt("64 bytes from 192.168.1.1: icmp_seq=1 ttl=64 time=0.5 ms"),
            Some(0)
        );
        assert_eq!(parse_ping_rtt("no such host"), None);
    }

    #[test]
    fn test_first_line() {
        assert_eq!(first_line("hello\nworld"), Some("hello"));
        assert_eq!(first_line("\n\nhello"), Some("hello"));
        assert_eq!(first_line(""), None);
    }

    #[test]
    fn test_validate_host() {
        assert!(validate_host("192.168.1.1").is_ok());
        assert!(validate_host("my-router.local").is_ok());
        assert!(validate_host("::1").is_ok());
        assert!(validate_host("host with spaces").is_err());
        assert!(validate_host("host;rm -rf /").is_err());
        assert!(validate_host("").is_err());
    }

    #[test]
    fn test_validate_url() {
        assert!(validate_url("http://192.168.1.1").is_ok());
        assert!(validate_url("https://router.local/status").is_ok());
        assert!(validate_url("ftp://bad").is_err());
        assert!(validate_url("").is_err());
    }

    #[test]
    fn test_validate_community() {
        assert!(validate_community("public").is_ok());
        assert!(validate_community("my-community_v2").is_ok());
        assert!(validate_community("has spaces").is_err());
        assert!(validate_community("has;semicolon").is_err());
        assert!(validate_community("").is_err());
    }

    #[test]
    fn test_validate_ipv4() {
        assert!(validate_ipv4("192.168.1.1"));
        assert!(validate_ipv4("10.0.0.1"));
        assert!(!validate_ipv4("999.999.999.999"));
        assert!(!validate_ipv4("not-an-ip"));
        assert!(!validate_ipv4(""));
    }

    #[test]
    fn test_validate_cidr() {
        assert!(validate_cidr("192.168.1.0/24"));
        assert!(validate_cidr("10.0.0.0/8"));
        assert!(!validate_cidr("192.168.1.0"));
        assert!(!validate_cidr("192.168.1.0/33"));
        assert!(!validate_cidr("not-a-cidr/24"));
    }

    #[test]
    fn test_truncate_utf8_safe() {
        assert_eq!(truncate("hello world", 5), "hello...");
        assert_eq!(truncate("hi", 5), "hi");
    }

    // ─── HTTP check fixtures ─────────────────────────────────────────
    //
    // A tiny in-process HTTP server that records every request head and
    // counts the bytes it writes, so a test can prove the body never left.

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::io::AsyncWriteExt;

    #[derive(Clone, Copy)]
    enum FixtureMode {
        /// HEAD and GET both answer 200 with a 1 MiB Content-Length; the
        /// body is written only for GET.
        HeadOk,
        /// HEAD is refused with 405; a GET with `Range: bytes=0-0` gets a
        /// one-byte 206; any other GET gets the full 1 MiB.
        HeadRefused,
    }

    struct Fixture {
        addr: std::net::SocketAddr,
        seen: Arc<Mutex<Vec<String>>>,
        bytes_out: Arc<AtomicUsize>,
    }

    const BIG: usize = 1024 * 1024;

    async fn spawn_fixture(mode: FixtureMode) -> Fixture {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let bytes_out = Arc::new(AtomicUsize::new(0));
        let (seen2, bytes2) = (seen.clone(), bytes_out.clone());
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut head = Vec::new();
                let mut buf = [0u8; 1024];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    let Ok(n) = sock.read(&mut buf).await else {
                        break;
                    };
                    if n == 0 {
                        break;
                    }
                    head.extend_from_slice(&buf[..n]);
                }
                let head = String::from_utf8_lossy(&head).to_string();
                seen2.lock().unwrap().push(head.clone());
                let first = head.lines().next().unwrap_or("").to_string();
                let is_head = first.starts_with("HEAD ");
                let ranged = head.to_ascii_lowercase().contains("range: bytes=0-0");
                let response: Vec<u8> = match (mode, is_head, ranged) {
                    (FixtureMode::HeadOk, true, _) => {
                        format!("HTTP/1.1 200 OK\r\nContent-Length: {BIG}\r\nConnection: close\r\n\r\n")
                            .into_bytes()
                    }
                    (FixtureMode::HeadRefused, true, _) => {
                        b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_vec()
                    }
                    (FixtureMode::HeadRefused, false, true) => {
                        format!("HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-0/{BIG}\r\nContent-Length: 1\r\nConnection: close\r\n\r\n<")
                            .into_bytes()
                    }
                    _ => {
                        let mut v = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {BIG}\r\nConnection: close\r\n\r\n"
                        )
                        .into_bytes();
                        v.resize(v.len() + BIG, b'x');
                        v
                    }
                };
                if sock.write_all(&response).await.is_ok() {
                    bytes2.fetch_add(response.len(), Ordering::SeqCst);
                }
                let _ = sock.shutdown().await;
            }
        });
        Fixture {
            addr,
            seen,
            bytes_out,
        }
    }

    fn curl_available() -> bool {
        std::process::Command::new("curl")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    #[tokio::test]
    async fn http_check_uses_head_and_never_transfers_the_body() {
        if !curl_available() {
            return;
        }
        let fx = spawn_fixture(FixtureMode::HeadOk).await;
        let r = check_http(&format!("http://{}/", fx.addr), 200, Some(3000)).await;
        assert!(r.ok, "{}", r.detail);
        assert_eq!(r.http_status, Some(200));
        assert!(r.latency_ms.is_some());
        let seen = fx.seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "one request: {seen:?}");
        assert!(seen[0].starts_with("HEAD /"), "{}", seen[0]);
        assert!(fx.bytes_out.load(Ordering::SeqCst) < 1024);
    }

    #[tokio::test]
    async fn http_check_falls_back_to_a_one_byte_range_get_when_head_is_refused() {
        if !curl_available() {
            return;
        }
        let fx = spawn_fixture(FixtureMode::HeadRefused).await;
        let r = check_http(&format!("http://{}/", fx.addr), 200, Some(3000)).await;
        assert!(r.ok, "{}", r.detail);
        assert_eq!(r.http_status, Some(206));
        let seen = fx.seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "HEAD then GET: {seen:?}");
        assert!(seen[1].starts_with("GET /"), "{}", seen[1]);
        assert!(
            seen[1].to_ascii_lowercase().contains("range: bytes=0-0"),
            "{}",
            seen[1]
        );
        assert!(fx.bytes_out.load(Ordering::SeqCst) < 1024);
    }

    #[tokio::test]
    async fn http_check_reports_an_unexpected_status_without_matching_it() {
        if !curl_available() {
            return;
        }
        let fx = spawn_fixture(FixtureMode::HeadOk).await;
        let r = check_http(&format!("http://{}/", fx.addr), 204, Some(3000)).await;
        assert!(!r.ok);
        assert_eq!(r.http_status, Some(200));
        assert!(r.detail.contains("expected 204"), "{}", r.detail);
    }

    #[tokio::test]
    async fn tcp_check_connects_natively() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let r = check_tcp("127.0.0.1", addr.port(), Some(2000)).await;
        assert!(r.ok, "{}", r.detail);
        assert!(r.latency_ms.is_some());
        assert!(r.detail.starts_with("TCP 127.0.0.1:"), "{}", r.detail);
    }

    /// A socket that is bound but never listens refuses every connect. A
    /// freed ephemeral port is not the same thing: another test's fixture
    /// can bind it in the meantime, and did once on CI's floating toolchain.
    #[tokio::test]
    async fn tcp_check_fails_on_a_closed_port() {
        let held = tokio::net::TcpSocket::new_v4().unwrap();
        held.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let port = held.local_addr().unwrap().port();
        let r = check_tcp("127.0.0.1", port, Some(2000)).await;
        drop(held);
        assert!(!r.ok);
        assert!(r.latency_ms.is_none());
        assert!(r.detail.contains("FAIL"), "{}", r.detail);
    }
}
