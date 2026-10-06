//! `sctl net`: the network tools a command, runbook or recovery action needs,
//! the same on every unit whatever the firmware ships (ADR-006).
//!
//! - `sctl net snmp get|walk|set`: SNMP v1/v2c
//! - `sctl net http`: one request through the agent's own TLS stack, with a
//!   cookie jar and redirects
//! - `sctl net tcp`: does a port answer
//!
//! Every tool prints one JSON object on stdout and exits 0 when the request
//! did what it was asked, 1 when it failed (the object says why), 2 on a
//! usage error. Secrets come from the environment (`SCTL_SNMP_COMMUNITY`,
//! `SCTL_CRED_USER_n`/`SCTL_CRED_PASS_n` expanded by the shell), never from
//! a value written in a runbook.

pub mod http;
pub mod snmp;
pub mod tcp;

pub fn help() -> &'static str {
    "Network tools for commands, runbooks and recovery actions\n\n\
Usage:\n  sctl net snmp get <host> <oid>... [OPTIONS]\n  sctl net snmp walk <host> <oid> [--limit N] [OPTIONS]\n  sctl net snmp set <host> <oid> <i|s> <value> [OPTIONS]\n  sctl net http <url> [OPTIONS]
  sctl net tcp <host> <port> [--timeout MS]

\
HTTP options:\n  -X <METHOD>          Method (default GET, POST with -d or -F)\n  -H <'Name: value'>   Header, repeatable\n  -d <BODY>            Raw body\n  -F <name=value>      Form field, repeatable (urlencoded)\n  -u <user:password>   Basic auth (pass \"$SCTL_CRED_USER_1:$SCTL_CRED_PASS_1\")\n  -b <FILE>            Cookie jar, read before and written after (0600)\n      --max-redirects <N>  Redirects to follow (default 5)\n      --pin <SHA256>   Expected certificate fingerprint\n      --tofu           Trust and record the certificate on first contact\n      --fail           Exit 1 on an HTTP status of 400 or more\n      --timeout <MS>   Default 10000\n\n\
SNMP options:\n  -c, --community <C>  Community (default: $SCTL_SNMP_COMMUNITY, else public for get and walk; set needs one)\n  -v <1|2c>            Protocol version (default 2c)\n      --port <N>       UDP port (default 161)\n      --timeout <MS>   Per-request timeout (default 3000)\n\n\
Output: one JSON object; exit 0 on success, 1 on failure, 2 on a usage error."
}

/// Run `sctl net <args>`; returns the exit code.
pub async fn main(args: Vec<String>) -> i32 {
    match args.first().map(String::as_str) {
        Some("snmp") => cli::snmp(&args[1..]).await,
        Some("http") => cli::http(&args[1..]).await,
        Some("tcp") => cli::tcp(&args[1..]).await,
        Some("-h" | "--help" | "help") | None => {
            println!("{}", help());
            if args.is_empty() {
                2
            } else {
                0
            }
        }
        Some(other) => usage(&format!("unknown net tool '{other}'")),
    }
}

fn usage(message: &str) -> i32 {
    eprintln!("{message}\n\n{}", help());
    2
}

fn print(value: &serde_json::Value) {
    println!("{}", serde_json::to_string(value).unwrap_or_default());
}

mod cli {
    use std::time::Duration;

    use serde_json::json;

    use super::snmp::{self, SetValue, Target, Version};
    use super::{print, usage};

    struct Parsed {
        positional: Vec<String>,
        community: Option<String>,
        version: Version,
        port: u16,
        timeout_ms: u64,
        limit: usize,
    }

    fn parse(args: &[String]) -> Result<Parsed, String> {
        let mut p = Parsed {
            positional: Vec::new(),
            community: None,
            version: Version::V2c,
            port: 161,
            timeout_ms: 3000,
            limit: 1000,
        };
        let mut it = args.iter();
        while let Some(a) = it.next() {
            let mut value = |name: &str| {
                it.next()
                    .cloned()
                    .ok_or_else(|| format!("{name} needs a value"))
            };
            match a.as_str() {
                "-c" | "--community" => p.community = Some(value(a)?),
                "-v" => {
                    p.version = match value("-v")?.as_str() {
                        "1" => Version::V1,
                        "2c" | "2" => Version::V2c,
                        other => return Err(format!("unknown SNMP version '{other}'")),
                    }
                }
                "--port" => {
                    p.port = value(a)?
                        .parse()
                        .map_err(|_| "--port must be a number".to_string())?;
                }
                "--timeout" => {
                    p.timeout_ms = value(a)?
                        .parse()
                        .map_err(|_| "--timeout must be milliseconds".to_string())?;
                }
                "--limit" => {
                    p.limit = value(a)?
                        .parse()
                        .map_err(|_| "--limit must be a number".to_string())?;
                }
                other
                    if other.starts_with('-')
                        && other.len() > 1
                        && other.parse::<i64>().is_err() =>
                {
                    return Err(format!("unknown option '{other}'"));
                }
                other => p.positional.push(other.to_string()),
            }
        }
        Ok(p)
    }

    pub async fn tcp(args: &[String]) -> i32 {
        let mut positional = Vec::new();
        let mut timeout_ms = 3000u64;
        let mut it = args.iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--timeout" => match it.next().and_then(|v| v.parse().ok()) {
                    Some(ms) => timeout_ms = ms,
                    None => return usage("--timeout must be milliseconds"),
                },
                other => positional.push(other.to_string()),
            }
        }
        let (Some(host), Some(port)) = (
            positional.first(),
            positional.get(1).and_then(|p| p.parse::<u16>().ok()),
        ) else {
            return usage("sctl net tcp <host> <port>");
        };
        let out = super::tcp::probe(
            host,
            port,
            Duration::from_millis(timeout_ms.clamp(100, 60_000)),
        )
        .await;
        let open = out.open;
        print(
            &json!({"ok": open, "host": host, "port": port, "open": out.open, "ms": out.ms, "error": out.error}),
        );
        i32::from(!open)
    }

    pub async fn http(args: &[String]) -> i32 {
        use super::http::{form_body, request, HttpRequest, DEFAULT_MAX_REDIRECTS};
        let mut req = HttpRequest {
            max_redirects: DEFAULT_MAX_REDIRECTS,
            ..HttpRequest::default()
        };
        let mut form: Vec<(String, String)> = Vec::new();
        let mut fail = false;
        let mut config: Option<String> = None;
        let mut it = args.iter();
        while let Some(a) = it.next() {
            let mut value = |name: &str| {
                it.next()
                    .cloned()
                    .ok_or_else(|| format!("{name} needs a value"))
            };
            let step: Result<(), String> = (|| {
                match a.as_str() {
                    "-X" | "--request" => req.method = value(a)?,
                    "-H" | "--header" => {
                        let h = value(a)?;
                        let (k, v) = h
                            .split_once(':')
                            .ok_or_else(|| format!("header '{h}' is not 'Name: value'"))?;
                        req.headers
                            .push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
                    }
                    "-d" | "--data" => req.body = Some(value(a)?),
                    "-F" | "--form" => {
                        let f = value(a)?;
                        let (k, v) = f
                            .split_once('=')
                            .ok_or_else(|| format!("form field '{f}' is not name=value"))?;
                        form.push((k.to_string(), v.to_string()));
                    }
                    "-u" | "--user" => req.basic = Some(value(a)?),
                    "-b" | "--cookie-jar" => req.cookie_jar = Some(value(a)?),
                    "--max-redirects" => {
                        req.max_redirects = value(a)?
                            .parse()
                            .map_err(|_| "--max-redirects must be a number".to_string())?;
                    }
                    "--timeout" => {
                        req.timeout_ms = Some(
                            value(a)?
                                .parse()
                                .map_err(|_| "--timeout must be milliseconds".to_string())?,
                        );
                    }
                    "--max-bytes" => {
                        req.max_bytes = Some(
                            value(a)?
                                .parse()
                                .map_err(|_| "--max-bytes must be a number".to_string())?,
                        );
                    }
                    "--pin" => req.pin_sha256 = Some(value(a)?),
                    "--tofu" => req.allow_tofu = true,
                    "--fail" => fail = true,
                    "--config" => config = Some(value(a)?),
                    other if other.starts_with('-') => {
                        return Err(format!("unknown option '{other}'"))
                    }
                    other if req.url.is_empty() => req.url = other.to_string(),
                    other => return Err(format!("unexpected argument '{other}'")),
                }
                Ok(())
            })();
            if let Err(e) = step {
                return usage(&e);
            }
        }
        if req.url.is_empty() {
            return usage("sctl net http <url>");
        }
        if !form.is_empty() {
            if req.body.is_some() {
                return usage("-d and -F cannot be combined");
            }
            req.body = Some(form_body(&form));
            if !req.headers.iter().any(|(k, _)| k == "content-type") {
                req.headers.push((
                    "content-type".into(),
                    "application/x-www-form-urlencoded".into(),
                ));
            }
        }
        if req.method.is_empty() {
            req.method = if req.body.is_some() { "POST" } else { "GET" }.into();
        }
        let state_dir = crate::config::Config::load(config.as_deref())
            .server
            .state_dir()
            .to_string();
        match request(&state_dir, &req).await {
            Ok(out) => {
                let failed = fail && out.status >= 400;
                let mut v = serde_json::to_value(&out).unwrap_or_default();
                v["ok"] = json!(!failed);
                print(&v);
                i32::from(failed)
            }
            Err(e) => {
                print(&json!({"ok": false, "url": req.url, "error": e.to_string()}));
                1
            }
        }
    }

    pub async fn snmp(args: &[String]) -> i32 {
        let Some(verb) = args.first().map(String::as_str) else {
            return usage("sctl net snmp needs get, walk or set");
        };
        let p = match parse(&args[1..]) {
            Ok(p) => p,
            Err(e) => return usage(&e),
        };
        let Some(host) = p.positional.first().cloned() else {
            return usage("a host is required");
        };
        let env_community = std::env::var("SCTL_SNMP_COMMUNITY")
            .ok()
            .filter(|c| !c.is_empty());
        let community = match (p.community.clone().or(env_community), verb) {
            (Some(c), _) => c,
            (None, "set") => return usage("set needs a community: -c or $SCTL_SNMP_COMMUNITY"),
            (None, _) => "public".to_string(),
        };
        let target = Target {
            host: host.clone(),
            port: p.port,
            community,
            version: p.version,
            timeout: Duration::from_millis(p.timeout_ms.clamp(200, 60_000)),
        };
        let rest = &p.positional[1..];
        let result = match verb {
            "get" if !rest.is_empty() => snmp::get(&target, rest).await,
            "walk" if rest.len() == 1 => snmp::walk(&target, &rest[0], p.limit.max(1)).await,
            "set" if rest.len() == 3 => {
                let value = match rest[1].as_str() {
                    "i" => match rest[2].parse() {
                        Ok(n) => SetValue::Integer(n),
                        Err(_) => return usage("an i value must be an integer"),
                    },
                    "s" => SetValue::Text(rest[2].clone()),
                    other => return usage(&format!("unknown set type '{other}' (i or s)")),
                };
                snmp::set(&target, &rest[0], &value).await
            }
            "get" | "walk" | "set" => return usage(&format!("wrong arguments for snmp {verb}")),
            other => return usage(&format!("unknown snmp verb '{other}'")),
        };
        match result {
            Ok(bindings) => {
                print(&json!({"ok": true, "host": host, "bindings": bindings}));
                0
            }
            Err(e) => {
                print(&json!({"ok": false, "host": host, "error": e.to_string()}));
                1
            }
        }
    }
}
