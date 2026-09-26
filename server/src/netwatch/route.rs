//! Install and remove the one kind of route the agent owns: an IPv4 host
//! route (`dst/32`) in the main table, tagged with [`RTPROT_SCTL`].
//!
//! Both calls need `CAP_NET_ADMIN` (`EPERM` otherwise) and return the
//! kernel's errno from its acknowledgement.

use std::io;
use std::net::Ipv4Addr;

use super::netlink::{
    self, NlSocket, Order, NLM_F_ACK, NLM_F_CREATE, NLM_F_REPLACE, NLM_F_REQUEST, RTA_DST,
    RTA_GATEWAY, RTA_OIF, RTA_PRIORITY, RTM_DELROUTE, RTM_NEWROUTE, RTN_UNICAST, RT_SCOPE_LINK,
    RT_SCOPE_NOWHERE, RT_SCOPE_UNIVERSE, RT_TABLE_MAIN,
};

/// `rtm_protocol` of every route sctl installs. 83 (ASCII `S`) is unassigned
/// in linux/rtnetlink.h: the kernel's own values are 0 to 4 and routing
/// daemons hold 8 to 18, 42, 99 and 186 to 192. The kernel stores the value
/// without interpreting it, so `ip route show proto 83` lists exactly sctl's
/// routes, and a delete carrying it can never match anyone else's.
pub const RTPROT_SCTL: u8 = 83;

/// One host route owned by sctl:
/// `dst/32 [via via] dev <oif> metric <metric> proto 83`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OwnedRoute {
    pub dst: Ipv4Addr,
    /// Next hop; None for a destination directly on the link.
    pub via: Option<Ipv4Addr>,
    /// Output interface index (`Interface::index` in a `NetState`).
    pub oif: u32,
    /// Route metric (`RTA_PRIORITY`).
    pub metric: u32,
}

/// Create the route, or replace the one already at `dst` with this metric.
///
/// The kernel matches a replace on destination, table, TOS and metric, not
/// on protocol: a route someone else installed at the same metric would be
/// taken over. The caller checks `NetState::host_routes` first.
pub async fn replace(route: &OwnedRoute) -> io::Result<()> {
    let mut socket = NlSocket::open(0)?;
    let seq = socket.next_seq();
    socket
        .acked(&encode_replace(Order::NATIVE, seq, route), seq)
        .await
}

/// Delete sctl's route to `route.dst`. Only a route carrying [`RTPROT_SCTL`]
/// can match; a non-zero `oif` or `metric` and a `via` narrow the match
/// further. `ESRCH` when there is no such route.
pub async fn delete(route: &OwnedRoute) -> io::Result<()> {
    let mut socket = NlSocket::open(0)?;
    let seq = socket.next_seq();
    socket
        .acked(&encode_delete(Order::NATIVE, seq, route), seq)
        .await
}

pub(crate) fn encode_replace(order: Order, seq: u32, route: &OwnedRoute) -> Vec<u8> {
    let scope = if route.via.is_some() {
        RT_SCOPE_UNIVERSE
    } else {
        RT_SCOPE_LINK
    };
    encode(
        order,
        RTM_NEWROUTE,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_REPLACE,
        seq,
        &netlink::rtmsg(32, RT_TABLE_MAIN, RTPROT_SCTL, scope, RTN_UNICAST),
        route,
    )
}

pub(crate) fn encode_delete(order: Order, seq: u32, route: &OwnedRoute) -> Vec<u8> {
    // Scope "nowhere" and type 0 match any scope and type; the protocol is
    // what confines the delete to sctl's own routes.
    encode(
        order,
        RTM_DELROUTE,
        NLM_F_REQUEST | NLM_F_ACK,
        seq,
        &netlink::rtmsg(32, RT_TABLE_MAIN, RTPROT_SCTL, RT_SCOPE_NOWHERE, 0),
        route,
    )
}

fn encode(
    order: Order,
    kind: u16,
    flags: u16,
    seq: u32,
    family_struct: &[u8],
    route: &OwnedRoute,
) -> Vec<u8> {
    let dst = route.dst.octets();
    let via = route.via.map(|gw| gw.octets());
    let oif = order.u32_bytes(route.oif);
    let metric = order.u32_bytes(route.metric);
    let mut attrs: Vec<(u16, &[u8])> = vec![(RTA_DST, &dst)];
    if let Some(gw) = &via {
        attrs.push((RTA_GATEWAY, gw));
    }
    attrs.push((RTA_OIF, &oif));
    attrs.push((RTA_PRIORITY, &metric));
    netlink::encode(order, kind, flags, seq, family_struct, &attrs)
}

#[cfg(test)]
mod tests {
    use super::super::netlink::tests::{attr, u16b, u32b};
    use super::*;

    const ROUTE: OwnedRoute = OwnedRoute {
        dst: Ipv4Addr::new(174, 138, 114, 209),
        via: Some(Ipv4Addr::new(10, 42, 0, 1)),
        oif: 3,
        metric: 0,
    };

    fn expected(
        order: Order,
        kind: u16,
        flags: u16,
        family: [u8; 12],
        route: &OwnedRoute,
    ) -> Vec<u8> {
        let mut body = family.to_vec();
        body.extend(attr(order, RTA_DST, &route.dst.octets()));
        if let Some(gw) = route.via {
            body.extend(attr(order, RTA_GATEWAY, &gw.octets()));
        }
        body.extend(attr(order, RTA_OIF, &u32b(order, route.oif)));
        body.extend(attr(order, RTA_PRIORITY, &u32b(order, route.metric)));
        let mut out = u32b(order, 16 + body.len() as u32);
        out.extend(u16b(order, kind));
        out.extend(u16b(order, flags));
        out.extend(u32b(order, 11));
        out.extend(u32b(order, 0));
        out.extend(body);
        out
    }

    #[test]
    fn replace_request_bytes_in_both_orders() {
        for order in [Order::Little, Order::Big] {
            let bytes = encode_replace(order, 11, &ROUTE);
            let family = [2, 32, 0, 0, 254, RTPROT_SCTL, 0, 1, 0, 0, 0, 0];
            assert_eq!(
                bytes,
                expected(order, RTM_NEWROUTE, 0x0505, family, &ROUTE),
                "{order:?}"
            );
            // The address bytes sit in network order whatever the host order.
            assert_eq!(&bytes[32..36], &[174, 138, 114, 209]);
        }
    }

    #[test]
    fn replace_without_gateway_is_link_scoped_and_omits_rta_gateway() {
        let direct = OwnedRoute { via: None, ..ROUTE };
        for order in [Order::Little, Order::Big] {
            let family = [2, 32, 0, 0, 254, RTPROT_SCTL, 253, 1, 0, 0, 0, 0];
            assert_eq!(
                encode_replace(order, 11, &direct),
                expected(order, RTM_NEWROUTE, 0x0505, family, &direct)
            );
        }
    }

    #[test]
    fn delete_request_is_confined_to_sctl_protocol() {
        for order in [Order::Little, Order::Big] {
            let family = [2, 32, 0, 0, 254, RTPROT_SCTL, 255, 0, 0, 0, 0, 0];
            assert_eq!(
                encode_delete(order, 11, &ROUTE),
                expected(order, RTM_DELROUTE, 0x0005, family, &ROUTE)
            );
        }
    }

    #[test]
    fn protocol_value_is_not_one_the_kernel_or_known_daemons_use() {
        let assigned = [
            0, 1, 2, 3, 4, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 42, 99, 186, 187, 188, 189,
            192,
        ];
        assert!(!assigned.contains(&RTPROT_SCTL));
    }
}
