//! AF_PACKET socket setup, send, and receive helpers.

use std::mem;
use std::os::fd::{AsRawFd, OwnedFd};
use std::time::Duration;

use dhcproto::v4::{Message, MessageType};
use dhcproto::{Decodable, Decoder};
use nix::sys::socket::setsockopt;
use nix::sys::socket::sockopt::ReceiveTimeout;
use nix::sys::time::TimeVal;

use crate::error::{NmblError, Result};
use crate::net::iface::Interface;

use super::RECV_BUF_LEN;
use super::packet::strip_ip_udp;

// ---------------------------------------------------------------------------
// Socket setup
// ---------------------------------------------------------------------------

/// Open an `AF_PACKET / SOCK_DGRAM / ETH_P_IP` socket and bind it to
/// `iface.index`. SOCK_DGRAM (not SOCK_RAW) means the kernel strips
/// the L2 header on receive and prepends it on send.
pub(super) fn open_packet_socket(iface: &Interface) -> Result<OwnedFd> {
    // `socket(AF_PACKET, SOCK_DGRAM, htons(ETH_P_IP))`. The protocol
    // argument is fed to the syscall in network byte order, so we
    // pre-swap here rather than via `SockProtocol`. nix's
    // `SockProtocol` enum does not include an `EthIp` variant, so we
    // fall through to the raw libc call for the socket() syscall.
    let proto = (libc::ETH_P_IP as u16).to_be() as libc::c_int;
    // SAFETY: libc::socket has no preconditions beyond integer-valued
    // arguments. We immediately wrap the fd in OwnedFd to ensure
    // close() runs on drop.
    let fd = unsafe {
        libc::socket(
            libc::AF_PACKET,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
            proto,
        )
    };
    if fd < 0 {
        return Err(NmblError::Rescue {
            stage: "dhcp-socket",
            source: Box::new(NmblError::Io {
                source: std::io::Error::last_os_error(),
                context: format!("socket(AF_PACKET, SOCK_DGRAM, ETH_P_IP) for {}", iface.name),
            }),
        });
    }
    // SAFETY: libc::socket returned a valid open fd.
    let sock = unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };

    // bind() to ifindex via sockaddr_ll. We hand-construct it
    // because nix's LinkAddr does not expose a constructor.
    let sll = make_sockaddr_ll(iface.index, libc::ETH_P_IP as u16);
    // SAFETY: libc::bind reads `addrlen` bytes starting at the
    // sockaddr pointer; `sll` is a fully initialized sockaddr_ll on
    // the stack with no padding holes.
    let rc = unsafe {
        libc::bind(
            sock.as_raw_fd(),
            (&sll as *const libc::sockaddr_ll).cast::<libc::sockaddr>(),
            mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(NmblError::Rescue {
            stage: "dhcp-socket",
            source: Box::new(NmblError::Io {
                source: std::io::Error::last_os_error(),
                context: format!("bind(AF_PACKET) to ifindex {}", iface.index),
            }),
        });
    }

    Ok(sock)
}

/// Build a `sockaddr_ll` for `bind`-ing an AF_PACKET socket to a
/// specific ifindex. We only ever bind (and send), so `sll_addr` is
/// the L2 broadcast address. `protocol_host` is the EtherType in
/// host byte order — the function does the `to_be()` swap.
pub(super) fn make_sockaddr_ll(ifindex: u32, protocol_host: u16) -> libc::sockaddr_ll {
    // SAFETY: `libc::sockaddr_ll` is a POD made of integers and a
    // fixed-size byte array. The all-zero bit pattern is a valid
    // value (sll_addr empty, sll_halen 0).
    let mut sll: libc::sockaddr_ll = unsafe { mem::zeroed() };
    sll.sll_family = libc::AF_PACKET as u16;
    sll.sll_protocol = protocol_host.to_be();
    sll.sll_ifindex = ifindex as i32;
    sll.sll_halen = 6;
    // L2 broadcast for the destination.
    for slot in sll.sll_addr.iter_mut().take(6) {
        *slot = 0xff;
    }
    sll
}

/// Apply SO_RCVTIMEO so `recvfrom` is bounded per retry instead of
/// blocking forever.
pub(super) fn set_recv_timeout(sock: &OwnedFd, dur: Duration) -> Result<()> {
    let secs = i64::try_from(dur.as_secs()).unwrap_or(i64::MAX);
    let usecs = i64::from(dur.subsec_micros());
    let tv = TimeVal::new(secs, usecs);
    setsockopt(sock, ReceiveTimeout, &tv).map_err(|source| NmblError::Rescue {
        stage: "dhcp-socket",
        source: Box::new(NmblError::Io {
            source: std::io::Error::from_raw_os_error(source as i32),
            context: "setsockopt(SO_RCVTIMEO)".to_string(),
        }),
    })
}

// ---------------------------------------------------------------------------
// Send / receive
// ---------------------------------------------------------------------------

/// `sendto` to L2 broadcast on `ifindex` with the supplied IP+UDP+DHCP
/// payload. `stage` is used to label any error.
pub(super) fn send_l2_broadcast(
    sock: &OwnedFd,
    ifindex: u32,
    payload: &[u8],
    stage: &'static str,
) -> Result<()> {
    let sll = make_sockaddr_ll(ifindex, libc::ETH_P_IP as u16);
    // SAFETY: libc::sendto reads `addrlen` bytes starting at the
    // sockaddr pointer; `sll` is fully initialized above. The
    // payload pointer/length pair come from a Rust slice, so they
    // are valid for `payload.len()` bytes.
    let rc = unsafe {
        libc::sendto(
            sock.as_raw_fd(),
            payload.as_ptr().cast::<libc::c_void>(),
            payload.len(),
            0,
            (&sll as *const libc::sockaddr_ll).cast::<libc::sockaddr>(),
            mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(NmblError::Rescue {
            stage,
            source: Box::new(NmblError::Io {
                source: std::io::Error::last_os_error(),
                context: format!("sendto(AF_PACKET) on ifindex {ifindex}"),
            }),
        });
    }
    Ok(())
}

/// Block on `recvfrom` until either the SO_RCVTIMEO fires or we
/// receive a DHCP message whose XID matches `xid` and whose
/// `MessageType` option matches `want`. NAKs are surfaced as a
/// `dhcp-nak` failure regardless of `want`.
pub(super) fn recv_dhcp(sock: &OwnedFd, xid: u32, want: MessageType) -> Result<Message> {
    let mut buf = vec![0u8; RECV_BUF_LEN];
    loop {
        // SAFETY: libc::recvfrom writes at most `buf.len()` bytes to
        // the buffer pointer. We pass NULL for the address pointer
        // because we already have the ifindex in the bound socket.
        let n = unsafe {
            libc::recvfrom(
                sock.as_raw_fd(),
                buf.as_mut_ptr().cast::<libc::c_void>(),
                buf.len(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            let raw = err.raw_os_error().unwrap_or(0);
            // SO_RCVTIMEO fires as EAGAIN / EWOULDBLOCK on Linux.
            if raw == libc::EAGAIN || raw == libc::EWOULDBLOCK {
                return Err(NmblError::Rescue {
                    stage: "dhcp-timeout",
                    source: Box::new(NmblError::Io {
                        source: err,
                        context: "recvfrom timed out (SO_RCVTIMEO)".to_string(),
                    }),
                });
            }
            return Err(NmblError::Rescue {
                stage: match want {
                    MessageType::Offer => "dhcp-recv-offer",
                    _ => "dhcp-recv-ack",
                },
                source: Box::new(NmblError::Io {
                    source: err,
                    context: "recvfrom(AF_PACKET)".to_string(),
                }),
            });
        }
        let n = n as usize;
        let frame = match buf.get(..n) {
            Some(s) => s,
            None => continue,
        };

        // SOCK_DGRAM strips the Ethernet header but leaves IP + UDP.
        let payload = match strip_ip_udp(frame) {
            Some(p) => p,
            None => continue,
        };
        let msg = match Message::decode(&mut Decoder::new(payload)) {
            Ok(m) => m,
            // A malformed frame is not a permanent failure; another
            // peer on the LAN may be chattering. Drop and re-read.
            Err(_) => continue,
        };
        if msg.xid() != xid {
            continue;
        }
        let mtype = msg.opts().msg_type();
        if mtype == Some(MessageType::Nak) {
            return Err(NmblError::Rescue {
                stage: "dhcp-nak",
                source: Box::new(NmblError::ConfigInvalid {
                    reason: "server returned DHCP NAK".to_string(),
                    context: format!("xid={xid:#010x}"),
                }),
            });
        }
        if mtype != Some(want) {
            continue;
        }
        return Ok(msg);
    }
}
