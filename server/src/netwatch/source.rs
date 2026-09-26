//! Which local address, and so which interface, the kernel would use to reach
//! a destination right now.
//!
//! Connecting a UDP socket runs the kernel's route lookup and fixes the
//! source address without sending anything, so the answer includes every
//! rule and metric the kernel applies. The interface is then the one holding
//! that address. [`route_get`] asks the kernel the same question over
//! netlink, as `ip route get` does, and also learns the next hop.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};

use super::netlink::{self, NlSocket, Order, RTA_DST, RTM_GETROUTE, RTM_NEWROUTE};

/// The kernel's current choice for reaching one destination.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceRoute {
    /// The source address a new connection would use.
    pub source: Ipv4Addr,
    /// The interface holding `source` (None if no interface lists it, which
    /// only happens while an address is being removed).
    pub dev: Option<String>,
}

/// The source address the kernel picks for a connection to `dst`. No packet
/// is sent. Fails (typically `ENETUNREACH`) when no route reaches `dst`.
pub fn source_for(dst: SocketAddrV4) -> io::Result<Ipv4Addr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.connect(dst)?;
    match socket.local_addr()? {
        SocketAddr::V4(local) => Ok(*local.ip()),
        SocketAddr::V6(_) => Err(io::Error::other("IPv4 socket reported an IPv6 address")),
    }
}

/// The kernel's route to `dst` as a source address and its interface.
pub fn route_to(dst: SocketAddrV4) -> io::Result<SourceRoute> {
    let source = source_for(dst)?;
    Ok(SourceRoute {
        source,
        dev: interface_with_ipv4(source),
    })
}

/// The route the kernel would use for `dst` right now, as `ip route get`
/// shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KernelRoute {
    /// The output interface.
    pub dev: String,
    /// The next hop; None when `dst` is on the link.
    pub via: Option<Ipv4Addr>,
    /// The source address a new connection would use.
    pub src: Option<Ipv4Addr>,
}

/// Look up the kernel's route to `dst` (`RTM_GETROUTE`, what `ip route get`
/// sends). Nothing is sent to `dst`. Fails with the kernel's errno, typically
/// `ENETUNREACH`, when no route reaches it.
pub async fn route_get(dst: Ipv4Addr) -> io::Result<KernelRoute> {
    let mut socket = NlSocket::open(0)?;
    let payload = socket
        .request(
            RTM_GETROUTE,
            &netlink::rtmsg(32, 0, 0, 0, 0),
            &[(RTA_DST, &dst.octets())],
            RTM_NEWROUTE,
        )
        .await?;
    let route = netlink::parse_route(&payload, Order::NATIVE)
        .ok_or_else(|| io::Error::other("unreadable route lookup answer"))?;
    let oif = route
        .oif
        .ok_or_else(|| io::Error::other("route lookup answer names no interface"))?;
    let dev = interface_name(oif)
        .ok_or_else(|| io::Error::other(format!("no interface with index {oif}")))?;
    Ok(KernelRoute {
        dev,
        via: route.gateway,
        src: route.prefsrc,
    })
}

/// The name of the interface with kernel index `index`.
pub fn interface_name(index: u32) -> Option<String> {
    let mut name = [0 as libc::c_char; libc::IF_NAMESIZE];
    // SAFETY: the buffer is IF_NAMESIZE bytes, the size if_indextoname(3)
    // writes at most, NUL included; it returns null on failure.
    let found = unsafe { libc::if_indextoname(index, name.as_mut_ptr()) };
    if found.is_null() {
        return None;
    }
    // SAFETY: on success the buffer holds a NUL-terminated name.
    let name = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) };
    name.to_str().ok().map(ToString::to_string)
}

/// Every IPv4 address held by a local interface, as (interface, address), in
/// `getifaddrs(3)` order. Empty if the call fails.
pub fn ipv4_addresses() -> Vec<(String, Ipv4Addr)> {
    let mut found = Vec::new();
    // SAFETY: getifaddrs fills a list that stays valid until freeifaddrs;
    // every pointer read below is null-checked, and an AF_INET ifa_addr is a
    // kernel-written sockaddr_in.
    unsafe {
        let mut ifaddrs: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&raw mut ifaddrs) != 0 {
            return found;
        }
        let mut current = ifaddrs;
        while !current.is_null() {
            let ifa = &*current;
            if !ifa.ifa_name.is_null()
                && !ifa.ifa_addr.is_null()
                && i32::from((*ifa.ifa_addr).sa_family) == libc::AF_INET
            {
                if let Ok(name) = std::ffi::CStr::from_ptr(ifa.ifa_name).to_str() {
                    #[allow(clippy::cast_ptr_alignment)]
                    let addr = &*(ifa.ifa_addr.cast::<libc::sockaddr_in>());
                    let ip = Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr));
                    found.push((name.to_string(), ip));
                }
            }
            current = ifa.ifa_next;
        }
        libc::freeifaddrs(ifaddrs);
    }
    found
}

/// The first IPv4 address of interface `name`.
pub fn interface_ipv4(name: &str) -> Option<Ipv4Addr> {
    ipv4_addresses()
        .into_iter()
        .find_map(|(iface, ip)| (iface == name).then_some(ip))
}

/// The interface holding `ip`.
pub fn interface_with_ipv4(ip: Ipv4Addr) -> Option<String> {
    ipv4_addresses()
        .into_iter()
        .find_map(|(iface, addr)| (addr == ip).then_some(iface))
}

/// Send everything on the socket `fd` through interface `dev`
/// (`SO_BINDTODEVICE`), whatever the routing table prefers. Binding to an
/// address alone only picks the source; the kernel would still route by
/// metric. Needs root (`CAP_NET_RAW`) on older kernels.
#[cfg(unix)]
pub fn bind_to_device(fd: &impl std::os::unix::io::AsRawFd, dev: &str) -> io::Result<()> {
    let name = std::ffi::CString::new(dev)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid interface name"))?;
    let len = libc::socklen_t::try_from(name.as_bytes_with_nul().len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "interface name too long"))?;
    // SAFETY: the option value is a NUL-terminated name that outlives the call.
    let ret = unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_BINDTODEVICE,
            name.as_ptr().cast(),
            len,
        )
    };
    if ret == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_destination_is_sourced_from_loopback() {
        let route = route_to(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9)).unwrap();
        assert_eq!(route.source, Ipv4Addr::LOCALHOST);
        assert_eq!(route.dev.as_deref(), Some("lo"));
    }

    #[tokio::test]
    async fn the_kernel_routes_loopback_through_lo() {
        let route = route_get(Ipv4Addr::LOCALHOST).await.unwrap();
        assert_eq!(route.dev, "lo");
        assert_eq!(route.via, None);
        assert_eq!(route.src, Some(Ipv4Addr::LOCALHOST));
    }

    #[tokio::test]
    async fn the_netlink_lookup_agrees_with_the_socket_lookup() {
        // Whatever this host's routes are, both ways of asking agree.
        let dst = Ipv4Addr::new(192, 0, 2, 1);
        match (route_get(dst).await, route_to(SocketAddrV4::new(dst, 9))) {
            (Ok(netlink), Ok(socket)) => {
                assert_eq!(netlink.src, Some(socket.source));
                if let Some(dev) = socket.dev {
                    assert_eq!(netlink.dev, dev);
                }
            }
            (Err(_), Err(_)) => {}
            (netlink, socket) => panic!("{netlink:?} vs {socket:?}"),
        }
    }

    #[test]
    fn interface_names_come_from_the_index() {
        assert_eq!(interface_name(1).as_deref(), Some("lo"));
        assert_eq!(interface_name(u32::MAX), None);
    }

    #[test]
    fn interface_lookups_agree_both_ways() {
        for (name, ip) in ipv4_addresses() {
            assert!(interface_ipv4(&name).is_some(), "{name}");
            assert!(interface_with_ipv4(ip).is_some(), "{ip}");
        }
        assert_eq!(interface_ipv4("lo"), Some(Ipv4Addr::LOCALHOST));
        assert_eq!(interface_ipv4("no-such-interface0"), None);
    }
}
