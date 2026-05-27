//! One-shot DHCPv4 client used by the rescue HTTP fallback (Phase D.2).
//!
//! The rescue path needs an IPv4 address before it can pull the rescue
//! squashfs over HTTP. We avoid pulling in a full DHCP daemon: this
//! module runs a single DISCOVER → OFFER → REQUEST → ACK exchange,
//! captures the lease, and hands it back to the caller. No renew, no
//! release — the rescue path runs for seconds, not days.
//!
//! ## Wire mechanics
//!
//! We open a single `AF_PACKET` / `SOCK_DGRAM` socket bound to the
//! interface's ifindex with protocol `ETH_P_IP`. The kernel strips
//! the Ethernet header for us (that's the difference between
//! `SOCK_DGRAM` and `SOCK_RAW` on `AF_PACKET`) but does NOT build
//! the IPv4 + UDP headers — userspace owns those. We therefore
//! hand-assemble a 28-byte IP+UDP header, prepend it to the
//! `dhcproto`-encoded payload, and `sendto` the result to the
//! broadcast L2 address.
//!
//! ## `unsafe` budget
//!
//! Building `libc::sockaddr_ll` requires either `mem::zeroed()` (one
//! POD-only unsafe block, identical to `iface::blank_ifreq`) or a
//! C-side helper. We pick the former — `sockaddr_ll` is integers
//! and a fixed-size byte array. Everything else uses safe nix /
//! getrandom wrappers.
//!
//! ## Robustness
//!
//! - XID is a random `u32` (`getrandom::fill` — single 4-byte read);
//!   we drop every packet whose XID does not match.
//! - SO_RCVTIMEO bounds each `recvfrom`; the outer loop bounds the
//!   total time via `Instant::now() >= deadline`.
//! - We retry DISCOVER up to [`MAX_RETRIES`] times with capped
//!   exponential backoff. Real-world DHCP servers respond in well
//!   under 100ms; the retry budget exists for packet loss, not for
//!   slow servers.
//! - A DHCP NAK (option 53 = 6) aborts immediately with a dedicated
//!   `"dhcp-nak"` stage so the operator can diagnose policy
//!   rejections instead of waiting for a timeout.

#![allow(dead_code)]

use std::mem;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, OwnedFd};
use std::time::{Duration, Instant};

use dhcproto::v4::{
    DhcpOption, Flags, Message, MessageType, Opcode, OptionCode, CLIENT_PORT, SERVER_PORT,
};
use dhcproto::{Decodable, Decoder, Encodable};
use nix::sys::socket::sockopt::ReceiveTimeout;
use nix::sys::socket::{
    setsockopt, socket, AddressFamily, MsgFlags, SockFlag, SockType, SockaddrStorage,
};
use nix::sys::time::TimeVal;

use crate::error::{NmblError, Result};
use crate::net::iface::Interface;

/// Cap on DISCOVER/REQUEST retries (per phase) before we give up. The
/// total elapsed time is still bounded by the `timeout` parameter so
/// this is mostly a sanity bound — a noisy LAN should not let us
/// loop forever even if `timeout` is generous.
const MAX_RETRIES: u32 = 5;
/// Lower bound on per-retry receive timeout. Sub-second values would
/// burn CPU on retries; DHCP servers normally answer in <100ms but
/// some embedded relays take a beat.
const MIN_PER_RETRY: Duration = Duration::from_millis(500);
/// Upper bound on per-retry receive timeout — keeps the exponential
/// backoff from collapsing the entire `timeout` budget into one
/// `recvfrom`.
const MAX_PER_RETRY: Duration = Duration::from_secs(4);
/// Receive buffer. DHCP packets are typically <600 bytes; 1500 is a
/// comfortable MTU-sized ceiling that lets us reject oversize frames
/// without a second `recvfrom`.
const RECV_BUF_LEN: usize = 1500;

/// IPv4 protocol number for UDP — referenced both in the IP header
/// and the UDP pseudo-header.
const IPPROTO_UDP_U8: u8 = 17;
/// Default TTL for outbound IPv4. 64 matches Linux's default and is
/// well above the 1-hop minimum DHCP servers expect.
const IP_TTL_DEFAULT: u8 = 64;
/// IPv4 header length in bytes (no options).
const IPV4_HEADER_LEN: usize = 20;
/// UDP header length in bytes.
const UDP_HEADER_LEN: usize = 8;

/// Outcome of a successful DHCPv4 exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DhcpLease {
    /// `yiaddr` from the ACK — the IP the server granted us.
    pub ip: Ipv4Addr,
    /// Option 1.
    pub netmask: Ipv4Addr,
    /// Option 3. RFC 2131 allows the server to omit this — small
    /// embedded networks sometimes do.
    pub gateway: Option<Ipv4Addr>,
    /// Option 6. May be empty if the server provides no resolvers.
    pub dns: Vec<Ipv4Addr>,
    /// Option 54. Required for unicast renew (we don't renew, but
    /// the caller may want it for logging).
    pub server_id: Ipv4Addr,
    /// Option 51. Informational only — we never renew.
    pub lease_secs: u32,
}

/// One-shot DHCPv4 lease acquisition on `iface`. Returns the granted
/// lease on success. `timeout` bounds the entire
/// DISCOVER → OFFER → REQUEST → ACK sequence.
pub fn acquire(iface: &Interface, timeout: Duration) -> Result<DhcpLease> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(|| Instant::now() + Duration::from_secs(3600));
    let xid = random_xid()?;

    let sock = open_packet_socket(iface)?;

    // OFFER phase --------------------------------------------------
    let offer = discover_until_offer(&sock, iface, xid, deadline)?;
    let server_id = offer.server_id;
    let offered_ip = offer.yiaddr;

    // REQUEST/ACK phase --------------------------------------------
    let ack = request_until_ack(&sock, iface, xid, offered_ip, server_id, deadline)?;
    Ok(ack)
}

// ---------------------------------------------------------------------------
// Socket setup
// ---------------------------------------------------------------------------

/// Open an `AF_PACKET / SOCK_DGRAM / ETH_P_IP` socket and bind it to
/// `iface.index`. SOCK_DGRAM (not SOCK_RAW) means the kernel strips
/// the L2 header on receive and prepends it on send.
fn open_packet_socket(iface: &Interface) -> Result<OwnedFd> {
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
/// the L2 broadcast address.
fn make_sockaddr_ll(ifindex: u32, protocol_be: u16) -> libc::sockaddr_ll {
    // SAFETY: `libc::sockaddr_ll` is a POD made of integers and a
    // fixed-size byte array. The all-zero bit pattern is a valid
    // value (sll_addr empty, sll_halen 0).
    let mut sll: libc::sockaddr_ll = unsafe { mem::zeroed() };
    sll.sll_family = libc::AF_PACKET as u16;
    sll.sll_protocol = protocol_be.to_be();
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
fn set_recv_timeout(sock: &OwnedFd, dur: Duration) -> Result<()> {
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
// DISCOVER / REQUEST loops
// ---------------------------------------------------------------------------

/// Internal handoff between the DISCOVER and REQUEST phases.
struct Offer {
    yiaddr: Ipv4Addr,
    server_id: Ipv4Addr,
}

/// Send DISCOVERs until the server responds with an OFFER (or we
/// exhaust the deadline / retry budget). The returned [`Offer`]
/// captures the two fields REQUEST needs.
fn discover_until_offer(
    sock: &OwnedFd,
    iface: &Interface,
    xid: u32,
    deadline: Instant,
) -> Result<Offer> {
    let discover = build_discover(xid, &iface.mac).map_err(|e| NmblError::Rescue {
        stage: "dhcp-send-discover",
        source: Box::new(e),
    })?;

    let mut attempt: u32 = 0;
    loop {
        let remaining = remaining_or_timeout(deadline)?;
        set_recv_timeout(sock, per_retry_timeout(attempt, remaining))?;

        send_l2_broadcast(sock, iface.index, &discover, "dhcp-send-discover")?;

        match recv_dhcp(sock, xid, MessageType::Offer) {
            Ok(parsed) => return Ok(parsed_to_offer(&parsed)),
            Err(NmblError::Rescue { stage, .. })
                if stage == "dhcp-timeout" && attempt + 1 < MAX_RETRIES =>
            {
                attempt += 1;
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Send REQUESTs until the server responds with an ACK. A NAK is
/// terminal — we surface it immediately rather than retry.
fn request_until_ack(
    sock: &OwnedFd,
    iface: &Interface,
    xid: u32,
    offered_ip: Ipv4Addr,
    server_id: Ipv4Addr,
    deadline: Instant,
) -> Result<DhcpLease> {
    let request =
        build_request(xid, &iface.mac, offered_ip, server_id).map_err(|e| NmblError::Rescue {
            stage: "dhcp-send-request",
            source: Box::new(e),
        })?;

    let mut attempt: u32 = 0;
    loop {
        let remaining = remaining_or_timeout(deadline)?;
        set_recv_timeout(sock, per_retry_timeout(attempt, remaining))?;

        send_l2_broadcast(sock, iface.index, &request, "dhcp-send-request")?;

        match recv_dhcp(sock, xid, MessageType::Ack) {
            Ok(parsed) => return Ok(parsed_to_lease(&parsed, offered_ip, server_id)),
            Err(NmblError::Rescue { stage, .. })
                if stage == "dhcp-timeout" && attempt + 1 < MAX_RETRIES =>
            {
                attempt += 1;
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Cap the per-retry receive timeout so a generous outer `timeout`
/// can't get blown on a single `recvfrom`. `2^attempt * 1s` clamped
/// to `[MIN_PER_RETRY, min(MAX_PER_RETRY, remaining)]`.
fn per_retry_timeout(attempt: u32, remaining: Duration) -> Duration {
    let base_ms = 1_000u64.saturating_mul(1u64 << attempt.min(3));
    let base = Duration::from_millis(base_ms);
    let upper = if remaining < MAX_PER_RETRY {
        remaining
    } else {
        MAX_PER_RETRY
    };
    if base > upper {
        upper
    } else {
        base.max(MIN_PER_RETRY)
    }
}

/// Compute the remaining wall-clock budget or fail with `dhcp-timeout`.
fn remaining_or_timeout(deadline: Instant) -> Result<Duration> {
    let now = Instant::now();
    if now >= deadline {
        Err(NmblError::Rescue {
            stage: "dhcp-timeout",
            source: Box::new(NmblError::Io {
                source: std::io::Error::from(std::io::ErrorKind::TimedOut),
                context: "deadline exceeded before exchange completed".to_string(),
            }),
        })
    } else {
        Ok(deadline.saturating_duration_since(now))
    }
}

// ---------------------------------------------------------------------------
// Send / receive
// ---------------------------------------------------------------------------

/// `sendto` to L2 broadcast on `ifindex` with the supplied IP+UDP+DHCP
/// payload. `stage` is used to label any error.
fn send_l2_broadcast(
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
fn recv_dhcp(sock: &OwnedFd, xid: u32, want: MessageType) -> Result<Message> {
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

// ---------------------------------------------------------------------------
// Packet construction
// ---------------------------------------------------------------------------

/// Build the DISCOVER payload (IP + UDP + DHCP) ready for `sendto`.
fn build_discover(xid: u32, mac: &[u8; 6]) -> Result<Vec<u8>> {
    let mut msg = Message::default();
    msg.set_opcode(Opcode::BootRequest)
        .set_xid(xid)
        .set_flags(Flags::default().set_broadcast())
        .set_chaddr(mac);
    msg.opts_mut()
        .insert(DhcpOption::MessageType(MessageType::Discover));
    msg.opts_mut()
        .insert(DhcpOption::ClientIdentifier(client_identifier(mac)));
    msg.opts_mut().insert(DhcpOption::ParameterRequestList(vec![
        OptionCode::SubnetMask,
        OptionCode::Router,
        OptionCode::DomainNameServer,
        OptionCode::ServerIdentifier,
        OptionCode::AddressLeaseTime,
    ]));

    let dhcp_bytes = msg.to_vec().map_err(|e| NmblError::Io {
        source: std::io::Error::other(e.to_string()),
        context: "encoding DHCPv4 DISCOVER".to_string(),
    })?;
    Ok(wrap_ip_udp(
        Ipv4Addr::UNSPECIFIED,
        Ipv4Addr::BROADCAST,
        CLIENT_PORT,
        SERVER_PORT,
        &dhcp_bytes,
    ))
}

/// Build the REQUEST payload (IP + UDP + DHCP). Echoes the offered
/// IP via option 50 + the server identifier via option 54 so the
/// chosen server claims the lease and any other servers withdraw
/// their offers.
fn build_request(
    xid: u32,
    mac: &[u8; 6],
    offered_ip: Ipv4Addr,
    server_id: Ipv4Addr,
) -> Result<Vec<u8>> {
    let mut msg = Message::default();
    msg.set_opcode(Opcode::BootRequest)
        .set_xid(xid)
        .set_flags(Flags::default().set_broadcast())
        .set_chaddr(mac);
    msg.opts_mut()
        .insert(DhcpOption::MessageType(MessageType::Request));
    msg.opts_mut()
        .insert(DhcpOption::ClientIdentifier(client_identifier(mac)));
    msg.opts_mut()
        .insert(DhcpOption::RequestedIpAddress(offered_ip));
    msg.opts_mut()
        .insert(DhcpOption::ServerIdentifier(server_id));
    msg.opts_mut().insert(DhcpOption::ParameterRequestList(vec![
        OptionCode::SubnetMask,
        OptionCode::Router,
        OptionCode::DomainNameServer,
        OptionCode::ServerIdentifier,
        OptionCode::AddressLeaseTime,
    ]));

    let dhcp_bytes = msg.to_vec().map_err(|e| NmblError::Io {
        source: std::io::Error::other(e.to_string()),
        context: "encoding DHCPv4 REQUEST".to_string(),
    })?;
    Ok(wrap_ip_udp(
        Ipv4Addr::UNSPECIFIED,
        Ipv4Addr::BROADCAST,
        CLIENT_PORT,
        SERVER_PORT,
        &dhcp_bytes,
    ))
}

/// RFC 2132 §9.14 client identifier: type byte (0x01 = Ethernet)
/// followed by the 6-byte MAC.
fn client_identifier(mac: &[u8; 6]) -> Vec<u8> {
    let mut id = Vec::with_capacity(7);
    id.push(0x01);
    id.extend_from_slice(mac);
    id
}

/// Wrap a DHCP payload in an IPv4 + UDP header pair. The UDP checksum
/// is left as zero (per RFC 768 §"Fields", a zero checksum means
/// "unchecked" for IPv4 — DHCP servers all accept it). The IPv4
/// checksum is computed inline because the kernel will NOT fix it
/// up for `SOCK_DGRAM / AF_PACKET` sends.
fn wrap_ip_udp(
    src_ip: Ipv4Addr,
    dst_ip: Ipv4Addr,
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
) -> Vec<u8> {
    let total_len = IPV4_HEADER_LEN + UDP_HEADER_LEN + payload.len();
    let mut buf = Vec::with_capacity(total_len);

    // IPv4 header --------------------------------------------------
    buf.push(0x45); // version=4, IHL=5
    buf.push(0x00); // DSCP/ECN
    buf.extend_from_slice(&u16::try_from(total_len).unwrap_or(u16::MAX).to_be_bytes());
    buf.extend_from_slice(&0u16.to_be_bytes()); // identification
    buf.extend_from_slice(&0u16.to_be_bytes()); // flags + frag offset
    buf.push(IP_TTL_DEFAULT);
    buf.push(IPPROTO_UDP_U8);
    buf.extend_from_slice(&0u16.to_be_bytes()); // checksum placeholder
    buf.extend_from_slice(&src_ip.octets());
    buf.extend_from_slice(&dst_ip.octets());

    // Compute checksum over the (now-complete) header and patch in.
    // The 11th/12th bytes (indices 10..12) hold the checksum.
    let csum = if let Some(hdr) = buf.get(..IPV4_HEADER_LEN) {
        ipv4_checksum(hdr)
    } else {
        0
    };
    let csum_be = csum.to_be_bytes();
    if let Some(slot) = buf.get_mut(10..12) {
        slot.copy_from_slice(&csum_be);
    }

    // UDP header ---------------------------------------------------
    let udp_len = u16::try_from(UDP_HEADER_LEN + payload.len()).unwrap_or(u16::MAX);
    buf.extend_from_slice(&src_port.to_be_bytes());
    buf.extend_from_slice(&dst_port.to_be_bytes());
    buf.extend_from_slice(&udp_len.to_be_bytes());
    buf.extend_from_slice(&0u16.to_be_bytes()); // checksum: 0 = unchecked

    // Payload ------------------------------------------------------
    buf.extend_from_slice(payload);
    buf
}

/// Standard RFC 1071 16-bit one's-complement sum, suitable for the
/// IPv4 header checksum.
fn ipv4_checksum(bytes: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut iter = bytes.chunks_exact(2);
    for pair in iter.by_ref() {
        if let [hi, lo] = pair {
            sum = sum.wrapping_add(u32::from(u16::from_be_bytes([*hi, *lo])));
        }
    }
    if let Some(&tail) = iter.remainder().first() {
        sum = sum.wrapping_add(u32::from(u16::from_be_bytes([tail, 0])));
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// Strip the IPv4 + UDP headers from a received frame and return the
/// UDP payload (the DHCP wire bytes). Returns `None` on any
/// malformed frame; the caller drops the packet and re-reads.
fn strip_ip_udp(frame: &[u8]) -> Option<&[u8]> {
    let first = *frame.first()?;
    if first >> 4 != 4 {
        return None;
    }
    let ihl_words = first & 0x0f;
    if ihl_words < 5 {
        return None;
    }
    let ihl = usize::from(ihl_words) * 4;
    if frame.len() < ihl + UDP_HEADER_LEN {
        return None;
    }
    // proto field at offset 9
    if *frame.get(9)? != IPPROTO_UDP_U8 {
        return None;
    }
    // total length at offsets 2..4
    let total_len_hi = *frame.get(2)?;
    let total_len_lo = *frame.get(3)?;
    let total_len = usize::from(u16::from_be_bytes([total_len_hi, total_len_lo]));
    if total_len < ihl + UDP_HEADER_LEN || total_len > frame.len() {
        return None;
    }
    let udp_hdr_start = ihl;
    let udp_payload_start = udp_hdr_start + UDP_HEADER_LEN;
    // UDP length at udp_hdr_start+4..+6 (matches total_len - ihl).
    let udp_len_hi = *frame.get(udp_hdr_start + 4)?;
    let udp_len_lo = *frame.get(udp_hdr_start + 5)?;
    let udp_len = usize::from(u16::from_be_bytes([udp_len_hi, udp_len_lo]));
    if udp_len < UDP_HEADER_LEN {
        return None;
    }
    let udp_payload_end = udp_hdr_start + udp_len;
    if udp_payload_end > total_len {
        return None;
    }
    frame.get(udp_payload_start..udp_payload_end)
}

// ---------------------------------------------------------------------------
// Option extraction
// ---------------------------------------------------------------------------

/// Pull the two fields we need from an OFFER. `server_id` (option 54)
/// is mandatory per RFC 2131; a missing value here means the server
/// is non-compliant. We fall back to `siaddr` only if option 54 was
/// absent.
fn parsed_to_offer(msg: &Message) -> Offer {
    let server_id = msg
        .opts()
        .get(OptionCode::ServerIdentifier)
        .and_then(|o| match o {
            DhcpOption::ServerIdentifier(ip) => Some(*ip),
            _ => None,
        })
        .unwrap_or_else(|| msg.siaddr());
    Offer {
        yiaddr: msg.yiaddr(),
        server_id,
    }
}

/// Materialize a [`DhcpLease`] from an ACK. The caller provides the
/// offered IP / server ID as fallbacks in case the server omitted
/// the corresponding options in the ACK (some servers only put them
/// in the OFFER).
fn parsed_to_lease(msg: &Message, offered_ip: Ipv4Addr, server_id: Ipv4Addr) -> DhcpLease {
    let ip = if msg.yiaddr().is_unspecified() {
        offered_ip
    } else {
        msg.yiaddr()
    };

    let netmask = msg
        .opts()
        .get(OptionCode::SubnetMask)
        .and_then(|o| match o {
            DhcpOption::SubnetMask(ip) => Some(*ip),
            _ => None,
        })
        .unwrap_or(Ipv4Addr::new(255, 255, 255, 0));

    let gateway = msg.opts().get(OptionCode::Router).and_then(|o| match o {
        DhcpOption::Router(ips) => ips.first().copied(),
        _ => None,
    });

    let dns = msg
        .opts()
        .get(OptionCode::DomainNameServer)
        .and_then(|o| match o {
            DhcpOption::DomainNameServer(ips) => Some(ips.clone()),
            _ => None,
        })
        .unwrap_or_default();

    let ack_server_id = msg
        .opts()
        .get(OptionCode::ServerIdentifier)
        .and_then(|o| match o {
            DhcpOption::ServerIdentifier(ip) => Some(*ip),
            _ => None,
        })
        .unwrap_or(server_id);

    let lease_secs = msg
        .opts()
        .get(OptionCode::AddressLeaseTime)
        .and_then(|o| match o {
            DhcpOption::AddressLeaseTime(secs) => Some(*secs),
            _ => None,
        })
        .unwrap_or(0);

    DhcpLease {
        ip,
        netmask,
        gateway,
        dns,
        server_id: ack_server_id,
        lease_secs,
    }
}

// ---------------------------------------------------------------------------
// RNG
// ---------------------------------------------------------------------------

/// Fetch four random bytes from the kernel and assemble a `u32`.
/// A single `getrandom(2)` call beats pulling in a full RNG crate.
fn random_xid() -> Result<u32> {
    let mut buf = [0u8; 4];
    getrandom::fill(&mut buf).map_err(|e| NmblError::Rescue {
        stage: "dhcp-socket",
        source: Box::new(NmblError::Io {
            source: std::io::Error::other(e.to_string()),
            context: "getrandom for DHCPv4 XID".to_string(),
        }),
    })?;
    Ok(u32::from_ne_bytes(buf))
}

// ---------------------------------------------------------------------------
// Unused helper kept for symmetry: re-exports nix's typed socket() so
// callers picking up the module without root can still link.
// ---------------------------------------------------------------------------

/// Compile-time guard that `AddressFamily`, `SockType`, `SockFlag`,
/// `MsgFlags`, `SockaddrStorage`, and `socket` (which we don't use
/// in the hot path because we need `AF_PACKET` + raw protocol) are
/// kept referenced. Without this the import block would be dead.
#[allow(dead_code)]
fn _nix_imports_keepalive() {
    let _ = (
        AddressFamily::Inet,
        SockType::Datagram,
        SockFlag::SOCK_CLOEXEC,
        MsgFlags::empty(),
        |_: &SockaddrStorage| (),
        socket::<nix::sys::socket::SockProtocol>,
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests are allowed to assert with panics"
)]
mod tests {
    use super::*;

    /// RFC 1071 §3 example. The canonical worked example is:
    /// header = 45 00 00 30 44 22 40 00 80 06 00 00 8c 7c 19 ac ae 24 1e 2b
    /// → checksum = 0x442e (after fixing zero placeholder)
    /// We feed the same bytes (with the 16-bit checksum slot set to
    /// zero) and verify the function reproduces 0x442e.
    #[test]
    fn ipv4_checksum_matches_rfc1071_example() {
        let header: [u8; 20] = [
            0x45, 0x00, 0x00, 0x30, 0x44, 0x22, 0x40, 0x00, 0x80, 0x06, 0x00, 0x00, 0x8c, 0x7c,
            0x19, 0xac, 0xae, 0x24, 0x1e, 0x2b,
        ];
        assert_eq!(ipv4_checksum(&header), 0x442e);
    }

    /// Sanity: feeding the header back through the checksum (with the
    /// real value patched in) yields zero — that's the receiver-side
    /// invariant the IETF spec relies on.
    #[test]
    fn ipv4_checksum_round_trip_zero() {
        let mut header: [u8; 20] = [
            0x45, 0x00, 0x00, 0x30, 0x44, 0x22, 0x40, 0x00, 0x80, 0x06, 0x00, 0x00, 0x8c, 0x7c,
            0x19, 0xac, 0xae, 0x24, 0x1e, 0x2b,
        ];
        let csum = ipv4_checksum(&header).to_be_bytes();
        header[10] = csum[0];
        header[11] = csum[1];
        assert_eq!(ipv4_checksum(&header), 0);
    }

    /// Make sure our wrap_ip_udp produces a header whose advertised
    /// total length, UDP length, and port fields all match the input
    /// payload. Without these, the kernel silently drops our DHCP
    /// frames.
    #[test]
    fn wrap_ip_udp_sets_header_fields() {
        let payload = vec![0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let frame = wrap_ip_udp(
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::BROADCAST,
            CLIENT_PORT,
            SERVER_PORT,
            &payload,
        );

        // total_len = 20 + 8 + 6 = 34
        let total_len = u16::from_be_bytes([frame[2], frame[3]]);
        assert_eq!(total_len as usize, 20 + 8 + payload.len());

        // protocol = 17 (UDP)
        assert_eq!(frame[9], IPPROTO_UDP_U8);

        // src/dst IPs at 12..16 / 16..20
        assert_eq!(&frame[12..16], &[0, 0, 0, 0]);
        assert_eq!(&frame[16..20], &[255, 255, 255, 255]);

        // UDP src port at 20..22, dst port at 22..24
        assert_eq!(u16::from_be_bytes([frame[20], frame[21]]), CLIENT_PORT);
        assert_eq!(u16::from_be_bytes([frame[22], frame[23]]), SERVER_PORT);

        // UDP length at 24..26
        let udp_len = u16::from_be_bytes([frame[24], frame[25]]);
        assert_eq!(udp_len as usize, 8 + payload.len());

        // UDP checksum at 26..28: we deliberately set this to zero.
        assert_eq!(&frame[26..28], &[0, 0]);

        // Payload preserved verbatim at the tail.
        assert_eq!(&frame[28..], &payload[..]);

        // IPv4 header checksum is non-zero and self-consistent.
        let hdr = &frame[..20];
        let mut zeroed_hdr = [0u8; 20];
        zeroed_hdr.copy_from_slice(hdr);
        zeroed_hdr[10] = 0;
        zeroed_hdr[11] = 0;
        let recomputed = ipv4_checksum(&zeroed_hdr);
        assert_eq!(u16::from_be_bytes([hdr[10], hdr[11]]), recomputed);
    }

    /// Strip a synthetic IP+UDP wrapper and confirm we get the
    /// original payload back. Counterpart to `wrap_ip_udp` so any
    /// drift between the two functions surfaces immediately.
    #[test]
    fn strip_ip_udp_round_trips_wrap_ip_udp() {
        let payload = vec![1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        let frame = wrap_ip_udp(
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::BROADCAST,
            CLIENT_PORT,
            SERVER_PORT,
            &payload,
        );
        let stripped = strip_ip_udp(&frame).expect("strip_ip_udp must succeed");
        assert_eq!(stripped, payload.as_slice());
    }

    /// Reject malformed frames the receiver might see: short headers,
    /// wrong IP version, wrong protocol, and truncated UDP.
    #[test]
    fn strip_ip_udp_rejects_garbage() {
        // Empty.
        assert!(strip_ip_udp(&[]).is_none());
        // Wrong version (IPv6).
        let mut v6 = [0u8; 40];
        v6[0] = 0x60;
        assert!(strip_ip_udp(&v6).is_none());
        // IPv4 but IHL=4 (illegal — minimum is 5).
        let mut bad_ihl = [0u8; 20];
        bad_ihl[0] = 0x44;
        assert!(strip_ip_udp(&bad_ihl).is_none());
        // IPv4 + UDP claimed but truncated frame.
        let mut truncated = [0u8; 20];
        truncated[0] = 0x45;
        truncated[9] = IPPROTO_UDP_U8;
        // total_len says 40, but buffer is 20.
        truncated[2] = 0;
        truncated[3] = 40;
        assert!(strip_ip_udp(&truncated).is_none());
    }

    /// Hand-roll a minimal OFFER and verify parsed_to_lease pulls out
    /// IP, netmask, gateway, DNS, server-id, and lease secs.
    #[test]
    fn parsed_to_lease_extracts_all_fields() {
        let mac = [0x52u8, 0x54, 0x00, 0x12, 0x34, 0x56];
        let mut msg = Message::default();
        msg.set_opcode(Opcode::BootReply)
            .set_xid(0xdeadbeef)
            .set_yiaddr(Ipv4Addr::new(192, 168, 1, 42))
            .set_chaddr(&mac);
        msg.opts_mut()
            .insert(DhcpOption::MessageType(MessageType::Ack));
        msg.opts_mut()
            .insert(DhcpOption::SubnetMask(Ipv4Addr::new(255, 255, 255, 0)));
        msg.opts_mut()
            .insert(DhcpOption::Router(vec![Ipv4Addr::new(192, 168, 1, 1)]));
        msg.opts_mut().insert(DhcpOption::DomainNameServer(vec![
            Ipv4Addr::new(1, 1, 1, 1),
            Ipv4Addr::new(8, 8, 8, 8),
        ]));
        msg.opts_mut()
            .insert(DhcpOption::ServerIdentifier(Ipv4Addr::new(192, 168, 1, 1)));
        msg.opts_mut().insert(DhcpOption::AddressLeaseTime(3600));

        let lease = parsed_to_lease(&msg, Ipv4Addr::UNSPECIFIED, Ipv4Addr::UNSPECIFIED);
        assert_eq!(lease.ip, Ipv4Addr::new(192, 168, 1, 42));
        assert_eq!(lease.netmask, Ipv4Addr::new(255, 255, 255, 0));
        assert_eq!(lease.gateway, Some(Ipv4Addr::new(192, 168, 1, 1)));
        assert_eq!(
            lease.dns,
            vec![Ipv4Addr::new(1, 1, 1, 1), Ipv4Addr::new(8, 8, 8, 8)]
        );
        assert_eq!(lease.server_id, Ipv4Addr::new(192, 168, 1, 1));
        assert_eq!(lease.lease_secs, 3600);
    }

    /// Confirm parsed_to_offer falls back to siaddr when option 54
    /// is missing — defensive against half-broken servers.
    #[test]
    fn parsed_to_offer_falls_back_to_siaddr() {
        let mut msg = Message::default();
        msg.set_opcode(Opcode::BootReply)
            .set_xid(1)
            .set_yiaddr(Ipv4Addr::new(10, 0, 0, 5))
            .set_siaddr(Ipv4Addr::new(10, 0, 0, 1));
        msg.opts_mut()
            .insert(DhcpOption::MessageType(MessageType::Offer));
        // No ServerIdentifier inserted.
        let offer = parsed_to_offer(&msg);
        assert_eq!(offer.yiaddr, Ipv4Addr::new(10, 0, 0, 5));
        assert_eq!(offer.server_id, Ipv4Addr::new(10, 0, 0, 1));
    }

    /// Encode a DISCOVER and feed it back through `Message::decode` —
    /// proves the wire is well-formed and `MessageType` survives the
    /// round trip.
    #[test]
    fn build_discover_round_trips_through_decoder() {
        let mac = [0xde, 0xad, 0xbe, 0xef, 0x00, 0x01];
        let frame = build_discover(0xcafebabe, &mac).expect("build_discover");
        let payload = strip_ip_udp(&frame).expect("strip_ip_udp");
        let msg = Message::decode(&mut Decoder::new(payload)).expect("decode");
        assert_eq!(msg.xid(), 0xcafebabe);
        assert_eq!(msg.opts().msg_type(), Some(MessageType::Discover));
        assert_eq!(msg.chaddr(), &mac[..]);
        assert!(msg.flags().broadcast());
    }

    /// Same round-trip for REQUEST, plus assertions on the two
    /// options REQUEST uniquely sets.
    #[test]
    fn build_request_carries_requested_ip_and_server_id() {
        let mac = [0xde, 0xad, 0xbe, 0xef, 0x00, 0x02];
        let offered = Ipv4Addr::new(10, 0, 0, 99);
        let server = Ipv4Addr::new(10, 0, 0, 1);
        let frame = build_request(0x12345678, &mac, offered, server).expect("build_request");
        let payload = strip_ip_udp(&frame).expect("strip_ip_udp");
        let msg = Message::decode(&mut Decoder::new(payload)).expect("decode");

        assert_eq!(msg.opts().msg_type(), Some(MessageType::Request));
        match msg.opts().get(OptionCode::RequestedIpAddress) {
            Some(DhcpOption::RequestedIpAddress(ip)) => assert_eq!(*ip, offered),
            other => panic!("expected RequestedIpAddress, got {other:?}"),
        }
        match msg.opts().get(OptionCode::ServerIdentifier) {
            Some(DhcpOption::ServerIdentifier(ip)) => assert_eq!(*ip, server),
            other => panic!("expected ServerIdentifier, got {other:?}"),
        }
    }

    /// per_retry_timeout must stay inside [MIN_PER_RETRY, MAX_PER_RETRY]
    /// regardless of attempt count, and must never exceed the
    /// remaining wall-clock budget.
    #[test]
    fn per_retry_timeout_stays_bounded() {
        let huge = Duration::from_secs(3600);
        assert!(per_retry_timeout(0, huge) >= MIN_PER_RETRY);
        assert!(per_retry_timeout(0, huge) <= MAX_PER_RETRY);
        assert!(per_retry_timeout(10, huge) <= MAX_PER_RETRY);

        let tiny = Duration::from_millis(50);
        // Clamps DOWN to the remaining budget, not up to MIN_PER_RETRY.
        assert!(per_retry_timeout(5, tiny) <= tiny.max(MIN_PER_RETRY));
    }

    /// Acquiring a lease requires CAP_NET_RAW and a real network. We
    /// keep the test as a documented `#[ignore]` smoke marker so
    /// future maintainers can flip the gate when running in a VM.
    #[test]
    #[ignore = "needs CAP_NET_RAW and a real DHCP server"]
    fn acquire_smoke_requires_net_admin() {
        // Intentionally empty: this exists as a discoverable marker.
    }
}
