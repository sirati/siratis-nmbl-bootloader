//! Network bring-up for the network-rescue path.
//!
//! Enumerates Ethernet NICs, brings the first live one up, acquires a
//! DHCP lease, and configures the interface with the granted address,
//! netmask, and default route.

use std::io;
use std::mem;
use std::net::Ipv4Addr;
use std::os::fd::AsRawFd;
use std::time::Duration;

use nix::sys::socket::{AddressFamily, SockFlag, SockType, socket};

use crate::error::{NmblError, Result};
use crate::net::dhcp::{self, DhcpLease};
use crate::net::iface::{self, Interface};
use crate::nmbl_warn;

/// Per-NIC link-up wait. 10s rides out PHY autonegotiation on
/// gigabit copper without burning the operator's patience.
pub(super) const LINK_WAIT: Duration = Duration::from_secs(10);
/// Per-NIC DHCP exchange budget. 30s covers a slow embedded relay
/// without making a dead network feel hung forever.
pub(super) const DHCP_TIMEOUT: Duration = Duration::from_secs(30);

/// Enumerate Ethernet NICs, bring each up in turn, and run DHCP on
/// the first one with a live carrier. Returns the interface chosen +
/// the granted lease.
pub(super) fn bring_up_and_dhcp() -> Result<(Interface, DhcpLease)> {
    let candidates = iface::enumerate()?;
    if candidates.is_empty() {
        return Err(NmblError::Rescue {
            stage: "net-no-iface",
            source: Box::new(NmblError::ConfigInvalid {
                reason: "no ARPHRD_ETHER interfaces found under /sys/class/net".to_string(),
                context: "enumerating NICs for network rescue".to_string(),
            }),
        });
    }

    let mut last_err: Option<NmblError> = None;
    for cand in &candidates {
        if let Err(e) = iface::bring_up(&cand.name) {
            nmbl_warn!(
                "network rescue: bring_up({}) failed: {e}; trying next NIC",
                cand.name
            );
            last_err = Some(e);
            continue;
        }
        match iface::wait_for_link(&cand.name, LINK_WAIT) {
            Ok(true) => {}
            Ok(false) => {
                nmbl_warn!(
                    "network rescue: no carrier on {} after {:?}; trying next NIC",
                    cand.name,
                    LINK_WAIT,
                );
                continue;
            }
            Err(e) => {
                nmbl_warn!(
                    "network rescue: wait_for_link({}) failed: {e}; trying next NIC",
                    cand.name,
                );
                last_err = Some(e);
                continue;
            }
        }

        match dhcp::acquire(cand, DHCP_TIMEOUT) {
            Ok(lease) => return Ok((cand.clone(), lease)),
            Err(e) => {
                nmbl_warn!(
                    "network rescue: DHCP on {} failed: {e}; trying next NIC",
                    cand.name
                );
                last_err = Some(e);
                continue;
            }
        }
    }

    Err(last_err.unwrap_or(NmblError::Rescue {
        stage: "net-no-iface",
        source: Box::new(NmblError::ConfigInvalid {
            reason: "no candidate NIC produced a DHCP lease".to_string(),
            context: format!("exhausted {} NIC(s)", candidates.len()),
        }),
    }))
}

/// Push the granted lease onto the interface: SIOCSIFADDR for the IP,
/// SIOCSIFNETMASK for the mask, and (when present) SIOCADDRT to
/// install a default route through the lease's gateway.
pub(super) fn apply_lease(iface: &Interface, lease: &DhcpLease) -> Result<()> {
    let sock = socket(
        AddressFamily::Inet,
        SockType::Datagram,
        SockFlag::SOCK_CLOEXEC,
        None,
    )
    .map_err(|source| NmblError::Rescue {
        stage: "net-apply-lease",
        source: Box::new(NmblError::Io {
            source: io::Error::from_raw_os_error(source as i32),
            context: format!("socket(AF_INET, SOCK_DGRAM) for {}", iface.name),
        }),
    })?;

    let mut req = blank_ifreq_with_addr(&iface.name, lease.ip)?;
    // SAFETY: SIOCSIFADDR reads `ifr_name` and `ifr_ifru.ifru_addr`
    // (an sockaddr_in stuffed into the union by
    // `blank_ifreq_with_addr`). Both are initialized above; the
    // socket fd is live for the duration of the call.
    let rc = unsafe {
        libc::ioctl(
            sock.as_raw_fd(),
            libc::SIOCSIFADDR as _,
            &req as *const libc::ifreq,
        )
    };
    if rc < 0 {
        return Err(NmblError::Rescue {
            stage: "net-apply-lease",
            source: Box::new(NmblError::Io {
                source: io::Error::last_os_error(),
                context: format!("SIOCSIFADDR {} -> {}", iface.name, lease.ip),
            }),
        });
    }

    // SIOCSIFNETMASK — reuse the ifreq but swap in the netmask.
    stuff_sockaddr_in(&mut req, lease.netmask);
    // SAFETY: identical preconditions to the SIOCSIFADDR call above;
    // only the in-union sockaddr_in payload changed.
    let rc = unsafe {
        libc::ioctl(
            sock.as_raw_fd(),
            libc::SIOCSIFNETMASK as _,
            &req as *const libc::ifreq,
        )
    };
    if rc < 0 {
        return Err(NmblError::Rescue {
            stage: "net-apply-lease",
            source: Box::new(NmblError::Io {
                source: io::Error::last_os_error(),
                context: format!("SIOCSIFNETMASK {} -> {}", iface.name, lease.netmask),
            }),
        });
    }

    if let Some(gw) = lease.gateway {
        add_default_route(sock.as_raw_fd(), gw)?;
    }
    Ok(())
}

/// Build a `libc::ifreq` whose `ifr_name` is populated and whose
/// union slot already carries the supplied IPv4 address as a
/// `sockaddr_in`. Caller mutates with [`stuff_sockaddr_in`] to reuse
/// the ifreq for SIOCSIFNETMASK.
fn blank_ifreq_with_addr(name: &str, addr: Ipv4Addr) -> Result<libc::ifreq> {
    if name.len() >= libc::IFNAMSIZ {
        return Err(NmblError::Rescue {
            stage: "net-apply-lease",
            source: Box::new(NmblError::ConfigInvalid {
                reason: format!(
                    "interface name {name:?} exceeds IFNAMSIZ ({})",
                    libc::IFNAMSIZ
                ),
                context: "preparing ifreq for SIOCSIFADDR".to_string(),
            }),
        });
    }
    // SAFETY: `libc::ifreq` is POD; the all-zero bit pattern is a
    // valid value. Same justification as `iface::blank_ifreq`.
    let mut req: libc::ifreq = unsafe { mem::zeroed() };
    for (slot, byte) in req.ifr_name.iter_mut().zip(name.as_bytes()) {
        *slot = *byte as libc::c_char;
    }
    stuff_sockaddr_in(&mut req, addr);
    Ok(req)
}

/// Overwrite the union slot of `req` with an IPv4 `sockaddr_in`
/// holding `addr` and port 0. Used to flip the same ifreq between
/// SIOCSIFADDR (IP) and SIOCSIFNETMASK (mask).
fn stuff_sockaddr_in(req: &mut libc::ifreq, addr: Ipv4Addr) {
    // SAFETY: `sockaddr_in` is POD; all-zero is a valid value.
    let mut sin: libc::sockaddr_in = unsafe { mem::zeroed() };
    sin.sin_family = libc::AF_INET as libc::sa_family_t;
    sin.sin_port = 0;
    sin.sin_addr.s_addr = u32::from_ne_bytes(addr.octets());
    // SAFETY: we're writing into the union slot of an ifreq. The
    // `ifr_ifru.ifru_addr` variant is `sockaddr`, which is the
    // C-standard prefix of `sockaddr_in` (same alignment, smaller
    // size). Copying `sockaddr_in` bytes into the union via a
    // matched-size raw write is the canonical pattern for the
    // SIOCSIF* ioctls (see netdevice(7)).
    unsafe {
        let dst: *mut libc::sockaddr_in =
            (&mut req.ifr_ifru as *mut _ as *mut u8).cast::<libc::sockaddr_in>();
        std::ptr::write_unaligned(dst, sin);
    }
}

/// Install `gateway` as the IPv4 default route via SIOCADDRT.
fn add_default_route(sock_fd: libc::c_int, gateway: Ipv4Addr) -> Result<()> {
    // SAFETY: `libc::rtentry` is POD; all-zero is a valid value.
    let mut rt: libc::rtentry = unsafe { mem::zeroed() };

    // Destination = 0.0.0.0 / 0.0.0.0 — i.e. the default route.
    stuff_rt_sockaddr(&mut rt.rt_dst, Ipv4Addr::UNSPECIFIED);
    stuff_rt_sockaddr(&mut rt.rt_genmask, Ipv4Addr::UNSPECIFIED);
    stuff_rt_sockaddr(&mut rt.rt_gateway, gateway);
    // RTF_UP | RTF_GATEWAY — both libc constants are already `u16`
    // (rt_flags' type), so no cast is needed.
    rt.rt_flags = libc::RTF_UP | libc::RTF_GATEWAY;
    rt.rt_metric = 0;

    // SAFETY: SIOCADDRT reads a `struct rtentry` from userspace.
    // `rt` is fully initialised above and outlives the ioctl call.
    let rc = unsafe { libc::ioctl(sock_fd, libc::SIOCADDRT as _, &rt as *const libc::rtentry) };
    if rc < 0 {
        let err = io::Error::last_os_error();
        // EEXIST = the route is already there (e.g. from a stray
        // RA on a dual-stack network). Treat as success rather than
        // forcing the operator to halt over a benign collision.
        if err.raw_os_error() == Some(libc::EEXIST) {
            return Ok(());
        }
        return Err(NmblError::Rescue {
            stage: "net-apply-lease",
            source: Box::new(NmblError::Io {
                source: err,
                context: format!("SIOCADDRT default via {gateway}"),
            }),
        });
    }
    Ok(())
}

/// Write an IPv4 `sockaddr_in` into one of the `rtentry`'s sockaddr
/// slots. `rt_dst`, `rt_gateway`, and `rt_genmask` are all generic
/// `sockaddr` slots that the kernel re-interprets per the address
/// family — same recipe as `stuff_sockaddr_in`.
fn stuff_rt_sockaddr(dst: &mut libc::sockaddr, addr: Ipv4Addr) {
    // SAFETY: `sockaddr_in` is POD.
    let mut sin: libc::sockaddr_in = unsafe { mem::zeroed() };
    sin.sin_family = libc::AF_INET as libc::sa_family_t;
    sin.sin_port = 0;
    sin.sin_addr.s_addr = u32::from_ne_bytes(addr.octets());
    // SAFETY: `sockaddr_in` and `sockaddr` share a common header;
    // the kernel demands the AF_INET-shaped payload here. We use
    // `write_unaligned` so the cast does not assume alignment.
    unsafe {
        let dst_ptr: *mut libc::sockaddr_in =
            (dst as *mut libc::sockaddr).cast::<libc::sockaddr_in>();
        std::ptr::write_unaligned(dst_ptr, sin);
    }
}
