//! SNMP v1/v2c client for `sctl net snmp` and the Infra SNMP check (ADR-006),
//! sending from a socket the guard bound and connected (ADR-012).
//!
//! The agent encodes the three requests it sends, GET, GETNEXT and SET, as
//! the short piece of BER they are, pinned byte for byte against what the
//! `snmp2` crate sends: `snmp2` 0.5.2 binds its own socket and keeps its
//! encoders to itself. `snmp2` still decodes the answer (`Pdu::from_bytes`),
//! and the agent checks what `snmp2` checks only inside its crate: the
//! request id, the community and the PDU type. Anything else that arrives
//! is ignored until the timeout. This module adds what a runbook and a
//! check need: one timeout per request, values as JSON, walk bounded to its
//! subtree, and an agent error named by its RFC 3416 status.

use std::str::FromStr;
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value as Json};
use snmp2::{MessageType, Oid, Pdu, Value};
use tokio::net::UdpSocket;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    V1,
    V2c,
}

impl Version {
    /// The message's version field: 0 for v1, 1 for v2c.
    fn wire(self) -> i64 {
        match self {
            Self::V1 => 0,
            Self::V2c => 1,
        }
    }
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

// ─── the agent's encoder ────────────────────────────────────────────────

/// The PDUs the agent sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Request {
    Get,
    GetNext,
    Set,
}

impl Request {
    fn tag(self) -> u8 {
        match self {
            Self::Get => snmp2::snmp::MSG_GET,
            Self::GetNext => snmp2::snmp::MSG_GET_NEXT,
            Self::Set => snmp2::snmp::MSG_SET,
        }
    }
}

/// A varbind's value as it is sent.
#[derive(Debug, Clone, Copy)]
enum Sent<'a> {
    Null,
    Integer(i64),
    OctetString(&'a [u8]),
}

const TAG_INTEGER: u8 = 0x02;
const TAG_OCTET_STRING: u8 = 0x04;
const TAG_NULL: u8 = 0x05;
const TAG_OID: u8 = 0x06;
const TAG_SEQUENCE: u8 = 0x30;

/// A BER length: short form under 128, else the long form in as few bytes
/// as it takes.
fn push_length(out: &mut Vec<u8>, len: usize) {
    if len < 128 {
        out.push(len as u8);
        return;
    }
    let bytes = len.to_be_bytes();
    let skip = bytes.iter().take_while(|b| **b == 0).count();
    out.push(0x80 | (bytes.len() - skip) as u8);
    out.extend_from_slice(&bytes[skip..]);
}

fn push_tlv(out: &mut Vec<u8>, tag: u8, content: &[u8]) {
    out.push(tag);
    push_length(out, content.len());
    out.extend_from_slice(content);
}

/// An INTEGER's content: two's complement in as few bytes as keep its sign.
fn integer_content(n: i64) -> Vec<u8> {
    let bytes = n.to_be_bytes();
    let mut start = 0;
    while start < bytes.len() - 1 {
        let (first, next) = (bytes[start], bytes[start + 1]);
        let redundant = (first == 0x00 && next & 0x80 == 0) || (first == 0xff && next & 0x80 != 0);
        if !redundant {
            break;
        }
        start += 1;
    }
    bytes[start..].to_vec()
}

/// One request message: `SEQUENCE { version, community, PDU { request-id,
/// error-status 0, error-index 0, varbinds } }`.
fn encode(
    version: Version,
    community: &[u8],
    request_id: i32,
    request: Request,
    varbinds: &[(&Oid<'_>, Sent<'_>)],
) -> Vec<u8> {
    let mut list = Vec::new();
    for (oid, value) in varbinds {
        let mut pair = Vec::new();
        push_tlv(&mut pair, TAG_OID, oid.as_bytes());
        match value {
            Sent::Null => push_tlv(&mut pair, TAG_NULL, &[]),
            Sent::Integer(n) => push_tlv(&mut pair, TAG_INTEGER, &integer_content(*n)),
            Sent::OctetString(bytes) => push_tlv(&mut pair, TAG_OCTET_STRING, bytes),
        }
        push_tlv(&mut list, TAG_SEQUENCE, &pair);
    }
    let mut pdu = Vec::new();
    push_tlv(
        &mut pdu,
        TAG_INTEGER,
        &integer_content(i64::from(request_id)),
    );
    push_tlv(&mut pdu, TAG_INTEGER, &integer_content(0));
    push_tlv(&mut pdu, TAG_INTEGER, &integer_content(0));
    push_tlv(&mut pdu, TAG_SEQUENCE, &list);
    let mut message = Vec::new();
    push_tlv(&mut message, TAG_INTEGER, &integer_content(version.wire()));
    push_tlv(&mut message, TAG_OCTET_STRING, community);
    push_tlv(&mut message, request.tag(), &pdu);
    let mut out = Vec::with_capacity(message.len() + 4);
    push_tlv(&mut out, TAG_SEQUENCE, &message);
    out
}

/// Whether `bytes` answer request `request_id`: a Response carrying the
/// community it was sent with. What `snmp2` checks inside its crate.
fn answers<'a>(bytes: &'a [u8], request_id: i32, community: &[u8]) -> Option<Pdu<'a>> {
    let pdu = Pdu::from_bytes(bytes).ok()?;
    (pdu.message_type == MessageType::Response
        && pdu.req_id == request_id
        && pdu.community == community)
        .then_some(pdu)
}

/// A random positive request id, so an answer to an earlier run never
/// matches.
fn first_request_id() -> i32 {
    let mut bytes = [0u8; 4];
    let random = ring::rand::SystemRandom::new();
    if ring::rand::SecureRandom::fill(&random, &mut bytes).is_err() {
        // A fixed start still works: ids only tell answers apart.
        return 1;
    }
    i32::from_be_bytes(bytes) & 0x7fff_ffff
}

// ─── the session ────────────────────────────────────────────────────────

/// Requests to one agent over one socket, which the caller bound and
/// connected where the guard approved: [`super::guard::Approved::udp`].
pub struct Session {
    socket: UdpSocket,
    community: Vec<u8>,
    version: Version,
    /// Per request.
    timeout: Duration,
    next_id: i32,
    /// One datagram, the largest UDP carries.
    buf: Vec<u8>,
}

impl Session {
    pub fn new(socket: UdpSocket, community: Vec<u8>, version: Version, timeout: Duration) -> Self {
        Self {
            socket,
            community,
            version,
            timeout,
            next_id: first_request_id(),
            buf: vec![0u8; 65_535],
        }
    }

    /// Send one request and wait for its answer: datagrams that are not one
    /// are ignored until the timeout.
    async fn request(
        &mut self,
        request: Request,
        varbinds: &[(&Oid<'_>, Sent<'_>)],
    ) -> Result<Vec<Binding>, SnmpError> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1) & 0x7fff_ffff;
        let message = encode(self.version, &self.community, id, request, varbinds);
        self.socket
            .send(&message)
            .await
            .map_err(|e| SnmpError::Io(format!("cannot send: {e}")))?;
        let (socket, community, buf) = (&self.socket, &self.community, &mut self.buf);
        let wait = async {
            loop {
                let n = socket
                    .recv(buf)
                    .await
                    .map_err(|e| SnmpError::Io(format!("cannot receive: {e}")))?;
                if let Some(pdu) = answers(&buf[..n], id, community) {
                    if pdu.error_status != 0 {
                        return Err(SnmpError::Agent {
                            status: pdu.error_status,
                            index: pdu.error_index,
                        });
                    }
                    return Ok(pdu
                        .varbinds
                        .map(|(o, v)| binding(&o, &v))
                        .collect::<Vec<_>>());
                }
            }
        };
        tokio::time::timeout(self.timeout, wait)
            .await
            .map_err(|_| SnmpError::Timeout)?
    }
}

pub async fn get(s: &mut Session, oids: &[String]) -> Result<Vec<Binding>, SnmpError> {
    let mut out = Vec::new();
    for raw in oids {
        let oid = parse_oid(raw)?;
        out.extend(s.request(Request::Get, &[(&oid, Sent::Null)]).await?);
    }
    Ok(out)
}

/// Every binding under `root`, by GETNEXT, at most `limit`.
pub async fn walk(s: &mut Session, root: &str, limit: usize) -> Result<Vec<Binding>, SnmpError> {
    let root_oid = parse_oid(root)?;
    let prefix = root_oid.to_id_string();
    let mut cursor = root_oid;
    let mut out = Vec::new();
    while out.len() < limit {
        let step = match s.request(Request::GetNext, &[(&cursor, Sent::Null)]).await {
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

pub async fn set(s: &mut Session, oid: &str, value: &SetValue) -> Result<Vec<Binding>, SnmpError> {
    let oid = parse_oid(oid)?;
    let sent = match value {
        SetValue::Integer(n) => Sent::Integer(*n),
        SetValue::Text(text) => Sent::OctetString(text.as_bytes()),
    };
    s.request(Request::Set, &[(&oid, sent)]).await
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

    #[test]
    fn integers_and_lengths_are_minimal_ber() {
        for (n, want) in [
            (0i64, vec![0x00]),
            (1, vec![0x01]),
            (127, vec![0x7f]),
            (128, vec![0x00, 0x80]),
            (256, vec![0x01, 0x00]),
            (-1, vec![0xff]),
            (-128, vec![0x80]),
            (-129, vec![0xff, 0x7f]),
            (i64::from(i32::MAX), vec![0x7f, 0xff, 0xff, 0xff]),
            (i64::MIN, vec![0x80, 0, 0, 0, 0, 0, 0, 0]),
        ] {
            assert_eq!(integer_content(n), want, "{n}");
        }
        for (len, want) in [
            (0usize, vec![0x00]),
            (127, vec![0x7f]),
            (128, vec![0x81, 0x80]),
            (255, vec![0x81, 0xff]),
            (256, vec![0x82, 0x01, 0x00]),
            (65_535, vec![0x82, 0xff, 0xff]),
        ] {
            let mut out = Vec::new();
            push_length(&mut out, len);
            assert_eq!(out, want, "{len}");
        }
    }

    /// One request as `snmp2` makes it.
    enum Theirs {
        Get(Oid<'static>),
        GetNext(Oid<'static>),
        SetInteger(Oid<'static>, i64),
        SetText(Oid<'static>, Vec<u8>),
    }

    /// What `snmp2` 0.5.2 sends for one request, caught on loopback: a
    /// socket that reads the request and never answers.
    async fn snmp2_sends(
        version: Version,
        community: &[u8],
        request_id: i32,
        theirs: Theirs,
    ) -> Vec<u8> {
        let catcher = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = catcher.local_addr().unwrap();
        let mut session = match version {
            Version::V1 => snmp2::AsyncSession::new_v1(addr, community, request_id).await,
            Version::V2c => snmp2::AsyncSession::new_v2c(addr, community, request_id).await,
        }
        .unwrap();
        // It waits for an answer that never comes, until aborted.
        let asking = tokio::spawn(async move {
            match theirs {
                Theirs::Get(oid) => drop(session.get(&oid).await),
                Theirs::GetNext(oid) => drop(session.getnext(&oid).await),
                Theirs::SetInteger(oid, n) => drop(session.set(&[(&oid, Value::Integer(n))]).await),
                Theirs::SetText(oid, text) => {
                    drop(session.set(&[(&oid, Value::OctetString(&text))]).await);
                }
            }
        });
        let mut buf = vec![0u8; 2048];
        let n = tokio::time::timeout(Duration::from_secs(5), catcher.recv(&mut buf))
            .await
            .expect("snmp2 sent its request")
            .unwrap();
        asking.abort();
        buf[..n].to_vec()
    }

    #[tokio::test]
    async fn the_encoder_sends_what_snmp2_sends_byte_for_byte() {
        let sys_descr = parse_oid("1.3.6.1.2.1.1.1.0").unwrap();
        // A power bar's outlet control: a long OID with sub-ids past 127.
        let outlet = parse_oid("1.3.6.1.4.1.318.1.1.12.3.3.1.1.4.8").unwrap();
        let long_name = vec![b'x'; 200];
        for (version, community, id) in [
            (Version::V2c, b"public".as_slice(), 0),
            (Version::V1, b"private".as_slice(), 1),
            (
                Version::V2c,
                b"c0mmunity-with-more-than-a-few-bytes".as_slice(),
                0x7fff_fffe,
            ),
            (Version::V1, b"".as_slice(), 300),
        ] {
            let theirs = snmp2_sends(version, community, id, Theirs::Get(sys_descr.clone())).await;
            let ours = encode(
                version,
                community,
                id,
                Request::Get,
                &[(&sys_descr, Sent::Null)],
            );
            assert_eq!(ours, theirs, "GET {version:?} {id}");

            let theirs = snmp2_sends(version, community, id, Theirs::GetNext(outlet.clone())).await;
            let ours = encode(
                version,
                community,
                id,
                Request::GetNext,
                &[(&outlet, Sent::Null)],
            );
            assert_eq!(ours, theirs, "GETNEXT {version:?} {id}");

            for n in [0i64, 1, 3, 128, -1, 65_536] {
                let theirs = snmp2_sends(
                    version,
                    community,
                    id,
                    Theirs::SetInteger(outlet.clone(), n),
                )
                .await;
                let ours = encode(
                    version,
                    community,
                    id,
                    Request::Set,
                    &[(&outlet, Sent::Integer(n))],
                );
                assert_eq!(ours, theirs, "SET i {n} {version:?} {id}");
            }

            for text in [b"on".as_slice(), long_name.as_slice()] {
                let theirs = snmp2_sends(
                    version,
                    community,
                    id,
                    Theirs::SetText(outlet.clone(), text.to_vec()),
                )
                .await;
                let ours = encode(
                    version,
                    community,
                    id,
                    Request::Set,
                    &[(&outlet, Sent::OctetString(text))],
                );
                assert_eq!(ours, theirs, "SET s {} bytes {version:?} {id}", text.len());
            }
        }
    }

    /// A loopback agent: answers each request with the request itself, its
    /// PDU tag turned into GetResponse (0xA2) and, when `error_status` is
    /// set, that status. Enough to prove the round trip.
    async fn echo_agent(error_status: u8) -> (u16, tokio::task::JoinHandle<Vec<Vec<u8>>>) {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
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

    /// The offset of the PDU's tag in a message.
    fn pdu_at(b: &[u8]) -> usize {
        let (mut at, _) = tlv(b, 0); // message SEQUENCE
        for _ in 0..2 {
            let (v, l) = tlv(b, at); // version, community
            at = v + l;
        }
        at
    }

    fn respond_in_place(b: &mut [u8], error_status: u8) {
        let at = pdu_at(b);
        b[at] = 0xA2; // the PDU: GetResponse
        let (inner, _) = tlv(b, at);
        let (v, l) = tlv(b, inner); // request-id
        let (status_at, _) = tlv(b, v + l); // error-status INTEGER, one byte
        b[status_at] = error_status;
    }

    async fn session(port: u16, community: &str) -> Session {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        socket.connect(("127.0.0.1", port)).await.unwrap();
        Session::new(
            socket,
            community.as_bytes().to_vec(),
            Version::V2c,
            Duration::from_millis(400),
        )
    }

    #[tokio::test]
    async fn get_round_trips_through_an_agent() {
        let (port, agent) = echo_agent(0).await;
        let mut s = session(port, "public").await;
        let got = get(&mut s, &["1.3.6.1.2.1.1.5.0".into()]).await.unwrap();
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
        let mut s = session(port, "private").await;
        let got = set(
            &mut s,
            "1.3.6.1.4.1.3808.1.1.3.3.1.1.0",
            &SetValue::Integer(4),
        )
        .await
        .unwrap();
        assert_eq!(got[0].value, json!(4));
        assert_eq!(got[0].kind, "integer");
        let text = set(&mut s, "1.3.6.1.2.1.1.5.0", &SetValue::Text("bar-7".into()))
            .await
            .unwrap();
        assert_eq!(text[0].value, json!("bar-7"));
        agent.await.unwrap();
    }

    #[tokio::test]
    async fn an_agent_refusal_is_named() {
        let (port, agent) = echo_agent(17).await;
        let mut s = session(port, "private").await;
        let err = set(&mut s, "1.3.6.1.2.1.1.5.0", &SetValue::Text("x".into()))
            .await
            .unwrap_err();
        assert!(matches!(err, SnmpError::Agent { status: 17, .. }), "{err}");
        agent.await.unwrap();
    }

    #[tokio::test]
    async fn a_walk_stops_on_an_agent_that_does_not_advance() {
        let (port, agent) = echo_agent(0).await;
        let mut s = session(port, "public").await;
        let got = walk(&mut s, "1.3.6.1.2.1.1", 50).await.unwrap();
        assert!(got.len() <= 1, "{got:?}");
        agent.await.unwrap();
    }

    #[tokio::test]
    async fn silence_is_a_timeout() {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = sock.local_addr().unwrap().port();
        let mut s = session(port, "public").await;
        let err = get(&mut s, &["1.3.6.1.2.1.1.5.0".into()])
            .await
            .unwrap_err();
        assert!(matches!(err, SnmpError::Timeout));
        drop(sock);
    }

    /// An agent that first answers with the wrong request id, the wrong
    /// community, the wrong PDU type and bytes that are no SNMP at all, and
    /// only then with the answer.
    #[tokio::test]
    async fn only_an_answer_to_the_request_is_taken() {
        let agent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = agent.local_addr().unwrap().port();
        let mut session_a = session(port, "public").await;
        let mut buf = [0u8; 2048];
        let serve = async {
            let (len, from) = agent.recv_from(&mut buf).await.unwrap();
            let request = buf[..len].to_vec();
            let mut good = request.clone();
            respond_in_place(&mut good, 0);
            // Another request id: its last byte changed.
            let mut wrong_id = good.clone();
            let (inner, _) = tlv(&wrong_id, pdu_at(&wrong_id));
            let (id_at, id_len) = tlv(&wrong_id, inner);
            wrong_id[id_at + id_len - 1] ^= 0x01;
            // Another community of the same length.
            let mut wrong_community = good.clone();
            let (community_at, community_len) = {
                let (message, _) = tlv(&wrong_community, 0);
                let (version_at, version_len) = tlv(&wrong_community, message);
                tlv(&wrong_community, version_at + version_len)
            };
            wrong_community[community_at..community_at + community_len].copy_from_slice(b"PUBLIC");
            // The request itself: a GetRequest, not a Response.
            let wrong_type = request;
            for reply in [
                wrong_id,
                wrong_community,
                wrong_type,
                b"no snmp here".to_vec(),
                good,
            ] {
                agent.send_to(&reply, from).await.unwrap();
            }
        };
        let oids = ["1.3.6.1.2.1.1.5.0".to_string()];
        let (got, ()) = tokio::join!(get(&mut session_a, &oids), serve);
        assert_eq!(got.unwrap()[0].oid, "1.3.6.1.2.1.1.5.0");

        // Without the good one: a timeout, whatever else arrived.
        let mut session_b = session(port, "public").await;
        let serve = async {
            let (len, from) = agent.recv_from(&mut buf).await.unwrap();
            let mut wrong_type = buf[..len].to_vec();
            let at = pdu_at(&wrong_type);
            wrong_type[at] = 0xA3; // a SetRequest
            agent.send_to(&wrong_type, from).await.unwrap();
        };
        let (got, ()) = tokio::join!(get(&mut session_b, &oids), serve);
        assert!(matches!(got, Err(SnmpError::Timeout)), "{got:?}");
    }

    #[tokio::test]
    async fn request_ids_differ_between_sessions_and_count_up() {
        let (port, agent) = echo_agent(0).await;
        let mut s = session(port, "public").await;
        let first = s.next_id;
        get(
            &mut s,
            &["1.3.6.1.2.1.1.5.0".into(), "1.3.6.1.2.1.1.6.0".into()],
        )
        .await
        .unwrap();
        assert_eq!(s.next_id, first.wrapping_add(2) & 0x7fff_ffff);
        assert!(first >= 0);
        agent.await.unwrap();
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
