//! Which local address, and so which interface, the kernel would use to reach
//! a destination right now.
//!
//! Connecting a UDP socket runs the kernel's route lookup and fixes the
//! source address without sending anything, so the answer includes every
//! rule and metric the kernel applies. The interface is then the one holding
//! that address.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};

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
