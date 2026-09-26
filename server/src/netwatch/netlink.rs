//! The small slice of netlink that `netwatch` speaks: a socket, request
//! encoding, and parsing of link, address and route messages (rtnetlink),
//! plus the requests and replies generic netlink families such as
//! WireGuard's are asked through.
//!
//! Netlink messages are the kernel's own C structs in the host's byte order,
//! every part aligned to 4 bytes. Everything here goes through byte slices,
//! never a pointer cast to a struct, so the same code reads the same bytes
//! correctly on big-endian MIPS, mipsel, armv7, riscv64 and x86_64. The byte
//! order is carried as an [`Order`] value: production always passes
//! [`Order::NATIVE`] (what `to_ne_bytes` / `from_ne_bytes` do), and the tests
//! pass both orders to prove the big-endian reading on a little-endian host.
//!
//! Only attributes that Linux 2.6 already had are required, so a 3.3.8 kernel
//! works. `IFLA_CARRIER` is newer and optional.

use std::io;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::Duration;

use tokio::io::unix::AsyncFd;

// Kernel ABI values from linux/netlink.h, linux/rtnetlink.h, linux/if_link.h,
// linux/if_addr.h and linux/if.h. Typed here as the wire carries them; a test
// checks each against libc.
pub(crate) const NLMSG_HDRLEN: usize = 16;
pub(crate) const NLMSG_ERROR: u16 = 2;
pub(crate) const NLMSG_DONE: u16 = 3;
pub(crate) const NLMSG_OVERRUN: u16 = 4;
pub(crate) const NLM_F_REQUEST: u16 = 0x1;
pub(crate) const NLM_F_ACK: u16 = 0x4;
pub(crate) const NLM_F_DUMP_INTR: u16 = 0x10;
pub(crate) const NLM_F_REPLACE: u16 = 0x100;
pub(crate) const NLM_F_CREATE: u16 = 0x400;
pub(crate) const NLM_F_DUMP: u16 = 0x300;

pub(crate) const RTM_NEWLINK: u16 = 16;
pub(crate) const RTM_GETLINK: u16 = 18;
pub(crate) const RTM_NEWADDR: u16 = 20;
pub(crate) const RTM_GETADDR: u16 = 22;
pub(crate) const RTM_NEWROUTE: u16 = 24;
pub(crate) const RTM_DELROUTE: u16 = 25;
pub(crate) const RTM_GETROUTE: u16 = 26;

pub(crate) const RTMGRP_LINK: u32 = 0x1;
pub(crate) const RTMGRP_IPV4_IFADDR: u32 = 0x10;
pub(crate) const RTMGRP_IPV4_ROUTE: u32 = 0x40;

pub(crate) const IFLA_IFNAME: u16 = 3;
pub(crate) const IFLA_OPERSTATE: u16 = 16;
pub(crate) const IFLA_CARRIER: u16 = 33;
pub(crate) const IFA_ADDRESS: u16 = 1;
pub(crate) const IFA_LOCAL: u16 = 2;
pub(crate) const IFA_F_SECONDARY: u8 = 0x1;
pub(crate) const RTA_DST: u16 = 1;
pub(crate) const RTA_OIF: u16 = 4;
pub(crate) const RTA_GATEWAY: u16 = 5;
pub(crate) const RTA_PRIORITY: u16 = 6;
pub(crate) const RTA_PREFSRC: u16 = 7;
pub(crate) const RTA_TABLE: u16 = 15;

pub(crate) const AF_INET: u8 = 2;
pub(crate) const RT_TABLE_MAIN: u8 = 254;
pub(crate) const RTN_UNICAST: u8 = 1;
pub(crate) const RT_SCOPE_UNIVERSE: u8 = 0;
pub(crate) const RT_SCOPE_LINK: u8 = 253;
pub(crate) const RT_SCOPE_NOWHERE: u8 = 255;
pub(crate) const IFF_LOOPBACK: u32 = 0x8;

/// `struct ifinfomsg`: family, pad, type, index, flags, change.
const IFINFOMSG_LEN: usize = 16;
/// `struct ifaddrmsg`: family, prefixlen, flags, scope, index.
const IFADDRMSG_LEN: usize = 8;
/// `struct rtmsg`: family, dst_len, src_len, tos, table, protocol, scope, type, flags.
const RTMSG_LEN: usize = 12;
/// `struct rtattr`: len, type.
const RTA_HDRLEN: usize = 4;
/// Attribute type bits; the top two are the nested and byte-order flags.
const NLA_TYPE_MASK: u16 = 0x3fff;

/// Largest datagram read at once. The kernel sizes dump replies to the
/// reader's buffer but never above 32 KiB.
pub(crate) const RECV_BUF: usize = 32 * 1024;
/// The kernel answers a request inside the send; this only bounds a kernel
/// that never does.
const REPLY_TIMEOUT: Duration = Duration::from_secs(3);
/// A dump interrupted by a concurrent change is retried this many times.
const DUMP_ATTEMPTS: usize = 3;

/// NLMSG_ALIGN / RTA_ALIGN.
pub(crate) const fn align4(len: usize) -> usize {
    (len + 3) & !3
}

/// Byte order of the integers in a netlink message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Order {
    Little,
    Big,
}

impl Order {
    /// The order of the kernel this binary runs on.
    pub(crate) const NATIVE: Self = if cfg!(target_endian = "big") {
        Self::Big
    } else {
        Self::Little
    };

    fn read<const N: usize>(bytes: &[u8], at: usize) -> Option<[u8; N]> {
        bytes.get(at..at.checked_add(N)?)?.try_into().ok()
    }

    pub(crate) fn u16_at(self, bytes: &[u8], at: usize) -> Option<u16> {
        let raw = Self::read::<2>(bytes, at)?;
        Some(match self {
            Self::Little => u16::from_le_bytes(raw),
            Self::Big => u16::from_be_bytes(raw),
        })
    }

    pub(crate) fn u32_at(self, bytes: &[u8], at: usize) -> Option<u32> {
        let raw = Self::read::<4>(bytes, at)?;
        Some(match self {
            Self::Little => u32::from_le_bytes(raw),
            Self::Big => u32::from_be_bytes(raw),
        })
    }

    pub(crate) fn i32_at(self, bytes: &[u8], at: usize) -> Option<i32> {
        let raw = Self::read::<4>(bytes, at)?;
        Some(match self {
            Self::Little => i32::from_le_bytes(raw),
            Self::Big => i32::from_be_bytes(raw),
        })
    }

    pub(crate) fn u16_bytes(self, value: u16) -> [u8; 2] {
        match self {
            Self::Little => value.to_le_bytes(),
            Self::Big => value.to_be_bytes(),
        }
    }

    pub(crate) fn u32_bytes(self, value: u32) -> [u8; 4] {
        match self {
            Self::Little => value.to_le_bytes(),
            Self::Big => value.to_be_bytes(),
        }
    }
}

/// Build one request: the header, the family struct, then the attributes,
/// each padded to 4 bytes. The port id is left 0 for the kernel to fill.
pub(crate) fn encode(
    order: Order,
    kind: u16,
    flags: u16,
    seq: u32,
    family_struct: &[u8],
    attrs: &[(u16, &[u8])],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(&[0; 4]); // length, written last
    out.extend_from_slice(&order.u16_bytes(kind));
    out.extend_from_slice(&order.u16_bytes(flags));
    out.extend_from_slice(&order.u32_bytes(seq));
    out.extend_from_slice(&order.u32_bytes(0));
    out.extend_from_slice(family_struct);
    out.resize(align4(out.len()), 0);
    for (attr, data) in attrs {
        out.extend_from_slice(&order.u16_bytes((RTA_HDRLEN + data.len()) as u16));
        out.extend_from_slice(&order.u16_bytes(*attr));
        out.extend_from_slice(data);
        out.resize(align4(out.len()), 0);
    }
    let len = order.u32_bytes(out.len() as u32);
    out[..4].copy_from_slice(&len);
    out
}

/// `struct rtmsg` for an IPv4 route; the trailing `rtm_flags` is zero.
pub(crate) fn rtmsg(dst_len: u8, table: u8, protocol: u8, scope: u8, kind: u8) -> [u8; RTMSG_LEN] {
    [
        AF_INET, dst_len, 0, 0, table, protocol, scope, kind, 0, 0, 0, 0,
    ]
}

/// The three dumps a network snapshot is made of, with the message type each
/// answers with. Links are dumped for every family (`AF_UNSPEC`); addresses
/// and routes for IPv4 only. A route dump covers every table (per-table
/// filtering needs a 4.20 kernel), so the reader keeps the main table.
pub(crate) fn dump_requests() -> [(u16, u16, Vec<u8>); 3] {
    let mut addr = vec![0; IFADDRMSG_LEN];
    addr[0] = AF_INET;
    [
        (RTM_GETLINK, RTM_NEWLINK, vec![0; IFINFOMSG_LEN]),
        (RTM_GETADDR, RTM_NEWADDR, addr),
        (RTM_GETROUTE, RTM_NEWROUTE, rtmsg(0, 0, 0, 0, 0).to_vec()),
    ]
}

/// One netlink message inside a datagram.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Message<'a> {
    pub kind: u16,
    pub flags: u16,
    pub seq: u32,
    pub payload: &'a [u8],
}

/// The messages of one datagram, in order. Stops at the first header whose
/// length does not fit, so a short or corrupt read yields what came before it.
pub(crate) struct Messages<'a> {
    rest: &'a [u8],
    order: Order,
}

impl<'a> Messages<'a> {
    pub(crate) fn new(datagram: &'a [u8], order: Order) -> Self {
        Self {
            rest: datagram,
            order,
        }
    }
}

impl<'a> Iterator for Messages<'a> {
    type Item = Message<'a>;

    fn next(&mut self) -> Option<Message<'a>> {
        let len = self.order.u32_at(self.rest, 0)? as usize;
        if len < NLMSG_HDRLEN || len > self.rest.len() {
            self.rest = &[];
            return None;
        }
        let message = Message {
            kind: self.order.u16_at(self.rest, 4)?,
            flags: self.order.u16_at(self.rest, 6)?,
            seq: self.order.u32_at(self.rest, 8)?,
            payload: &self.rest[NLMSG_HDRLEN..len],
        };
        self.rest = &self.rest[align4(len).min(self.rest.len())..];
        Some(message)
    }
}

/// The attributes after a family struct, as (type, data).
pub(crate) struct Attrs<'a> {
    rest: &'a [u8],
    order: Order,
}

impl<'a> Attrs<'a> {
    pub(crate) fn new(bytes: &'a [u8], order: Order) -> Self {
        Self { rest: bytes, order }
    }
}

impl<'a> Iterator for Attrs<'a> {
    type Item = (u16, &'a [u8]);

    fn next(&mut self) -> Option<(u16, &'a [u8])> {
        let len = usize::from(self.order.u16_at(self.rest, 0)?);
        if len < RTA_HDRLEN || len > self.rest.len() {
            self.rest = &[];
            return None;
        }
        let kind = self.order.u16_at(self.rest, 2)? & NLA_TYPE_MASK;
        let data = &self.rest[RTA_HDRLEN..len];
        self.rest = &self.rest[align4(len).min(self.rest.len())..];
        Some((kind, data))
    }
}

fn ipv4(data: &[u8]) -> Option<Ipv4Addr> {
    let octets: [u8; 4] = data.get(..4)?.try_into().ok()?;
    Some(Ipv4Addr::from(octets))
}

fn c_string(data: &[u8]) -> String {
    let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    String::from_utf8_lossy(&data[..end]).into_owned()
}

/// An `RTM_NEWLINK` message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Link {
    pub index: u32,
    pub flags: u32,
    pub name: String,
    /// `IF_OPER_*`; 0 (unknown) when the attribute is absent.
    pub operstate: u8,
    /// `IFLA_CARRIER`, which kernels before 3.4 do not send.
    pub carrier: Option<bool>,
}

pub(crate) fn parse_link(payload: &[u8], order: Order) -> Option<Link> {
    let body = payload.get(IFINFOMSG_LEN..)?;
    let mut link = Link {
        index: order.u32_at(payload, 4)?,
        flags: order.u32_at(payload, 8)?,
        name: String::new(),
        operstate: 0,
        carrier: None,
    };
    for (attr, data) in Attrs::new(body, order) {
        match attr {
            IFLA_IFNAME => link.name = c_string(data),
            IFLA_OPERSTATE => link.operstate = data.first().copied().unwrap_or(0),
            IFLA_CARRIER => link.carrier = data.first().map(|&c| c != 0),
            _ => {}
        }
    }
    (!link.name.is_empty()).then_some(link)
}

/// An IPv4 `RTM_NEWADDR` message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Addr {
    pub index: u32,
    pub prefix: u8,
    pub secondary: bool,
    pub local: Ipv4Addr,
}

pub(crate) fn parse_addr(payload: &[u8], order: Order) -> Option<Addr> {
    if *payload.first()? != AF_INET {
        return None;
    }
    let body = payload.get(IFADDRMSG_LEN..)?;
    // IFA_LOCAL is this host's address; on a point-to-point link IFA_ADDRESS
    // is the peer's, so it only stands in when IFA_LOCAL is absent.
    let (mut local, mut address) = (None, None);
    for (attr, data) in Attrs::new(body, order) {
        match attr {
            IFA_LOCAL => local = ipv4(data),
            IFA_ADDRESS => address = ipv4(data),
            _ => {}
        }
    }
    Some(Addr {
        index: order.u32_at(payload, 4)?,
        prefix: payload[1],
        secondary: payload[2] & IFA_F_SECONDARY != 0,
        local: local.or(address)?,
    })
}

/// An IPv4 `RTM_NEWROUTE` message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Route {
    pub dst_len: u8,
    /// `RTA_TABLE` when present (tables above 255), else `rtm_table`.
    pub table: u32,
    pub protocol: u8,
    pub scope: u8,
    pub kind: u8,
    pub dst: Option<Ipv4Addr>,
    pub gateway: Option<Ipv4Addr>,
    pub oif: Option<u32>,
    /// `RTA_PRIORITY`, the metric; 0 when absent.
    pub priority: u32,
    /// `RTA_PREFSRC`: in the answer to a route lookup, the source address
    /// the kernel chose.
    pub prefsrc: Option<Ipv4Addr>,
}

pub(crate) fn parse_route(payload: &[u8], order: Order) -> Option<Route> {
    if *payload.first()? != AF_INET {
        return None;
    }
    let body = payload.get(RTMSG_LEN..)?;
    let mut route = Route {
        dst_len: payload[1],
        table: u32::from(payload[4]),
        protocol: payload[5],
        scope: payload[6],
        kind: payload[7],
        dst: None,
        gateway: None,
        oif: None,
        priority: 0,
        prefsrc: None,
    };
    for (attr, data) in Attrs::new(body, order) {
        match attr {
            RTA_DST => route.dst = ipv4(data),
            RTA_GATEWAY => route.gateway = ipv4(data),
            RTA_PREFSRC => route.prefsrc = ipv4(data),
            RTA_OIF => route.oif = order.u32_at(data, 0),
            RTA_PRIORITY => route.priority = order.u32_at(data, 0).unwrap_or(0),
            RTA_TABLE => {
                if let Some(table) = order.u32_at(data, 0) {
                    route.table = table;
                }
            }
            _ => {}
        }
    }
    Some(route)
}

/// A kernel error code (negative errno) as an `io::Error`.
fn errno(negative: i32) -> io::Error {
    io::Error::from_raw_os_error(negative.wrapping_neg())
}

/// The kernel's answer to request `seq` if this datagram holds it: `Ok` for
/// an acknowledgement, the errno for a refusal.
pub(crate) fn ack_in(datagram: &[u8], order: Order, seq: u32) -> Option<io::Result<()>> {
    Messages::new(datagram, order)
        .filter(|m| m.seq == seq && m.kind == NLMSG_ERROR)
        .find_map(|m| order.i32_at(m.payload, 0))
        .map(|code| if code == 0 { Ok(()) } else { Err(errno(code)) })
}

/// The kernel's answer to request `seq` if this datagram holds it: the
/// payload of its `reply` message, or the errno of a refusal. For requests
/// answered by one message rather than a dump.
pub(crate) fn reply_in(
    datagram: &[u8],
    order: Order,
    seq: u32,
    reply: u16,
) -> Option<io::Result<Vec<u8>>> {
    Messages::new(datagram, order)
        .filter(|m| m.seq == seq)
        .find_map(|m| match m.kind {
            NLMSG_ERROR => Some(match order.i32_at(m.payload, 0) {
                Some(code) if code < 0 => Err(errno(code)),
                _ => Err(io::Error::other(
                    "netlink request answered with an acknowledgement",
                )),
            }),
            kind if kind == reply => Some(Ok(m.payload.to_vec())),
            _ => None,
        })
}

/// Collects the replies to one dump request across datagrams.
pub(crate) struct DumpReply {
    seq: u32,
    kind: u16,
    interrupted: bool,
    payloads: Vec<Vec<u8>>,
}

impl DumpReply {
    pub(crate) fn new(seq: u32, kind: u16) -> Self {
        Self {
            seq,
            kind,
            interrupted: false,
            payloads: Vec::new(),
        }
    }

    /// Take one datagram. `Ok(true)` once the dump is complete. A dump the
    /// kernel flags as interrupted by a concurrent change (`NLM_F_DUMP_INTR`,
    /// from 3.1) ends in `ErrorKind::Interrupted` so the caller dumps again.
    pub(crate) fn feed(&mut self, datagram: &[u8], order: Order) -> io::Result<bool> {
        for message in Messages::new(datagram, order) {
            if message.seq != self.seq {
                continue;
            }
            if message.flags & NLM_F_DUMP_INTR != 0 {
                self.interrupted = true;
            }
            match message.kind {
                NLMSG_DONE => {
                    // The DONE payload carries the dump's own error, if any.
                    if let Some(code) = order.i32_at(message.payload, 0).filter(|&c| c < 0) {
                        return Err(errno(code));
                    }
                    if self.interrupted {
                        return Err(io::Error::new(
                            io::ErrorKind::Interrupted,
                            "netlink dump interrupted by a concurrent change",
                        ));
                    }
                    return Ok(true);
                }
                NLMSG_ERROR => {
                    return Err(match order.i32_at(message.payload, 0) {
                        Some(code) if code < 0 => errno(code),
                        _ => io::Error::other("netlink dump answered with an acknowledgement"),
                    });
                }
                NLMSG_OVERRUN => return Err(io::Error::other("netlink overrun")),
                kind if kind == self.kind => self.payloads.push(message.payload.to_vec()),
                _ => {}
            }
        }
        Ok(false)
    }

    pub(crate) fn into_payloads(self) -> Vec<Vec<u8>> {
        self.payloads
    }
}

/// Read one datagram without blocking: its length and whether it was cut to
/// fit `buf`.
fn recv_raw(fd: RawFd, buf: &mut [u8]) -> io::Result<(usize, bool)> {
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    // SAFETY: msghdr is plain data; all zero is "no address, no control".
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &raw mut iov;
    msg.msg_iovlen = 1;
    // SAFETY: msg points at one iovec covering buf; both outlive the call.
    let n = unsafe { libc::recvmsg(fd, &raw mut msg, libc::MSG_DONTWAIT) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((n.unsigned_abs(), msg.msg_flags & libc::MSG_TRUNC != 0))
}

/// A non-blocking netlink socket on the tokio reactor.
pub(crate) struct NlSocket {
    fd: AsyncFd<OwnedFd>,
    seq: u32,
}

impl NlSocket {
    /// Open a `NETLINK_ROUTE` socket joined to the multicast `groups` (0 for
    /// a socket that only sends requests and reads their replies). Needs a
    /// tokio runtime.
    pub(crate) fn open(groups: u32) -> io::Result<Self> {
        Self::open_protocol(libc::NETLINK_ROUTE, groups)
    }

    /// Open a socket of netlink `protocol` (`NETLINK_ROUTE`,
    /// `NETLINK_GENERIC`) joined to the multicast `groups`.
    pub(crate) fn open_protocol(protocol: libc::c_int, groups: u32) -> io::Result<Self> {
        // SAFETY: socket(2) with plain integer arguments; the result is checked.
        let raw = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                protocol,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: raw is a descriptor this call just created and nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: sockaddr_nl is plain data; all zero is a valid value.
        let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        addr.nl_groups = groups;
        // SAFETY: addr is a sockaddr_nl and its exact size is passed with it.
        let rc = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                (&raw const addr).cast(),
                std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            fd: AsyncFd::new(fd)?,
            seq: 0,
        })
    }

    pub(crate) fn next_seq(&mut self) -> u32 {
        self.seq = self.seq.wrapping_add(1);
        self.seq
    }

    async fn send(&self, request: &[u8]) -> io::Result<()> {
        loop {
            let mut guard = self.fd.writable().await?;
            let sent = guard.try_io(|fd| {
                // SAFETY: request is a live byte slice; pointer and length go together.
                let n = unsafe {
                    libc::send(fd.as_raw_fd(), request.as_ptr().cast(), request.len(), 0)
                };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n.unsigned_abs())
                }
            });
            match sent {
                Ok(Ok(n)) if n == request.len() => return Ok(()),
                Ok(Ok(_)) => return Err(io::Error::other("short netlink send")),
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => {}
            }
        }
    }

    /// Wait for one datagram: its length and whether it was cut to fit `buf`.
    pub(crate) async fn recv(&self, buf: &mut [u8]) -> io::Result<(usize, bool)> {
        loop {
            let mut guard = self.fd.readable().await?;
            if let Ok(result) = guard.try_io(|fd| recv_raw(fd.as_raw_fd(), buf)) {
                return result;
            }
        }
    }

    /// Read without waiting; `WouldBlock` when nothing is queued.
    pub(crate) fn try_recv(&self, buf: &mut [u8]) -> io::Result<(usize, bool)> {
        recv_raw(self.fd.as_raw_fd(), buf)
    }

    async fn recv_reply(&self, buf: &mut [u8]) -> io::Result<usize> {
        let (n, truncated) = tokio::time::timeout(REPLY_TIMEOUT, self.recv(buf))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "no netlink reply"))??;
        if truncated {
            return Err(io::Error::other(
                "netlink reply larger than the read buffer",
            ));
        }
        Ok(n)
    }

    /// Dump one object type: the payload of every `reply` message, in order.
    pub(crate) async fn dump(
        &mut self,
        request: u16,
        reply: u16,
        family_struct: &[u8],
        buf: &mut [u8],
    ) -> io::Result<Vec<Vec<u8>>> {
        self.dump_attrs(request, reply, family_struct, &[], buf)
            .await
    }

    /// [`Self::dump`] with attributes after the family struct, for a dump
    /// narrowed to one object (a generic netlink device by name).
    pub(crate) async fn dump_attrs(
        &mut self,
        request: u16,
        reply: u16,
        family_struct: &[u8],
        attrs: &[(u16, &[u8])],
        buf: &mut [u8],
    ) -> io::Result<Vec<Vec<u8>>> {
        let mut attempt = 1;
        loop {
            match self
                .dump_once(request, reply, family_struct, attrs, buf)
                .await
            {
                Err(e) if e.kind() == io::ErrorKind::Interrupted && attempt < DUMP_ATTEMPTS => {
                    attempt += 1;
                }
                result => return result,
            }
        }
    }

    async fn dump_once(
        &mut self,
        request: u16,
        reply: u16,
        family_struct: &[u8],
        attrs: &[(u16, &[u8])],
        buf: &mut [u8],
    ) -> io::Result<Vec<Vec<u8>>> {
        let seq = self.next_seq();
        let message = encode(
            Order::NATIVE,
            request,
            NLM_F_REQUEST | NLM_F_DUMP,
            seq,
            family_struct,
            attrs,
        );
        self.send(&message).await?;
        let mut replies = DumpReply::new(seq, reply);
        loop {
            let n = self.recv_reply(buf).await?;
            if replies.feed(&buf[..n], Order::NATIVE)? {
                return Ok(replies.into_payloads());
            }
        }
    }

    /// Send a request answered by one `reply` message (a route lookup, a
    /// generic netlink family by name) and return that message's payload,
    /// or the kernel's errno.
    pub(crate) async fn request(
        &mut self,
        kind: u16,
        family_struct: &[u8],
        attrs: &[(u16, &[u8])],
        reply: u16,
    ) -> io::Result<Vec<u8>> {
        let seq = self.next_seq();
        let message = encode(
            Order::NATIVE,
            kind,
            NLM_F_REQUEST,
            seq,
            family_struct,
            attrs,
        );
        self.send(&message).await?;
        let mut buf = vec![0; RECV_BUF];
        loop {
            let n = self.recv_reply(&mut buf).await?;
            if let Some(answer) = reply_in(&buf[..n], Order::NATIVE, seq, reply) {
                return answer;
            }
        }
    }

    /// Send a request carrying `NLM_F_ACK` with sequence `seq` and wait for
    /// the kernel's verdict.
    pub(crate) async fn acked(&self, request: &[u8], seq: u32) -> io::Result<()> {
        self.send(request).await?;
        let mut buf = vec![0; RECV_BUF];
        loop {
            let n = self.recv_reply(&mut buf).await?;
            if let Some(verdict) = ack_in(&buf[..n], Order::NATIVE, seq) {
                return verdict;
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Test-side builders, written field by field and independent of
    /// [`encode`], so a fixture in either byte order is built explicitly.
    pub(crate) fn u16b(order: Order, v: u16) -> Vec<u8> {
        match order {
            Order::Little => v.to_le_bytes().to_vec(),
            Order::Big => v.to_be_bytes().to_vec(),
        }
    }

    pub(crate) fn u32b(order: Order, v: u32) -> Vec<u8> {
        match order {
            Order::Little => v.to_le_bytes().to_vec(),
            Order::Big => v.to_be_bytes().to_vec(),
        }
    }

    pub(crate) fn attr(order: Order, kind: u16, data: &[u8]) -> Vec<u8> {
        let mut out = u16b(order, 4 + data.len() as u16);
        out.extend(u16b(order, kind));
        out.extend_from_slice(data);
        while !out.len().is_multiple_of(4) {
            out.push(0);
        }
        out
    }

    pub(crate) fn message(order: Order, kind: u16, flags: u16, seq: u32, body: &[u8]) -> Vec<u8> {
        let mut out = u32b(order, 16 + body.len() as u32);
        out.extend(u16b(order, kind));
        out.extend(u16b(order, flags));
        out.extend(u32b(order, seq));
        out.extend(u32b(order, 0x002e_6e89));
        out.extend_from_slice(body);
        out
    }

    /// `ifinfomsg` + IFLA_IFNAME, IFLA_OPERSTATE and, when given, IFLA_CARRIER.
    pub(crate) fn link_message(
        order: Order,
        index: u32,
        flags: u32,
        name: &str,
        operstate: u8,
        carrier: Option<u8>,
    ) -> Vec<u8> {
        let mut body = vec![0, 0];
        body.extend(u16b(order, 1)); // ARPHRD_ETHER
        body.extend(u32b(order, index));
        body.extend(u32b(order, flags));
        body.extend(u32b(order, 0));
        let mut ifname = name.as_bytes().to_vec();
        ifname.push(0);
        body.extend(attr(order, IFLA_IFNAME, &ifname));
        body.extend(attr(order, IFLA_OPERSTATE, &[operstate]));
        if let Some(c) = carrier {
            body.extend(attr(order, IFLA_CARRIER, &[c]));
        }
        message(order, RTM_NEWLINK, 2, 7, &body)
    }

    pub(crate) fn addr_message(
        order: Order,
        index: u32,
        local: [u8; 4],
        prefix: u8,
        flags: u8,
    ) -> Vec<u8> {
        let mut body = vec![AF_INET, prefix, flags, 0];
        body.extend(u32b(order, index));
        body.extend(attr(order, IFA_ADDRESS, &local));
        body.extend(attr(order, IFA_LOCAL, &local));
        message(order, RTM_NEWADDR, 2, 7, &body)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn route_message(
        order: Order,
        dst: Option<([u8; 4], u8)>,
        via: Option<[u8; 4]>,
        oif: u32,
        metric: u32,
        protocol: u8,
        table: u32,
    ) -> Vec<u8> {
        let dst_len = dst.map_or(0, |(_, len)| len);
        let scope = if via.is_some() { 0 } else { 253 };
        let mut body = vec![AF_INET, dst_len, 0, 0, 254, protocol, scope, RTN_UNICAST];
        body.extend(u32b(order, 0));
        body.extend(attr(order, RTA_TABLE, &u32b(order, table)));
        if let Some((octets, _)) = dst {
            body.extend(attr(order, RTA_DST, &octets));
        }
        body.extend(attr(order, RTA_PRIORITY, &u32b(order, metric)));
        if let Some(gw) = via {
            body.extend(attr(order, RTA_GATEWAY, &gw));
        }
        body.extend(attr(order, RTA_OIF, &u32b(order, oif)));
        message(order, RTM_NEWROUTE, 2, 7, &body)
    }

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// `default via 192.168.8.1 dev eth9 proto boot metric 10`, read from a
    /// little-endian x86_64 kernel (6.17) with a raw route dump.
    const CAPTURED_DEFAULT: &str = "3c0000001800020007000000896e2e0002000000fe0300010000000008000f00fe000000080006000a00000008000500c0a808010800040003000000"; // secret-scan: allow
    /// `174.138.114.209 via 192.168.8.1 dev eth9 proto static metric 40`.
    const CAPTURED_HOST: &str = "440000001800020007000000896e2e0002200000fe0400010000000008000f00fe00000008000100ae8a72d1080006002800000008000500c0a808010800040003000000"; // secret-scan: allow
    /// `192.168.8.2/24 brd 192.168.8.255 scope global eth9`, with its label,
    /// IFA_FLAGS and cache info.
    const CAPTURED_ADDR: &str = "500000001400020007000000896e2e00021880000300000008000100c0a8080208000200c0a80802090003006574683900000000080008008000000014000600ffffffffffffffffd9469403d9469403"; // secret-scan: allow

    const ORDERS: [Order; 2] = [Order::Little, Order::Big];

    #[test]
    fn constants_match_the_kernel_headers_in_libc() {
        assert_eq!(std::mem::size_of::<libc::nlmsghdr>(), NLMSG_HDRLEN);
        assert_eq!(i32::from(NLMSG_ERROR), libc::NLMSG_ERROR);
        assert_eq!(i32::from(NLMSG_DONE), libc::NLMSG_DONE);
        assert_eq!(i32::from(NLMSG_OVERRUN), libc::NLMSG_OVERRUN);
        assert_eq!(i32::from(NLM_F_REQUEST), libc::NLM_F_REQUEST);
        assert_eq!(i32::from(NLM_F_ACK), libc::NLM_F_ACK);
        assert_eq!(i32::from(NLM_F_DUMP_INTR), libc::NLM_F_DUMP_INTR);
        assert_eq!(i32::from(NLM_F_REPLACE), libc::NLM_F_REPLACE);
        assert_eq!(i32::from(NLM_F_CREATE), libc::NLM_F_CREATE);
        assert_eq!(i32::from(NLM_F_DUMP), libc::NLM_F_DUMP);
        assert_eq!(RTM_NEWLINK, libc::RTM_NEWLINK);
        assert_eq!(RTM_GETLINK, libc::RTM_GETLINK);
        assert_eq!(RTM_NEWADDR, libc::RTM_NEWADDR);
        assert_eq!(RTM_GETADDR, libc::RTM_GETADDR);
        assert_eq!(RTM_NEWROUTE, libc::RTM_NEWROUTE);
        assert_eq!(RTM_DELROUTE, libc::RTM_DELROUTE);
        assert_eq!(RTM_GETROUTE, libc::RTM_GETROUTE);
        assert_eq!(i64::from(RTMGRP_LINK), i64::from(libc::RTMGRP_LINK));
        assert_eq!(
            i64::from(RTMGRP_IPV4_IFADDR),
            i64::from(libc::RTMGRP_IPV4_IFADDR)
        );
        assert_eq!(
            i64::from(RTMGRP_IPV4_ROUTE),
            i64::from(libc::RTMGRP_IPV4_ROUTE)
        );
        assert_eq!(IFLA_IFNAME, libc::IFLA_IFNAME);
        assert_eq!(IFLA_OPERSTATE, libc::IFLA_OPERSTATE);
        assert_eq!(IFLA_CARRIER, libc::IFLA_CARRIER);
        assert_eq!(IFA_ADDRESS, libc::IFA_ADDRESS);
        assert_eq!(IFA_LOCAL, libc::IFA_LOCAL);
        assert_eq!(u32::from(IFA_F_SECONDARY), libc::IFA_F_SECONDARY);
        assert_eq!(RTA_DST, libc::RTA_DST);
        assert_eq!(RTA_OIF, libc::RTA_OIF);
        assert_eq!(RTA_GATEWAY, libc::RTA_GATEWAY);
        assert_eq!(RTA_PRIORITY, libc::RTA_PRIORITY);
        assert_eq!(RTA_PREFSRC, libc::RTA_PREFSRC);
        assert_eq!(RTA_TABLE, libc::RTA_TABLE);
        assert_eq!(i32::from(AF_INET), libc::AF_INET);
        assert_eq!(RT_TABLE_MAIN, libc::RT_TABLE_MAIN);
        assert_eq!(RTN_UNICAST, libc::RTN_UNICAST);
        assert_eq!(RT_SCOPE_UNIVERSE, libc::RT_SCOPE_UNIVERSE);
        assert_eq!(RT_SCOPE_LINK, libc::RT_SCOPE_LINK);
        assert_eq!(RT_SCOPE_NOWHERE, libc::RT_SCOPE_NOWHERE);
        assert_eq!(i64::from(IFF_LOOPBACK), i64::from(libc::IFF_LOOPBACK));
    }

    #[test]
    fn the_builders_reproduce_captured_kernel_bytes() {
        let default = route_message(Order::Little, None, Some([192, 168, 8, 1]), 3, 10, 3, 254);
        // The capture puts RTA_PRIORITY before RTA_GATEWAY; so do the builders.
        assert_eq!(default, unhex(CAPTURED_DEFAULT));
        let host = route_message(
            Order::Little,
            Some(([174, 138, 114, 209], 32)),
            Some([192, 168, 8, 1]),
            3,
            40,
            4,
            254,
        );
        assert_eq!(host, unhex(CAPTURED_HOST));
    }

    #[test]
    fn captured_messages_parse() {
        let bytes = unhex(CAPTURED_HOST);
        let messages: Vec<_> = Messages::new(&bytes, Order::Little).collect();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].kind, RTM_NEWROUTE);
        assert_eq!(messages[0].seq, 7);
        let route = parse_route(messages[0].payload, Order::Little).unwrap();
        assert_eq!(
            route,
            Route {
                dst_len: 32,
                table: 254,
                protocol: 4,
                scope: 0,
                kind: RTN_UNICAST,
                dst: Some(Ipv4Addr::new(174, 138, 114, 209)),
                gateway: Some(Ipv4Addr::new(192, 168, 8, 1)),
                oif: Some(3),
                priority: 40,
                prefsrc: None,
            }
        );

        let bytes = unhex(CAPTURED_ADDR);
        let message = Messages::new(&bytes, Order::Little).next().unwrap();
        assert_eq!(message.kind, RTM_NEWADDR);
        assert_eq!(
            parse_addr(message.payload, Order::Little).unwrap(),
            Addr {
                index: 3,
                prefix: 24,
                secondary: false,
                local: Ipv4Addr::new(192, 168, 8, 2),
            }
        );
    }

    #[test]
    fn both_byte_orders_parse_to_the_same_records() {
        for order in ORDERS {
            let link = link_message(order, 3, 0x1_1043, "eth1", 6, Some(1));
            let message = Messages::new(&link, order).next().unwrap();
            assert_eq!(message.kind, RTM_NEWLINK, "{order:?}");
            assert_eq!(
                parse_link(message.payload, order).unwrap(),
                Link {
                    index: 3,
                    flags: 0x1_1043,
                    name: "eth1".into(),
                    operstate: 6,
                    carrier: Some(true),
                },
                "{order:?}"
            );

            let addr = addr_message(order, 3, [10, 42, 0, 7], 24, 0);
            let message = Messages::new(&addr, order).next().unwrap();
            assert_eq!(
                parse_addr(message.payload, order).unwrap(),
                Addr {
                    index: 3,
                    prefix: 24,
                    secondary: false,
                    local: Ipv4Addr::new(10, 42, 0, 7),
                },
                "{order:?}"
            );

            let route = route_message(
                order,
                Some(([174, 138, 114, 209], 32)),
                Some([10, 42, 0, 1]),
                0x0102_0304,
                0x0a0b_0c0d,
                83,
                254,
            );
            let message = Messages::new(&route, order).next().unwrap();
            let parsed = parse_route(message.payload, order).unwrap();
            // Addresses are network order in either host order: never swapped.
            assert_eq!(
                parsed.dst,
                Some(Ipv4Addr::new(174, 138, 114, 209)),
                "{order:?}"
            );
            assert_eq!(
                parsed.gateway,
                Some(Ipv4Addr::new(10, 42, 0, 1)),
                "{order:?}"
            );
            assert_eq!(parsed.oif, Some(0x0102_0304), "{order:?}");
            assert_eq!(parsed.priority, 0x0a0b_0c0d, "{order:?}");
            assert_eq!(parsed.protocol, 83, "{order:?}");
        }
    }

    #[test]
    fn a_message_read_in_the_wrong_order_is_not_misread_as_valid() {
        let bytes = unhex(CAPTURED_DEFAULT);
        // 0x3c000000 is far longer than the datagram: nothing parses.
        assert_eq!(Messages::new(&bytes, Order::Big).count(), 0);
    }

    #[test]
    fn link_without_carrier_attribute_reads_none() {
        for order in ORDERS {
            let link = link_message(order, 2, 0x1003, "eth0.2", 2, None);
            let message = Messages::new(&link, order).next().unwrap();
            let parsed = parse_link(message.payload, order).unwrap();
            assert_eq!(parsed.carrier, None);
            assert_eq!(parsed.operstate, 2);
        }
    }

    #[test]
    fn several_messages_in_one_datagram_and_the_table_attribute() {
        for order in ORDERS {
            let mut datagram = route_message(order, None, Some([10, 0, 0, 1]), 2, 0, 4, 254);
            datagram.extend(route_message(
                order,
                None,
                Some([10, 0, 0, 1]),
                2,
                0,
                4,
                1000,
            ));
            let tables: Vec<u32> = Messages::new(&datagram, order)
                .map(|m| parse_route(m.payload, order).unwrap().table)
                .collect();
            assert_eq!(tables, vec![254, 1000]);
        }
    }

    #[test]
    fn truncated_and_corrupt_input_stops_without_panicking() {
        let whole = unhex(CAPTURED_HOST);
        for cut in 0..whole.len() {
            assert_eq!(Messages::new(&whole[..cut], Order::Little).count(), 0);
        }
        let payload = &whole[NLMSG_HDRLEN..];
        for cut in 0..payload.len() {
            let _ = parse_route(&payload[..cut], Order::Little);
        }
        // A header claiming less than its own size.
        let mut bad = whole.clone();
        bad[..4].copy_from_slice(&8u32.to_le_bytes());
        assert_eq!(Messages::new(&bad, Order::Little).count(), 0);
        // An attribute claiming less than its own header ends the attributes.
        let mut body = rtmsg(32, 254, 4, 0, 1).to_vec();
        body.extend([2, 0, 1, 0, 9, 9, 9, 9]);
        let parsed = parse_route(&body, Order::Little).unwrap();
        assert_eq!(parsed.dst, None);
        // An IPv6 payload is not an IPv4 route.
        body[0] = 10;
        assert!(parse_route(&body, Order::Little).is_none());
    }

    #[test]
    fn encode_lays_out_header_family_struct_and_padded_attributes() {
        for order in ORDERS {
            let family = rtmsg(32, 254, 83, 0, 1);
            let bytes = encode(
                order,
                RTM_NEWROUTE,
                0x0605,
                9,
                &family,
                &[(RTA_DST, &[1, 2, 3, 4]), (7, &[5])],
            );
            let mut expected = u32b(order, 44);
            expected.extend(u16b(order, 24));
            expected.extend(u16b(order, 0x0605));
            expected.extend(u32b(order, 9));
            expected.extend(u32b(order, 0));
            expected.extend(family);
            expected.extend(attr(order, RTA_DST, &[1, 2, 3, 4]));
            expected.extend(attr(order, 7, &[5]));
            assert_eq!(bytes, expected, "{order:?}");
        }
    }

    fn done(order: Order, seq: u32, code: i32) -> Vec<u8> {
        message(
            order,
            NLMSG_DONE,
            2,
            seq,
            &u32b(order, code.cast_unsigned()),
        )
    }

    fn error(order: Order, seq: u32, code: i32) -> Vec<u8> {
        let mut body = u32b(order, code.cast_unsigned());
        body.extend(vec![0; 16]);
        message(order, NLMSG_ERROR, 0, seq, &body)
    }

    #[test]
    fn dump_reply_collects_until_done_and_ignores_other_sequences() {
        for order in ORDERS {
            let mut reply = DumpReply::new(5, RTM_NEWROUTE);
            let mut first = route_message(order, None, Some([10, 0, 0, 1]), 2, 0, 4, 254);
            // A stale reply from an earlier request (seq 7) is skipped.
            first.extend(route_message(order, None, None, 9, 0, 4, 254));
            let fix_seq = |mut m: Vec<u8>, seq: u32| {
                m[8..12].copy_from_slice(&u32b(order, seq));
                m
            };
            let first = fix_seq(first, 5);
            assert!(!reply.feed(&first, order).unwrap());
            assert!(reply.feed(&done(order, 5, 0), order).unwrap());
            let payloads = reply.into_payloads();
            assert_eq!(payloads.len(), 1, "{order:?}");
            assert_eq!(parse_route(&payloads[0], order).unwrap().oif, Some(2));
        }
    }

    #[test]
    fn dump_reply_surfaces_errors_and_interruption() {
        for order in ORDERS {
            let mut reply = DumpReply::new(1, RTM_NEWLINK);
            let err = reply
                .feed(&error(order, 1, -libc::EPERM), order)
                .unwrap_err();
            assert_eq!(err.raw_os_error(), Some(libc::EPERM));

            let mut reply = DumpReply::new(1, RTM_NEWLINK);
            let err = reply
                .feed(&done(order, 1, -libc::EBUSY), order)
                .unwrap_err();
            assert_eq!(err.raw_os_error(), Some(libc::EBUSY));

            let mut reply = DumpReply::new(1, RTM_NEWLINK);
            let mut link = link_message(order, 1, 0, "eth0", 6, None);
            link[6..8].copy_from_slice(&u16b(order, 2 | NLM_F_DUMP_INTR));
            link[8..12].copy_from_slice(&u32b(order, 1));
            assert!(!reply.feed(&link, order).unwrap());
            let err = reply.feed(&done(order, 1, 0), order).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::Interrupted);
        }
    }

    #[test]
    fn a_single_reply_is_its_payload_or_the_errno() {
        for order in ORDERS {
            let mut answer = route_message(order, None, Some([10, 0, 0, 1]), 2, 0, 4, 254);
            answer[8..12].copy_from_slice(&u32b(order, 6));
            let payload = reply_in(&answer, order, 6, RTM_NEWROUTE).unwrap().unwrap();
            assert_eq!(
                parse_route(&payload, order).unwrap().oif,
                Some(2),
                "{order:?}"
            );
            // Another request's answer is not ours.
            assert!(reply_in(&answer, order, 7, RTM_NEWROUTE).is_none());
            let refused = reply_in(&error(order, 6, -libc::ENETUNREACH), order, 6, RTM_NEWROUTE)
                .unwrap()
                .unwrap_err();
            assert_eq!(refused.raw_os_error(), Some(libc::ENETUNREACH));
            // A bare acknowledgement answers nothing.
            assert!(reply_in(&error(order, 6, 0), order, 6, RTM_NEWROUTE)
                .unwrap()
                .is_err());
        }
    }

    #[test]
    fn a_route_lookup_answer_carries_the_chosen_source() {
        for order in ORDERS {
            let mut answer = route_message(
                order,
                Some(([174, 138, 114, 209], 32)),
                Some([10, 42, 0, 1]),
                3,
                0,
                0,
                254,
            );
            // The answer's RTA_PREFSRC, appended as the kernel does after
            // the others; the header's length grows with it.
            answer.extend(attr(order, RTA_PREFSRC, &[10, 42, 0, 7]));
            let len = answer.len() as u32;
            answer[..4].copy_from_slice(&u32b(order, len));
            let message = Messages::new(&answer, order).next().unwrap();
            let route = parse_route(message.payload, order).unwrap();
            assert_eq!(
                route.prefsrc,
                Some(Ipv4Addr::new(10, 42, 0, 7)),
                "{order:?}"
            );
            assert_eq!(
                route.gateway,
                Some(Ipv4Addr::new(10, 42, 0, 1)),
                "{order:?}"
            );
            assert_eq!(route.oif, Some(3), "{order:?}");
        }
    }

    #[test]
    fn ack_carries_the_kernel_errno() {
        for order in ORDERS {
            assert!(ack_in(&error(order, 3, 0), order, 3).unwrap().is_ok());
            let refused = ack_in(&error(order, 3, -libc::EEXIST), order, 3).unwrap();
            assert_eq!(refused.unwrap_err().raw_os_error(), Some(libc::EEXIST));
            // Another request's verdict is not ours.
            assert!(ack_in(&error(order, 4, 0), order, 3).is_none());
        }
    }
}
