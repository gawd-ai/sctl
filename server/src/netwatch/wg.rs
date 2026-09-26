//! The endpoints of a WireGuard interface's peers, read from the kernel as
//! `wg show <ifname> endpoints` does, over generic netlink rather than by
//! running `wg`.
//!
//! Two requests on one `NETLINK_GENERIC` socket: the controller resolves the
//! `wireguard` family to its id, then `WG_CMD_GET_DEVICE` dumps the device.
//! The kernel answers only with `CAP_NET_ADMIN`, and its answer includes the
//! interface's private key: it is read past, never kept, like every
//! attribute but the peers' endpoints.

use std::io;
use std::net::SocketAddr;

use super::netlink::{Attrs, NlSocket, Order, RECV_BUF};

// Kernel ABI values from linux/genetlink.h and linux/wireguard.h.
/// The generic netlink controller's fixed family id (`GENL_ID_CTRL`).
pub(crate) const GENL_ID_CTRL: u16 = 0x10;
pub(crate) const CTRL_CMD_GETFAMILY: u8 = 3;
pub(crate) const CTRL_ATTR_FAMILY_ID: u16 = 1;
pub(crate) const CTRL_ATTR_FAMILY_NAME: u16 = 2;
/// `struct genlmsghdr`: cmd, version, reserved.
const GENL_HDRLEN: usize = 4;
const WG_GENL_NAME: &[u8] = b"wireguard\0";
const WG_GENL_VERSION: u8 = 1;
const WG_CMD_GET_DEVICE: u8 = 0;
const WGDEVICE_A_IFNAME: u16 = 2;
const WGDEVICE_A_PEERS: u16 = 8;
const WGPEER_A_ENDPOINT: u16 = 4;
const AF_INET: u16 = 2;
const AF_INET6: u16 = 10;

/// `struct genlmsghdr` for `cmd` at `version`.
fn genl_header(cmd: u8, version: u8) -> [u8; GENL_HDRLEN] {
    [cmd, version, 0, 0]
}

/// The endpoint of every peer of WireGuard interface `ifname` that has one,
/// in the kernel's order. Fails with the kernel's errno: `ENOENT` when the
/// wireguard module is not loaded, `ENODEV` when there is no such
/// interface, `EPERM` without `CAP_NET_ADMIN`.
pub async fn endpoints(ifname: &str) -> io::Result<Vec<SocketAddr>> {
    let mut socket = NlSocket::open_protocol(libc::NETLINK_GENERIC, 0)?;
    let answer = socket
        .request(
            GENL_ID_CTRL,
            &genl_header(CTRL_CMD_GETFAMILY, 1),
            &[(CTRL_ATTR_FAMILY_NAME, WG_GENL_NAME)],
            GENL_ID_CTRL,
        )
        .await?;
    let family = family_id(&answer, Order::NATIVE)
        .ok_or_else(|| io::Error::other("the controller named no wireguard family id"))?;
    let mut name = ifname.as_bytes().to_vec();
    name.push(0);
    let mut buf = vec![0; RECV_BUF];
    let device = socket
        .dump_attrs(
            family,
            family,
            &genl_header(WG_CMD_GET_DEVICE, WG_GENL_VERSION),
            &[(WGDEVICE_A_IFNAME, &name)],
            &mut buf,
        )
        .await?;
    Ok(device
        .iter()
        .flat_map(|payload| peer_endpoints(payload, Order::NATIVE))
        .collect())
}

/// `CTRL_ATTR_FAMILY_ID` from the controller's answer.
pub(crate) fn family_id(payload: &[u8], order: Order) -> Option<u16> {
    Attrs::new(payload.get(GENL_HDRLEN..)?, order)
        .find(|(attr, _)| *attr == CTRL_ATTR_FAMILY_ID)
        .and_then(|(_, data)| order.u16_at(data, 0))
}

/// The peer endpoints in one message of a `WG_CMD_GET_DEVICE` dump. A device
/// with many peers spreads them over several messages.
pub(crate) fn peer_endpoints(payload: &[u8], order: Order) -> Vec<SocketAddr> {
    let Some(attrs) = payload.get(GENL_HDRLEN..) else {
        return Vec::new();
    };
    Attrs::new(attrs, order)
        .filter(|(attr, _)| *attr == WGDEVICE_A_PEERS)
        // Each peer is a nested attribute whose type is its position.
        .flat_map(|(_, peers)| Attrs::new(peers, order))
        .filter_map(|(_, peer)| {
            Attrs::new(peer, order)
                .find(|(attr, _)| *attr == WGPEER_A_ENDPOINT)
                .and_then(|(_, data)| sockaddr(data, order))
        })
        .collect()
}

/// A `struct sockaddr_in` or `sockaddr_in6`: the family in host order, the
/// port and address in network order.
fn sockaddr(data: &[u8], order: Order) -> Option<SocketAddr> {
    let port = u16::from_be_bytes(data.get(2..4)?.try_into().ok()?);
    match order.u16_at(data, 0)? {
        AF_INET => {
            let ip: [u8; 4] = data.get(4..8)?.try_into().ok()?;
            Some(SocketAddr::from((ip, port)))
        }
        AF_INET6 => {
            let ip: [u8; 16] = data.get(8..24)?.try_into().ok()?;
            Some(SocketAddr::from((ip, port)))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::super::netlink::tests::{attr, message, u16b, u32b};
    use super::super::netlink::{encode, Messages, NLM_F_DUMP, NLM_F_REQUEST};
    use super::*;

    const ORDERS: [Order; 2] = [Order::Little, Order::Big];
    const NLA_F_NESTED: u16 = 0x8000;

    #[test]
    fn constants_match_the_kernel_headers_in_libc() {
        assert_eq!(i32::from(GENL_ID_CTRL), libc::GENL_ID_CTRL);
        assert_eq!(i32::from(CTRL_CMD_GETFAMILY), libc::CTRL_CMD_GETFAMILY);
        assert_eq!(i32::from(CTRL_ATTR_FAMILY_ID), libc::CTRL_ATTR_FAMILY_ID);
        assert_eq!(
            i32::from(CTRL_ATTR_FAMILY_NAME),
            libc::CTRL_ATTR_FAMILY_NAME
        );
        assert_eq!(i32::from(AF_INET), libc::AF_INET);
        assert_eq!(i32::from(AF_INET6), libc::AF_INET6);
    }

    fn sockaddr_in(order: Order, ip: [u8; 4], port: u16) -> Vec<u8> {
        let mut out = u16b(order, AF_INET);
        out.extend(port.to_be_bytes());
        out.extend(ip);
        out.extend([0; 8]);
        out
    }

    fn sockaddr_in6(order: Order, ip: [u8; 16], port: u16) -> Vec<u8> {
        let mut out = u16b(order, AF_INET6);
        out.extend(port.to_be_bytes());
        out.extend(u32b(order, 0)); // flowinfo
        out.extend(ip);
        out.extend(u32b(order, 0)); // scope id
        out
    }

    /// One message of a `WG_CMD_GET_DEVICE` dump for wg0 with `peers`, each
    /// a list of peer attributes; a private key comes first, as the kernel
    /// sends it.
    fn device_message(order: Order, family: u16, peers: &[Vec<(u16, Vec<u8>)>]) -> Vec<u8> {
        let mut body = genl_header(WG_CMD_GET_DEVICE, WG_GENL_VERSION).to_vec();
        body.extend(attr(order, 1, &u32b(order, 9))); // WGDEVICE_A_IFINDEX
        body.extend(attr(order, WGDEVICE_A_IFNAME, b"wg0\0"));
        body.extend(attr(order, 3, &[0x5a; 32])); // WGDEVICE_A_PRIVATE_KEY
        let mut nested = Vec::new();
        for (index, peer) in peers.iter().enumerate() {
            let mut inner = Vec::new();
            for (kind, data) in peer {
                inner.extend(attr(order, *kind, data));
            }
            nested.extend(attr(order, index as u16 | NLA_F_NESTED, &inner));
        }
        body.extend(attr(order, WGDEVICE_A_PEERS | NLA_F_NESTED, &nested));
        message(order, family, 2, 4, &body)
    }

    #[test]
    fn the_controller_answer_names_the_family_id() {
        for order in ORDERS {
            let mut body = genl_header(1, 2).to_vec(); // CTRL_CMD_NEWFAMILY
            body.extend(attr(order, CTRL_ATTR_FAMILY_ID, &u16b(order, 0x1b)));
            body.extend(attr(order, CTRL_ATTR_FAMILY_NAME, WG_GENL_NAME));
            let bytes = message(order, GENL_ID_CTRL, 0, 1, &body);
            let msg = Messages::new(&bytes, order).next().unwrap();
            assert_eq!(family_id(msg.payload, order), Some(0x1b), "{order:?}");
            assert_eq!(family_id(&[1, 2], order), None);
        }
    }

    #[test]
    fn peers_give_their_endpoints_in_both_byte_orders() {
        for order in ORDERS {
            let peers = vec![
                vec![
                    (1, vec![0x11; 32]), // WGPEER_A_PUBLIC_KEY
                    (
                        WGPEER_A_ENDPOINT,
                        sockaddr_in(order, [174, 138, 114, 209], 51820),
                    ),
                ],
                // A peer that has not been heard from has no endpoint.
                vec![(1, vec![0x22; 32])],
                vec![
                    (
                        WGPEER_A_ENDPOINT,
                        sockaddr_in6(
                            order,
                            [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
                            443,
                        ),
                    ),
                    (1, vec![0x33; 32]),
                ],
            ];
            let bytes = device_message(order, 0x1b, &peers);
            let msg = Messages::new(&bytes, order).next().unwrap();
            assert_eq!(msg.kind, 0x1b);
            assert_eq!(
                peer_endpoints(msg.payload, order),
                vec![
                    SocketAddr::from((Ipv4Addr::new(174, 138, 114, 209), 51820)),
                    SocketAddr::from((Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1), 443)),
                ],
                "{order:?}"
            );
        }
    }

    #[test]
    fn a_device_without_peers_or_a_short_payload_has_no_endpoints() {
        for order in ORDERS {
            let bytes = device_message(order, 0x1b, &[]);
            let msg = Messages::new(&bytes, order).next().unwrap();
            assert!(peer_endpoints(msg.payload, order).is_empty());
            assert!(peer_endpoints(&[0, 1], order).is_empty());
            // A truncated endpoint is left out, not misread.
            let peers = vec![vec![(
                WGPEER_A_ENDPOINT,
                sockaddr_in(order, [10, 0, 0, 1], 1)[..6].to_vec(),
            )]];
            let bytes = device_message(order, 0x1b, &peers);
            let msg = Messages::new(&bytes, order).next().unwrap();
            assert!(peer_endpoints(msg.payload, order).is_empty());
        }
    }

    #[test]
    fn the_device_request_is_a_dump_by_name() {
        for order in ORDERS {
            let bytes = encode(
                order,
                0x1b,
                NLM_F_REQUEST | NLM_F_DUMP,
                2,
                &genl_header(WG_CMD_GET_DEVICE, WG_GENL_VERSION),
                &[(WGDEVICE_A_IFNAME, b"wg0\0")],
            );
            let msg = Messages::new(&bytes, order).next().unwrap();
            assert_eq!(msg.kind, 0x1b);
            assert_eq!(msg.flags, NLM_F_REQUEST | NLM_F_DUMP);
            assert_eq!(&msg.payload[..GENL_HDRLEN], &[0, 1, 0, 0]);
            let (kind, data) = Attrs::new(&msg.payload[GENL_HDRLEN..], order)
                .next()
                .unwrap();
            assert_eq!((kind, data), (WGDEVICE_A_IFNAME, &b"wg0\0"[..]));
        }
    }

    #[tokio::test]
    async fn an_interface_that_does_not_exist_is_an_error() {
        // ENOENT without the module, ENODEV with it, EPERM without the
        // capability: never an empty success, and never a hang.
        let answer =
            tokio::time::timeout(std::time::Duration::from_secs(10), endpoints("sctl-no-wg0"))
                .await
                .expect("the kernel answers");
        assert!(answer.is_err(), "{answer:?}");
    }
}
