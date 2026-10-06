//! SNMP v1/v2c client for `sctl net snmp` and the Infra SNMP check (ADR-006).
//!
//! The protocol work is the `snmp2` crate's; this module adds what a runbook
//! and a check need: one timeout per request, values as JSON, walk bounded to
//! its subtree, and an agent error named by its RFC 3416 status.

use std::str::FromStr;
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value as Json};
use snmp2::{AsyncSession, Oid, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    V1,
    V2c,
}

#[derive(Debug, Clone)]
pub struct Target {
    pub host: String,
    pub port: u16,
    pub community: String,
    pub version: Version,
    pub timeout: Duration,
}

/// One variable binding, owned and ready to print.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Binding {
    pub oid: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub value: Json,
}

#[derive(Debug)]
pub enum SnmpError {
    Invalid(String),
    Timeout,
    Io(String),
    /// The agent answered with a non-zero error-status.
    Agent {
        status: u32,
        index: u32,
    },
}

impl std::fmt::Display for SnmpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(m) | Self::Io(m) => write!(f, "{m}"),
            Self::Timeout => write!(f, "no answer from the SNMP agent"),
            Self::Agent { status, index } => {
                write!(
                    f,
                    "the agent refused: {} (varbind {index})",
                    status_name(*status)
                )
            }
        }
    }
}

/// RFC 3416 error-status names.
pub fn status_name(status: u32) -> &'static str {
    match status {
        1 => "tooBig",
        2 => "noSuchName",
        3 => "badValue",
        4 => "readOnly",
        5 => "genErr",
        6 => "noAccess",
        7 => "wrongType",
        8 => "wrongLength",
        9 => "wrongEncoding",
        10 => "wrongValue",
        11 => "noCreation",
        12 => "inconsistentValue",
        13 => "resourceUnavailable",
        14 => "commitFailed",
        15 => "undoFailed",
        16 => "authorizationError",
        17 => "notWritable",
        18 => "inconsistentName",
        _ => "unknown error",
    }
}

/// A value to set: the two types a power bar, a switch port or a reboot
/// object takes.
#[derive(Debug, Clone, PartialEq)]
pub enum SetValue {
    Integer(i64),
    Text(String),
}

pub fn parse_oid(raw: &str) -> Result<Oid<'static>, SnmpError> {
    let trimmed = raw.trim().trim_start_matches('.');
    Oid::from_str(trimmed)
        .map(|o| o.to_owned())
        .map_err(|_| SnmpError::Invalid(format!("not a numeric OID: {raw}")))
}

fn bytes_json(bytes: &[u8]) -> Json {
    match std::str::from_utf8(bytes) {
        Ok(s)
            if s.chars()
                .all(|c| !c.is_control() || c == '\n' || c == '\r' || c == '\t') =>
        {
            json!(s)
        }
        _ => json!(bytes
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":")),
    }
}

fn binding(oid: &Oid<'_>, value: &Value<'_>) -> Binding {
    let (kind, value) = match value {
        Value::Boolean(b) => ("boolean", json!(b)),
        Value::Null => ("null", Json::Null),
        Value::Integer(n) => ("integer", json!(n)),
        Value::OctetString(b) => ("string", bytes_json(b)),
        Value::ObjectIdentifier(o) => ("oid", json!(o.to_id_string())),
        Value::IpAddress(a) => (
            "ipaddress",
            json!(format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3])),
        ),
        Value::Counter32(n) => ("counter32", json!(n)),
        Value::Unsigned32(n) => ("gauge32", json!(n)),
        Value::Timeticks(n) => ("timeticks", json!(n)),
        Value::Counter64(n) => ("counter64", json!(n)),
        Value::Opaque(b) => ("opaque", bytes_json(b)),
        Value::NoSuchObject => ("noSuchObject", Json::Null),
        Value::NoSuchInstance => ("noSuchInstance", Json::Null),
        Value::EndOfMibView => ("endOfMibView", Json::Null),
        _ => ("unsupported", Json::Null),
    };
    Binding {
        oid: oid.to_id_string(),
        kind,
        value,
    }
}

async fn session(t: &Target) -> Result<AsyncSession, SnmpError> {
    let addr = format!("{}:{}", t.host, t.port);
    let community = t.community.as_bytes();
    let made = match t.version {
        Version::V1 => AsyncSession::new_v1(addr, community, 0).await,
        Version::V2c => AsyncSession::new_v2c(addr, community, 0).await,
    };
    made.map_err(|e| SnmpError::Io(format!("cannot open a socket to {}: {e}", t.host)))
}

/// Run one request with the target's timeout; the closure copies out what
/// it needs, because a response borrows the session's buffer.
macro_rules! request {
    ($t:expr, $call:expr) => {
        match tokio::time::timeout($t.timeout, $call).await {
            Err(_) => Err(SnmpError::Timeout),
            Ok(Err(e)) => Err(SnmpError::Io(format!("{e:?}"))),
            Ok(Ok(pdu)) => {
                if pdu.error_status != 0 {
                    Err(SnmpError::Agent {
                        status: pdu.error_status,
                        index: pdu.error_index,
                    })
                } else {
                    Ok(pdu
                        .varbinds
                        .map(|(o, v)| binding(&o, &v))
                        .collect::<Vec<_>>())
                }
            }
        }
    };
}

pub async fn get(t: &Target, oids: &[String]) -> Result<Vec<Binding>, SnmpError> {
    let mut sess = session(t).await?;
    let mut out = Vec::new();
    for raw in oids {
        let oid = parse_oid(raw)?;
        out.extend(request!(t, sess.get(&oid))?);
    }
    Ok(out)
}

/// Every binding under `root`, by GETNEXT, at most `limit`.
pub async fn walk(t: &Target, root: &str, limit: usize) -> Result<Vec<Binding>, SnmpError> {
    let root_oid = parse_oid(root)?;
    let prefix = root_oid.to_id_string();
    let mut sess = session(t).await?;
    let mut cursor = root_oid;
    let mut out = Vec::new();
    while out.len() < limit {
        let step = match request!(t, sess.getnext(&cursor)) {
            Ok(b) => b,
            // v1 ends a walk with noSuchName.
            Err(SnmpError::Agent { status: 2, .. }) => break,
            Err(e) => return Err(e),
        };
        let Some(b) = step.into_iter().next() else {
            break;
        };
        let inside = b.oid == prefix || b.oid.starts_with(&format!("{prefix}."));
        // An agent that answers the OID it was asked for would loop forever:
        // stop, as net-snmp does ("OID not increasing").
        if !inside || b.kind == "endOfMibView" || b.oid == cursor.to_id_string() {
            break;
        }
        cursor = parse_oid(&b.oid)?;
        out.push(b);
    }
    Ok(out)
}

pub async fn set(t: &Target, oid: &str, value: &SetValue) -> Result<Vec<Binding>, SnmpError> {
    let oid = parse_oid(oid)?;
    let mut sess = session(t).await?;
    let v = match value {
        SetValue::Integer(n) => Value::Integer(*n),
        SetValue::Text(s) => Value::OctetString(s.as_bytes()),
    };
    request!(t, sess.set(&[(&oid, v)]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oids_parse_with_or_without_the_leading_dot() {
        assert_eq!(
            parse_oid(".1.3.6.1.2.1.1.1.0").unwrap().to_id_string(),
            "1.3.6.1.2.1.1.1.0"
        );
        assert_eq!(
            parse_oid("1.3.6.1.4.1.3808").unwrap().to_id_string(),
            "1.3.6.1.4.1.3808"
        );
        assert!(parse_oid("sysDescr.0").is_err());
    }

    #[test]
    fn values_print_as_json() {
        let oid = parse_oid("1.3.6.1.2.1.1.1.0").unwrap();
        assert_eq!(binding(&oid, &Value::Integer(4)).value, json!(4));
        assert_eq!(
            binding(&oid, &Value::OctetString(b"PDU81005")).value,
            json!("PDU81005")
        );
        assert_eq!(
            binding(&oid, &Value::OctetString(&[0x00, 0x1b, 0xff])).value,
            json!("00:1b:ff")
        );
        assert_eq!(
            binding(&oid, &Value::IpAddress([192, 168, 0, 1])).value,
            json!("192.168.0.1")
        );
    }

    /// A loopback agent: answers each request with the request itself, its
    /// PDU tag turned into GetResponse (0xA2) and, when `error_status` is
    /// set, that status. Enough to prove the round trip through `snmp2`.
    async fn echo_agent(error_status: u8) -> (u16, tokio::task::JoinHandle<Vec<Vec<u8>>>) {
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = sock.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            let mut seen = Vec::new();
            let mut buf = [0u8; 2048];
            while let Ok(Ok((n, from))) =
                tokio::time::timeout(Duration::from_millis(500), sock.recv_from(&mut buf)).await
            {
                let mut msg = buf[..n].to_vec();
                seen.push(msg.clone());
                respond_in_place(&mut msg, error_status);
                sock.send_to(&msg, from).await.unwrap();
            }
            seen
        });
        (port, handle)
    }

    /// (offset of the value, length of the value) of the TLV at `at`.
    fn tlv(b: &[u8], at: usize) -> (usize, usize) {
        let first = b[at + 1] as usize;
        if first < 0x80 {
            (at + 2, first)
        } else {
            let n = first & 0x7f;
            let len = b[at + 2..at + 2 + n]
                .iter()
                .fold(0usize, |acc, x| (acc << 8) | *x as usize);
            (at + 2 + n, len)
        }
    }

    fn respond_in_place(b: &mut [u8], error_status: u8) {
        let (mut at, _) = tlv(b, 0); // message SEQUENCE
        for _ in 0..2 {
            let (v, l) = tlv(b, at); // version, community
            at = v + l;
        }
        b[at] = 0xA2; // the PDU: GetResponse
        let (inner, _) = tlv(b, at);
        let (v, l) = tlv(b, inner); // request-id
        let (status_at, _) = tlv(b, v + l); // error-status INTEGER, one byte
        b[status_at] = error_status;
    }

    fn target(port: u16, community: &str) -> Target {
        Target {
            host: "127.0.0.1".into(),
            port,
            community: community.into(),
            version: Version::V2c,
            timeout: Duration::from_millis(400),
        }
    }

    #[tokio::test]
    async fn get_round_trips_through_an_agent() {
        let (port, agent) = echo_agent(0).await;
        let got = get(&target(port, "public"), &["1.3.6.1.2.1.1.5.0".into()])
            .await
            .unwrap();
        assert_eq!(
            got,
            vec![Binding {
                oid: "1.3.6.1.2.1.1.5.0".into(),
                kind: "null",
                value: Json::Null
            }]
        );
        let seen = agent.await.unwrap();
        assert_eq!(seen.len(), 1);
        assert!(
            seen[0].windows(6).any(|w| w == b"public"),
            "the community is on the wire"
        );
    }

    #[tokio::test]
    async fn set_sends_the_value_and_reads_the_agent_answer() {
        let (port, agent) = echo_agent(0).await;
        let got = set(
            &target(port, "private"),
            "1.3.6.1.4.1.3808.1.1.3.3.1.1.0",
            &SetValue::Integer(4),
        )
        .await
        .unwrap();
        assert_eq!(got[0].value, json!(4));
        assert_eq!(got[0].kind, "integer");
        agent.await.unwrap();
    }

    #[tokio::test]
    async fn an_agent_refusal_is_named() {
        let (port, agent) = echo_agent(17).await;
        let err = set(
            &target(port, "private"),
            "1.3.6.1.2.1.1.5.0",
            &SetValue::Text("x".into()),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, SnmpError::Agent { status: 17, .. }), "{err}");
        agent.await.unwrap();
    }

    #[tokio::test]
    async fn a_walk_stops_on_an_agent_that_does_not_advance() {
        let (port, agent) = echo_agent(0).await;
        let got = walk(&target(port, "public"), "1.3.6.1.2.1.1", 50)
            .await
            .unwrap();
        assert!(got.len() <= 1, "{got:?}");
        agent.await.unwrap();
    }

    #[tokio::test]
    async fn silence_is_a_timeout() {
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = sock.local_addr().unwrap().port();
        let err = get(&target(port, "public"), &["1.3.6.1.2.1.1.5.0".into()])
            .await
            .unwrap_err();
        assert!(matches!(err, SnmpError::Timeout));
        drop(sock);
    }

    #[test]
    fn agent_errors_are_named() {
        assert_eq!(
            SnmpError::Agent {
                status: 17,
                index: 1
            }
            .to_string(),
            "the agent refused: notWritable (varbind 1)"
        );
    }
}
