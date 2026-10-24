//! `sctl net`: the network tools a command, runbook or recovery action needs,
//! the same on every unit whatever the firmware ships (ADR-006).
//!
//! - `sctl net snmp get|walk|set`: SNMP v1/v2c
//! - `sctl net http`: one request through the agent's own TLS stack, with a
//!   cookie jar and redirects
//! - `sctl net tcp`: does a port answer
//!
//! They send only on the unit's LAN side, bound to the interface the guard
//! approved, and with `--expect-mac` only to the device holding that MAC
//! (ADR-012, [`guard`]). Every tool prints one JSON object on stdout and
//! exits 0 when the request did what it was asked, 1 when it failed (the
//! object says why), 2 on a usage error, 3 when the guard refused it: the
//! request never left, at most a name lookup and the ARP request did, and
//! `refused` says why. The SNMP community comes from `SCTL_SNMP_COMMUNITY`
//! only, so the shell puts it in the tool's environment and in no argv; no
//! output and no usage error carries it.

pub mod guard;
pub mod http;
pub mod snmp;
pub mod tcp;

use std::path::PathBuf;
use std::sync::OnceLock;

pub use guard::AGENT_CONFIG_ENV;

/// Exit code of a refused request: nothing was sent.
pub const EXIT_REFUSED: i32 = 3;

/// The config file the agent loaded, once it says so at start: `Some(None)`
/// when it loaded none.
static AGENT_CONFIG: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Called once by the agent at start: its Infra checks' guard reads the LTE
/// bearer `config` names, and every command it runs gets `file` (the
/// absolute path of the config it loaded) as `SCTL_AGENT_CONFIG`, or none
/// when it loaded none (ADR-012 decision 2).
pub fn declare_agent(config: &crate::config::Config, file: Option<PathBuf>) {
    guard::set_agent_lte(guard::Lte::from_config(Some(config)));
    let _ = AGENT_CONFIG.set(file);
}

/// Give a command the agent runs (exec, a session or its jobs, an Infra
/// check or recovery) the agent's config as `SCTL_AGENT_CONFIG`. Set after
/// the caller's environment, so the agent's own wins. A process that never
/// declared itself the agent (a test) leaves the environment as it is.
pub fn pass_agent_config(cmd: &mut tokio::process::Command) {
    apply_agent_config(cmd, AGENT_CONFIG.get());
}

fn apply_agent_config(cmd: &mut tokio::process::Command, declared: Option<&Option<PathBuf>>) {
    match declared {
        Some(Some(path)) => {
            cmd.env(AGENT_CONFIG_ENV, path);
        }
        Some(None) => {
            cmd.env_remove(AGENT_CONFIG_ENV);
        }
        None => {}
    }
}

pub fn help() -> &'static str {
    "Network tools for commands, runbooks and recovery actions

Usage:
  sctl net snmp get <host> <oid>... [OPTIONS]
  sctl net snmp walk <host> <oid> [--limit N] [OPTIONS]
  sctl net snmp set <host> <oid> <i|s> <value> [OPTIONS]
  sctl net http <url> [OPTIONS]
  sctl net tcp <host> <port> [OPTIONS]

They send only on this unit's LAN side: a destination the kernel routes
with no gateway, by an interface that is up, takes ARP, is not
point-to-point, is not the LTE bearer and holds an IPv4 address. The socket
is bound to that interface and its address. A destination the unit holds
itself passes. Broadcast, multicast and IPv6 destinations are refused.

Options for every tool:
      --dev <IFACE>        Send by this LAN-side interface holding an address
                           (a bridge port is named by its bridge). The
                           destination must lie under a route through it
                           with no gateway: its network, or a host route.
      --expect-mac <MAC>   Send only when every answer to one ARP request for
                           the destination, and the kernel's neighbour entry,
                           name this MAC (aa:bb:cc:dd:ee:ff or aa-bb-cc-dd-ee-ff)
      --config <PATH>      The agent's config, for its [lte] interface
                           (default: $SCTL_AGENT_CONFIG, which the agent sets
                           for the commands it runs; never ./sctl.toml)
      --timeout <MS>       snmp: per request (default 3000); http: default
                           10000; tcp: default 3000

HTTP options:
  -X <METHOD>              Method (default GET, POST with -d or -F)
  -H <'Name: value'>       Header, repeatable
  -d <BODY>                Raw body
  -F <name=value>          Form field, repeatable (urlencoded)
  -u <user:password>       Basic auth
  -b <FILE>                Cookie jar, read before and written after (0600)
      --max-redirects <N>  Redirects to follow (default 5); each passes the
                           checks above. A redirect to another scheme, host or
                           port drops the Basic auth, the headers and the body,
                           and is not followed when it would send the body
                           again (a 307 or 308, or a 301 or 302 after a PUT),
                           nor with --expect-mac to another address.
      --pin <SHA256>       Expected certificate fingerprint
      --tofu               Trust and record the certificate on first contact
      --fail               Exit 1 on an HTTP status of 400 or more
      --max-bytes <N>      Largest body read

SNMP options:
  -v <1|2c>                Protocol version (default 2c)
      --port <N>           UDP port (default 161)
  The community comes from the environment only:
    SCTL_SNMP_COMMUNITY=\"${SCTL_CRED_PASS_1}\" sctl net snmp ...
  Unset: public for get and walk; set needs one. Set but empty: refused.

Output: one JSON object. Exit 0 done, 1 failed (the object says why),
2 usage, 3 refused: the request never left (at most a name lookup and the
ARP request did), and `refused` is one of not_on_lan, no_address,
not_unicast, ipv6, own_address, mac_mismatch, no_answer, config_unreadable,
guard_failed."
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
        Some(_) => usage("unknown net tool: snmp, http or tcp"),
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

    use super::guard::{self, Approved, Guard, Mac, Refusal, Stop};
    use super::http::{form_body, request, HttpError, HttpRequest, DEFAULT_MAX_REDIRECTS};
    use super::snmp::{self, Session, SetValue, Version};
    use super::{print, usage, EXIT_REFUSED};

    /// The environment form the community takes, shown without its value.
    const COMMUNITY_FORM: &str = "SCTL_SNMP_COMMUNITY=\"${SCTL_CRED_PASS_1}\" sctl net snmp ...";

    /// The options every tool takes.
    #[derive(Debug, Default)]
    struct Common {
        dev: Option<String>,
        expect_mac: Option<Mac>,
        config: Option<String>,
        timeout_ms: Option<u64>,
    }

    impl Common {
        /// Take `arg` (and its value from `it`) if it is one of the common
        /// options. `Ok(false)` when it is not one.
        fn take(
            &mut self,
            arg: &str,
            it: &mut std::slice::Iter<'_, String>,
        ) -> Result<bool, String> {
            let mut value = |name: &str| {
                it.next()
                    .cloned()
                    .ok_or_else(|| format!("{name} needs a value"))
            };
            match arg {
                "--dev" => {
                    let dev = value(arg)?;
                    if !interface_name(&dev) {
                        return Err("--dev must be an interface name".into());
                    }
                    self.dev = Some(dev);
                }
                "--expect-mac" => {
                    self.expect_mac = Some(Mac::parse(&value(arg)?).ok_or_else(|| {
                        "--expect-mac must be six hex bytes separated by : or -".to_string()
                    })?);
                }
                "--config" => {
                    let path = value(arg)?;
                    if path.is_empty() {
                        return Err("--config needs a path".into());
                    }
                    self.config = Some(path);
                }
                "--timeout" => {
                    self.timeout_ms = Some(
                        value(arg)?
                            .parse()
                            .map_err(|_| "--timeout must be milliseconds".to_string())?,
                    );
                }
                _ => return Ok(false),
            }
            Ok(true)
        }

        /// The guard these options make, or the refusal that the config
        /// could not be read.
        fn guard(&self, timeout: Duration, host: &str) -> Result<Guard, Box<Refusal>> {
            guard::for_tool(
                self.config.as_deref(),
                self.dev.clone(),
                self.expect_mac,
                timeout,
                host,
            )
            .map(|(guard, _)| guard)
        }
    }

    /// 1 to 15 bytes, none of them a slash, a space or a control.
    fn interface_name(name: &str) -> bool {
        !name.is_empty()
            && name.len() < libc::IF_NAMESIZE
            && name != "."
            && name != ".."
            && name
                .bytes()
                .all(|b| b.is_ascii_graphic() && b != b'/' && b != b':')
    }

    /// The usage error for an option the tool does not take. An option's
    /// name is shown, never a value: a `--name=value` shows its name only,
    /// and a short option with something attached (`-xVALUE`) shows none.
    fn unknown_option(arg: &str) -> String {
        if is_community_option(arg) {
            return community_refused();
        }
        let name = match arg.strip_prefix("--") {
            Some(long) => long.split_once('=').map_or(long, |(name, _)| name),
            None if arg.len() == 2 => &arg[1..],
            None => "",
        };
        let shaped = !name.is_empty()
            && name.len() <= 32
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
        if shaped {
            let dashes = if arg.starts_with("--") { "--" } else { "-" };
            format!("unknown option {dashes}{name}")
        } else {
            "an unknown option".to_string()
        }
    }

    /// `-c`, `--community`, or either with its value attached.
    fn is_community_option(arg: &str) -> bool {
        arg.starts_with("--community") || (arg.starts_with("-c") && !arg.starts_with("--"))
    }

    fn community_refused() -> String {
        format!(
            "-c and --community are not taken: the community comes from the environment, as {COMMUNITY_FORM}"
        )
    }

    /// Print a refusal, with `extra` fields (the URL, the port), and give
    /// its exit code.
    fn refused(refusal: &Refusal, extra: &[(&str, serde_json::Value)]) -> i32 {
        let mut out = refusal.to_json();
        for (key, value) in extra {
            out[*key] = value.clone();
        }
        print(&out);
        EXIT_REFUSED
    }

    /// Print why a request did not start, and give its exit code.
    fn stopped(stop: Stop, host: &str, extra: &[(&str, serde_json::Value)]) -> i32 {
        match stop {
            Stop::Refused(refusal) => refused(&refusal, extra),
            Stop::Failed(error) => {
                let mut out = json!({"ok": false, "host": host, "error": error});
                for (key, value) in extra {
                    out[*key] = value.clone();
                }
                print(&out);
                1
            }
        }
    }

    struct Parsed {
        positional: Vec<String>,
        common: Common,
        version: Version,
        port: u16,
        limit: usize,
    }

    fn parse(args: &[String]) -> Result<Parsed, String> {
        let mut p = Parsed {
            positional: Vec::new(),
            common: Common::default(),
            version: Version::V2c,
            port: 161,
            limit: 1000,
        };
        let mut it = args.iter();
        while let Some(a) = it.next() {
            if p.common.take(a, &mut it)? {
                continue;
            }
            let mut value = |name: &str| {
                it.next()
                    .cloned()
                    .ok_or_else(|| format!("{name} needs a value"))
            };
            match a.as_str() {
                other if is_community_option(other) => return Err(community_refused()),
                "-v" => {
                    p.version = match value("-v")?.as_str() {
                        "1" => Version::V1,
                        "2c" | "2" => Version::V2c,
                        _ => return Err("-v must be 1 or 2c".to_string()),
                    }
                }
                "--port" => {
                    p.port = value(a)?
                        .parse()
                        .map_err(|_| "--port must be a number".to_string())?;
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
                    return Err(unknown_option(other));
                }
                other => p.positional.push(other.to_string()),
            }
        }
        Ok(p)
    }

    /// The community `SCTL_SNMP_COMMUNITY` gives, `public` for a read
    /// when it is unset, or the usage error. Never echoed.
    pub(super) fn community(verb: &str, env: Option<std::ffi::OsString>) -> Result<String, String> {
        match env {
            Some(value) if value.is_empty() => Err(format!(
                "SCTL_SNMP_COMMUNITY is set but empty: the credential it names is not in this command's environment ({COMMUNITY_FORM})"
            )),
            Some(value) => value
                .into_string()
                .map_err(|_| "SCTL_SNMP_COMMUNITY is not text".to_string()),
            None if verb == "set" => Err(format!(
                "set needs a community, from the environment: {COMMUNITY_FORM}"
            )),
            None => Ok("public".to_string()),
        }
    }

    /// `tcp`'s arguments: a host, a port, and the common options; nothing
    /// else.
    fn parse_tcp(args: &[String]) -> Result<(String, u16, Common), String> {
        let mut common = Common::default();
        let mut positional = Vec::new();
        let mut it = args.iter();
        while let Some(a) = it.next() {
            if common.take(a, &mut it)? {
                continue;
            }
            if a.starts_with('-') && a.len() > 1 {
                return Err(unknown_option(a));
            }
            positional.push(a.clone());
        }
        match positional.as_slice() {
            [host, port] => {
                let port = port
                    .parse::<u16>()
                    .map_err(|_| "the port must be a number".to_string())?;
                Ok((host.clone(), port, common))
            }
            [] | [_] => Err("sctl net tcp <host> <port>".to_string()),
            _ => Err("sctl net tcp takes a host and a port, nothing else".to_string()),
        }
    }

    pub async fn tcp(args: &[String]) -> i32 {
        let (host, port, common) = match parse_tcp(args) {
            Ok(parsed) => parsed,
            Err(e) => return usage(&e),
        };
        let timeout = Duration::from_millis(common.timeout_ms.unwrap_or(3000).clamp(100, 60_000));
        let extra = [("port", json!(port))];
        let guard = match common.guard(timeout, &host) {
            Ok(guard) => guard,
            Err(refusal) => return refused(&refusal, &extra),
        };
        let approved = match guard.approve_host(&host).await {
            Ok(approved) => approved,
            Err(stop) => return stopped(stop, &host, &extra),
        };
        let out = match super::tcp::probe(&approved, port, timeout).await {
            Ok(out) => out,
            Err(e) => return refused(&approved.bind_refusal(&e), &extra),
        };
        let open = out.open;
        print(&json!({
            "ok": open,
            "host": host,
            "address": approved.address,
            "dev": approved.dev,
            "port": port,
            "open": out.open,
            "ms": out.ms,
            "error": out.error,
        }));
        i32::from(!open)
    }

    /// `http`'s arguments: one URL, its options and the common ones, in any
    /// order. The request they make, its body and method settled, the
    /// common options and `--fail`.
    fn parse_http(args: &[String]) -> Result<(HttpRequest, Common, bool), String> {
        let mut req = HttpRequest {
            max_redirects: DEFAULT_MAX_REDIRECTS,
            ..HttpRequest::default()
        };
        let mut common = Common::default();
        let mut form: Vec<(String, String)> = Vec::new();
        let mut fail = false;
        let mut it = args.iter();
        while let Some(a) = it.next() {
            if common.take(a, &mut it)? {
                continue;
            }
            let mut value = |name: &str| {
                it.next()
                    .cloned()
                    .ok_or_else(|| format!("{name} needs a value"))
            };
            match a.as_str() {
                "-X" | "--request" => req.method = value(a)?,
                "-H" | "--header" => {
                    let h = value(a)?;
                    let (k, v) = h
                        .split_once(':')
                        .ok_or_else(|| "a header must be 'Name: value'".to_string())?;
                    req.headers
                        .push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
                }
                "-d" | "--data" => req.body = Some(value(a)?),
                "-F" | "--form" => {
                    let f = value(a)?;
                    let (k, v) = f
                        .split_once('=')
                        .ok_or_else(|| "a form field must be name=value".to_string())?;
                    form.push((k.to_string(), v.to_string()));
                }
                "-u" | "--user" => req.basic = Some(value(a)?),
                "-b" | "--cookie-jar" => req.cookie_jar = Some(value(a)?),
                "--max-redirects" => {
                    req.max_redirects = value(a)?
                        .parse()
                        .map_err(|_| "--max-redirects must be a number".to_string())?;
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
                other if other.starts_with('-') => return Err(unknown_option(other)),
                other if req.url.is_empty() => req.url = other.to_string(),
                _ => return Err("sctl net http takes one URL".to_string()),
            }
        }
        if req.url.is_empty() {
            return Err("sctl net http <url>".to_string());
        }
        req.timeout_ms = common.timeout_ms;
        if !form.is_empty() {
            if req.body.is_some() {
                return Err("-d and -F cannot be combined".to_string());
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
        Ok((req, common, fail))
    }

    pub async fn http(args: &[String]) -> i32 {
        let (req, common, fail) = match parse_http(args) {
            Ok(parsed) => parsed,
            Err(e) => return usage(&e),
        };
        let timeout = Duration::from_millis(req.timeout_ms.unwrap_or(10_000));
        let extra = [("url", json!(req.url))];
        let host = super::http::host_of(&req.url);
        let (guard, config) = match guard::for_tool(
            common.config.as_deref(),
            common.dev.clone(),
            common.expect_mac,
            timeout,
            &host,
        ) {
            Ok(pair) => pair,
            Err(refusal) => return refused(&refusal, &extra),
        };
        // The pin store is the agent's: its state_dir, else the defaults'.
        let state_dir = config
            .or_else(|| crate::config::Config::from_toml(""))
            .map(|c| c.server.state_dir().to_string())
            .unwrap_or_default();
        match request(&state_dir, &req, &guard).await {
            Ok(out) => {
                let failed = (fail && out.status >= 400) || out.not_followed.is_some();
                let mut v = serde_json::to_value(&out).unwrap_or_default();
                v["ok"] = json!(!failed);
                print(&v);
                i32::from(failed)
            }
            Err(HttpError::Refused(refusal)) => refused(&refusal, &extra),
            Err(HttpError::Failed(e)) => {
                print(&json!({"ok": false, "url": req.url, "error": e}));
                1
            }
        }
    }

    pub async fn snmp(args: &[String]) -> i32 {
        let Some(verb) = args.first().map(String::as_str) else {
            return usage("sctl net snmp needs get, walk or set");
        };
        if !matches!(verb, "get" | "walk" | "set") {
            return usage("sctl net snmp needs get, walk or set");
        }
        let p = match parse(&args[1..]) {
            Ok(p) => p,
            Err(e) => return usage(&e),
        };
        let Some(host) = p.positional.first().cloned() else {
            return usage("a host is required");
        };
        let rest = &p.positional[1..];
        let value = match (verb, rest.len()) {
            ("get", 1..) | ("walk", 1) => None,
            ("set", 3) => match rest[1].as_str() {
                "i" => match rest[2].parse() {
                    Ok(n) => Some(SetValue::Integer(n)),
                    Err(_) => return usage("an i value must be an integer"),
                },
                "s" => Some(SetValue::Text(rest[2].clone())),
                _ => return usage("a set's type is i (integer) or s (string)"),
            },
            _ => return usage(&format!("wrong arguments for snmp {verb}")),
        };
        let community = match community(verb, std::env::var_os("SCTL_SNMP_COMMUNITY")) {
            Ok(c) => c,
            Err(e) => return usage(&e),
        };
        let timeout = Duration::from_millis(p.common.timeout_ms.unwrap_or(3000).clamp(200, 60_000));
        let guard = match p.common.guard(timeout, &host) {
            Ok(guard) => guard,
            Err(refusal) => return refused(&refusal, &[]),
        };
        let approved: Approved = match guard.approve_host(&host).await {
            Ok(approved) => approved,
            Err(stop) => return stopped(stop, &host, &[]),
        };
        let socket = match approved.udp(p.port).await {
            Ok(socket) => socket,
            Err(e) => return refused(&approved.bind_refusal(&e), &[]),
        };
        let mut session = Session::new(socket, community.into_bytes(), p.version, timeout);
        let result = match (verb, value) {
            ("set", Some(value)) => snmp::set(&mut session, &rest[0], &value).await,
            ("walk", _) => snmp::walk(&mut session, &rest[0], p.limit.max(1)).await,
            _ => snmp::get(&mut session, rest).await,
        };
        match result {
            Ok(bindings) => {
                print(&json!({
                    "ok": true,
                    "host": host,
                    "address": approved.address,
                    "dev": approved.dev,
                    "bindings": bindings,
                }));
                0
            }
            Err(e) => {
                print(&json!({
                    "ok": false,
                    "host": host,
                    "address": approved.address,
                    "dev": approved.dev,
                    "error": e.to_string(),
                }));
                1
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn args(list: &[&str]) -> Vec<String> {
            list.iter().map(ToString::to_string).collect()
        }

        const SECRET: &str = "s3cr3t-c0mmunity";

        #[test]
        fn the_community_is_refused_on_the_command_line_and_never_echoed() {
            for form in [
                vec!["-c", SECRET],
                vec!["--community", SECRET],
                vec!["--community=s3cr3t-c0mmunity"],
                vec!["-cs3cr3t-c0mmunity"],
            ] {
                let mut list = vec!["192.0.2.10", "1.3.6.1.2.1.1.1.0"];
                list.extend(form.iter().copied());
                let err = parse(&args(&list)).err().expect("refused");
                assert!(!err.contains(SECRET), "{err}");
                assert!(
                    err.contains("SCTL_SNMP_COMMUNITY=\"${SCTL_CRED_PASS_1}\" sctl net snmp ..."),
                    "{err}"
                );
            }
            // Before the positional arguments too.
            let err = parse(&args(&["-c", SECRET, "192.0.2.10"])).err().unwrap();
            assert!(!err.contains(SECRET), "{err}");
            let help = crate::net_tools::help();
            assert!(!help.contains("--community"));
            assert!(!help.contains("-c,"));
            for word in [
                "--dev",
                "--expect-mac",
                "--config",
                "3 refused",
                "not_on_lan",
            ] {
                assert!(help.contains(word), "{word}");
            }
        }

        #[test]
        fn the_community_comes_from_the_environment_only() {
            use std::ffi::OsString;
            assert_eq!(
                community("set", Some(OsString::from(SECRET))).unwrap(),
                SECRET
            );
            assert_eq!(community("get", None).unwrap(), "public");
            assert_eq!(community("walk", None).unwrap(), "public");
            let err = community("set", None).unwrap_err();
            assert!(err.contains("SCTL_SNMP_COMMUNITY="), "{err}");
            // A reference to a credential the command does not carry.
            for verb in ["get", "walk", "set"] {
                let err = community(verb, Some(OsString::new())).unwrap_err();
                assert!(err.contains("set but empty"), "{err}");
            }
        }

        #[test]
        fn no_usage_error_repeats_an_option_s_value() {
            for (list, value) in [
                (vec!["192.0.2.10", "1.3", "-v", "s3cr3t"], "s3cr3t"),
                (vec!["192.0.2.10", "1.3", "--port", "s3cr3t"], "s3cr3t"),
                (
                    vec!["192.0.2.10", "1.3", "--expect-mac", "s3cr3t"],
                    "s3cr3t",
                ),
                (vec!["192.0.2.10", "1.3", "--dev", "s3/cr3t"], "s3/cr3t"),
                (vec!["192.0.2.10", "1.3", "--timeout", "s3cr3t"], "s3cr3t"),
                (vec!["192.0.2.10", "1.3", "--secret=s3cr3t"], "s3cr3t"),
            ] {
                let err = parse(&args(&list)).err().expect("a usage error");
                assert!(!err.contains(value), "{err}");
            }
            let err = parse(&args(&["192.0.2.10", "--secret=s3cr3t"]))
                .err()
                .unwrap();
            assert_eq!(err, "unknown option --secret");
            let err = parse(&args(&["192.0.2.10", "-us3cr3t"])).err().unwrap();
            assert_eq!(err, "an unknown option", "an attached value is not shown");
            let err = parse(&args(&["192.0.2.10", "-x"])).err().unwrap();
            assert_eq!(err, "unknown option -x");
        }

        #[test]
        fn the_common_options_parse_anywhere() {
            let p = parse(&args(&[
                "--dev",
                "br-lan",
                "--expect-mac",
                "00-1B-FF-A0-0C-DE",
                "192.0.2.10",
                "1.3.6.1.2.1.1.5.0",
                "--config",
                "/etc/sctl/sctl.toml",
                "--timeout",
                "800",
                "-v",
                "1",
            ]))
            .unwrap();
            assert_eq!(p.positional, ["192.0.2.10", "1.3.6.1.2.1.1.5.0"]);
            assert_eq!(p.common.dev.as_deref(), Some("br-lan"));
            assert_eq!(
                p.common.expect_mac.map(|m| m.to_string()).as_deref(),
                Some("00:1b:ff:a0:0c:de")
            );
            assert_eq!(p.common.config.as_deref(), Some("/etc/sctl/sctl.toml"));
            assert_eq!(p.common.timeout_ms, Some(800));
            assert_eq!(p.version, Version::V1);
            // A negative integer is a set's value, not an option.
            let p = parse(&args(&["192.0.2.10", "1.3", "i", "-1"])).unwrap();
            assert_eq!(p.positional, ["192.0.2.10", "1.3", "i", "-1"]);
        }

        #[test]
        fn tcp_refuses_an_argument_it_does_not_know() {
            let (host, port, common) = parse_tcp(&args(&[
                "--dev",
                "eth1",
                "--expect-mac",
                "00:1b:ff:a0:0c:de",
                "192.0.2.10",
                "80",
            ]))
            .unwrap();
            assert_eq!((host.as_str(), port), ("192.0.2.10", 80));
            assert_eq!(common.dev.as_deref(), Some("eth1"));
            assert!(common.expect_mac.is_some());
            // On 0.6.11 these ran unguarded and could exit 0.
            for list in [
                vec!["192.0.2.10", "80", "--devv", "eth1"],
                vec!["192.0.2.10", "80", "extra"],
                vec!["192.0.2.10", "80", "-x"],
                vec!["192.0.2.10"],
                vec!["192.0.2.10", "http"],
            ] {
                assert!(parse_tcp(&args(&list)).is_err(), "{list:?}");
            }
            let err = parse_tcp(&args(&["192.0.2.10", "80", "s3cr3t"])).unwrap_err();
            assert!(!err.contains("s3cr3t"), "{err}");
        }

        #[test]
        fn http_takes_one_url_its_options_and_the_common_ones_anywhere() {
            let (req, common, fail) = parse_http(&args(&[
                "--dev",
                "br-lan",
                "http://192.0.2.10/login",
                "-F",
                "user=u",
                "--expect-mac",
                "02-00-00-00-00-0A",
                "-F",
                "pass=p",
                "--timeout",
                "4000",
                "--config",
                "/etc/sctl/sctl.toml",
                "-b",
                "/tmp/jar",
                "--fail",
            ]))
            .unwrap();
            assert_eq!(req.url, "http://192.0.2.10/login");
            assert_eq!(req.method, "POST", "a form is a POST");
            assert_eq!(req.body.as_deref(), Some("user=u&pass=p"));
            assert!(req.headers.contains(&(
                "content-type".to_string(),
                "application/x-www-form-urlencoded".to_string()
            )));
            assert_eq!(req.timeout_ms, Some(4000));
            assert_eq!(req.cookie_jar.as_deref(), Some("/tmp/jar"));
            assert_eq!(req.max_redirects, DEFAULT_MAX_REDIRECTS);
            assert_eq!(common.dev.as_deref(), Some("br-lan"));
            assert_eq!(
                common.expect_mac.map(|m| m.to_string()).as_deref(),
                Some("02:00:00:00:00:0a")
            );
            assert_eq!(common.config.as_deref(), Some("/etc/sctl/sctl.toml"));
            assert!(fail);

            let (req, common, fail) = parse_http(&args(&[
                "-X",
                "put",
                "-H",
                "X-Custom:  kept ",
                "http://192.0.2.10/x",
                "-d",
                "{}",
                "-u",
                "admin:p4ss",
                "--max-redirects",
                "0",
                "--max-bytes",
                "512",
                "--pin",
                "ab12",
                "--tofu",
            ]))
            .unwrap();
            assert_eq!(
                (req.method.as_str(), req.body.as_deref()),
                ("put", Some("{}"))
            );
            assert_eq!(req.headers, [("x-custom".to_string(), "kept".to_string())]);
            assert_eq!(req.basic.as_deref(), Some("admin:p4ss"));
            assert_eq!((req.max_redirects, req.max_bytes), (0, Some(512)));
            assert_eq!(req.pin_sha256.as_deref(), Some("ab12"));
            assert!(req.allow_tofu && !fail && common.dev.is_none());

            let (req, ..) = parse_http(&args(&["http://192.0.2.10/"])).unwrap();
            assert_eq!(
                (req.method.as_str(), req.body, req.timeout_ms),
                ("GET", None, None)
            );
        }

        #[test]
        fn no_http_usage_error_repeats_an_option_s_value() {
            const URL: &str = "http://192.0.2.10/";
            for (list, value) in [
                (vec![URL, "-c", SECRET], SECRET),
                (vec![URL, "--community", SECRET], SECRET),
                (vec![URL, "--community=s3cr3t-c0mmunity"], SECRET),
                (vec![URL, "-cs3cr3t-c0mmunity"], SECRET),
                (vec![URL, "--secret=s3cr3t"], "s3cr3t"),
                (vec![URL, "-us3cr3t"], "s3cr3t"),
                (vec!["-us3cr3t", URL], "s3cr3t"),
                (vec![URL, "-H", "s3cr3t"], "s3cr3t"),
                (vec![URL, "-F", "s3cr3t"], "s3cr3t"),
                (vec![URL, "--max-redirects", "s3cr3t"], "s3cr3t"),
                (vec![URL, "--max-bytes", "s3cr3t"], "s3cr3t"),
                (vec![URL, "--expect-mac", "s3cr3t"], "s3cr3t"),
                (vec![URL, "--timeout", "s3cr3t"], "s3cr3t"),
                (vec![URL, "--dev", "s3/cr3t"], "s3/cr3t"),
                (vec![URL, "http://192.0.2.11/s3cr3t"], "s3cr3t"),
                (vec![URL, "-d", "pass=s3cr3t", "-F", "user=u"], "s3cr3t"),
                (vec![URL, "-F", "pass=s3cr3t", "-d", "x"], "s3cr3t"),
            ] {
                let err = parse_http(&args(&list)).expect_err("a usage error");
                assert!(!err.contains(value), "{list:?}: {err}");
            }
            let err = |list: &[&str]| parse_http(&args(list)).expect_err("a usage error");
            assert!(err(&[URL, "-c", SECRET]).contains("SCTL_SNMP_COMMUNITY="));
            assert_eq!(err(&[URL, "--secret=s3cr3t"]), "unknown option --secret");
            assert_eq!(
                err(&[URL, "-us3cr3t"]),
                "an unknown option",
                "an attached value is not shown"
            );
            assert_eq!(
                err(&[URL, "http://192.0.2.11/"]),
                "sctl net http takes one URL"
            );
            assert_eq!(
                err(&[URL, "-d", "x", "-F", "a=b"]),
                "-d and -F cannot be combined"
            );
            assert_eq!(err(&[URL, "-H"]), "-H needs a value");
            assert_eq!(err(&["--fail"]), "sctl net http <url>");
        }

        #[test]
        fn interface_names_are_checked() {
            for good in ["eth1", "br-lan", "wwan0", "eth0.2", "lan1@x"] {
                assert!(interface_name(good), "{good}");
            }
            for bad in [
                "",
                "a/b",
                "sixteen-chars-xx",
                "with space",
                ".",
                "..",
                "a:b",
            ] {
                assert!(!interface_name(bad), "{bad:?}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::config::Config;

    /// Prints the variable; a constant script, no value is spliced in.
    const PRINT: &str = "printf %s \"$SCTL_AGENT_CONFIG\"";

    async fn child_sees(declared: Option<&Option<PathBuf>>) -> String {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg(PRINT)
            .env(AGENT_CONFIG_ENV, "/inherited.toml");
        apply_agent_config(&mut cmd, declared);
        String::from_utf8(cmd.output().await.unwrap().stdout).unwrap()
    }

    #[tokio::test]
    async fn a_child_gets_the_agent_s_config_or_none() {
        let file = Some(PathBuf::from("/etc/sctl/sctl.toml"));
        assert_eq!(child_sees(Some(&file)).await, "/etc/sctl/sctl.toml");
        assert_eq!(
            child_sees(Some(&None)).await,
            "",
            "an agent that loaded no file names none, whatever it inherited"
        );
        assert_eq!(
            child_sees(None).await,
            "/inherited.toml",
            "a process that is not the agent leaves the environment alone"
        );
    }

    /// Exec, a job, an Infra recovery and a custom_script check: each
    /// command the agent runs reads the agent's config, and a caller's own
    /// value does not replace it.
    #[tokio::test]
    async fn every_command_the_agent_runs_gets_its_config() {
        let file = std::env::temp_dir().join("sctl-agent-config-for-children.toml");
        declare_agent(&Config::from_toml("").unwrap(), Some(file.clone()));
        let want = file.display().to_string();

        let r = crate::shell::process::exec_command("/bin/sh", "/", PRINT, 5_000, None)
            .await
            .unwrap();
        assert_eq!(r.stdout, want, "exec");
        let elsewhere =
            HashMap::from([(AGENT_CONFIG_ENV.to_string(), "/elsewhere.toml".to_string())]);
        let r = crate::shell::process::exec_command("/bin/sh", "/", PRINT, 5_000, Some(&elsewhere))
            .await
            .unwrap();
        assert_eq!(r.stdout, want, "the agent's own wins");

        let job = crate::shell::process::spawn_command_pgroup("/bin/sh", "/", PRINT, None).unwrap();
        let out = job.wait_with_output().await.unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), want, "a job");

        let (_, out, _) = crate::infra::checks::exec_with_env(PRINT, &[], 5_000)
            .await
            .unwrap();
        assert_eq!(out, want, "a recovery");

        let check = crate::infra::checks::run_check(&crate::infra::CheckSpec::CustomScript {
            command: PRINT.into(),
            timeout_ms: Some(5_000),
        })
        .await;
        assert!(check.ok, "{}", check.detail);
        assert!(
            check.detail.ends_with(&want),
            "a custom_script check: {}",
            check.detail
        );
    }
}
