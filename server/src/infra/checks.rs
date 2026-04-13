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

use super::CheckSpec;

/// Result of a single health check.
#[derive(Debug, Clone)]
pub struct CheckResult {
    /// Whether the check succeeded (target responded).
    pub ok: bool,
    /// Round-trip time in milliseconds (None if check failed).
    pub latency_ms: Option<u64>,
    /// Human-readable detail (e.g., "PING OK 12ms", "HTTP 200 OK 45ms").
    pub detail: String,
    /// HTTP status code if applicable.
    pub http_status: Option<u16>,
}

/// Run a health check according to the spec. Never panics — errors are
/// captured in the `CheckResult`.
pub async fn run_check(spec: &CheckSpec) -> CheckResult {
    match spec {
        CheckSpec::Ping { host, timeout_ms } => check_ping(host, *timeout_ms).await,
        CheckSpec::Http {
            url,
            expected_status,
            timeout_ms,
        } => check_http(url, expected_status.unwrap_or(200), *timeout_ms, false).await,
        CheckSpec::Https {
            url,
            expected_status,
            timeout_ms,
        } => check_http(url, expected_status.unwrap_or(200), *timeout_ms, true).await,
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
            }
        }
        Ok((_exit, _stdout, stderr)) => CheckResult {
            ok: false,
            latency_ms: None,
            detail: format!("PING FAIL: {}", first_line(&stderr).unwrap_or("timeout")),
            http_status: None,
        },
        Err(e) => CheckResult {
            ok: false,
            latency_ms: None,
            detail: format!("PING ERROR: {e}"),
            http_status: None,
        },
    }
}

/// HTTP/HTTPS check using curl (args-based, no shell interpretation).
async fn check_http(
    url: &str,
    expected_status: u16,
    timeout_ms: Option<u64>,
    _https: bool,
) -> CheckResult {
    if let Err(e) = validate_url(url) {
        return CheckResult {
            ok: false,
            latency_ms: None,
            detail: format!("HTTP INVALID: {e}"),
            http_status: None,
        };
    }
    let connect_timeout = timeout_ms.unwrap_or(5000) / 1000;
    let connect_timeout = connect_timeout.max(1);
    let start = Instant::now();

    let ct = connect_timeout.to_string();
    let output = exec_args(
        "curl",
        &[
            "-s",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code} %{time_total}",
            "--connect-timeout",
            &ct,
            "-k",
            url,
        ],
        timeout_ms.unwrap_or(10000),
    )
    .await;

    let elapsed = start.elapsed().as_millis() as u64;

    match output {
        Ok((0, stdout, _stderr)) => {
            // Parse "200 0.045123" from curl output
            let parts: Vec<&str> = stdout.split_whitespace().collect();
            let status_code: u16 = parts.first().and_then(|s| s.parse().ok()).unwrap_or(0);
            let time_secs: f64 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let latency = (time_secs * 1000.0) as u64;
            let latency = if latency == 0 { elapsed } else { latency };

            if status_code == expected_status {
                CheckResult {
                    ok: true,
                    latency_ms: Some(latency),
                    detail: format!("HTTP {status_code} OK {latency}ms"),
                    http_status: Some(status_code),
                }
            } else {
                CheckResult {
                    ok: false,
                    latency_ms: Some(latency),
                    detail: format!("HTTP {status_code} (expected {expected_status}) {latency}ms"),
                    http_status: Some(status_code),
                }
            }
        }
        Ok((_exit, _stdout, stderr)) => CheckResult {
            ok: false,
            latency_ms: None,
            detail: format!(
                "HTTP FAIL: {}",
                first_line(&stderr).unwrap_or("connection refused")
            ),
            http_status: None,
        },
        Err(e) => CheckResult {
            ok: false,
            latency_ms: None,
            detail: format!("HTTP ERROR: {e}"),
            http_status: None,
        },
    }
}

/// TCP port reachability check using nc (args-based, no shell interpretation).
async fn check_tcp(host: &str, port: u16, timeout_ms: Option<u64>) -> CheckResult {
    if let Err(e) = validate_host(host) {
        return CheckResult {
            ok: false,
            latency_ms: None,
            detail: format!("TCP INVALID: {e}"),
            http_status: None,
        };
    }
    let timeout_secs = timeout_ms.unwrap_or(5000) / 1000;
    let timeout_secs = timeout_secs.max(1);
    let start = Instant::now();

    let ts = timeout_secs.to_string();
    let port_str = port.to_string();
    let output = exec_args(
        "nc",
        &["-z", "-w", &ts, host, &port_str],
        timeout_ms.unwrap_or(10000),
    )
    .await;

    let elapsed = start.elapsed().as_millis() as u64;

    match output {
        Ok((0, _, _)) => CheckResult {
            ok: true,
            latency_ms: Some(elapsed),
            detail: format!("TCP {host}:{port} OK {elapsed}ms"),
            http_status: None,
        },
        Ok((_exit, _stdout, stderr)) => CheckResult {
            ok: false,
            latency_ms: None,
            detail: format!(
                "TCP {host}:{port} FAIL: {}",
                first_line(&stderr).unwrap_or("connection refused or timeout")
            ),
            http_status: None,
        },
        Err(e) => CheckResult {
            ok: false,
            latency_ms: None,
            detail: format!("TCP ERROR: {e}"),
            http_status: None,
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
        };
    }
    if let Err(e) = validate_community(community) {
        return CheckResult {
            ok: false,
            latency_ms: None,
            detail: format!("SNMP INVALID: {e}"),
            http_status: None,
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
        },
        Ok((_exit, stdout, stderr)) => {
            let err = if stderr.is_empty() { &stdout } else { &stderr };
            CheckResult {
                ok: false,
                latency_ms: None,
                detail: format!("SNMP FAIL: {}", first_line(err).unwrap_or("timeout")),
                http_status: None,
            }
        }
        Err(e) => CheckResult {
            ok: false,
            latency_ms: None,
            detail: format!("SNMP ERROR: {e}"),
            http_status: None,
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
            }
        }
        Err(e) => CheckResult {
            ok: false,
            latency_ms: None,
            detail: format!("SCRIPT ERROR: {e}"),
            http_status: None,
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
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
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
    s.lines().find(|l| !l.trim().is_empty()).map(|l| l.trim())
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
}
