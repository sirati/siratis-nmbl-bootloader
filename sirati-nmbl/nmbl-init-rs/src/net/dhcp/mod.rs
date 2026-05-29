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

use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use crate::error::Result;
use crate::net::iface::Interface;

mod exchange;
mod options;
mod packet;
mod socket;

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
    let xid = options::random_xid()?;

    let sock = socket::open_packet_socket(iface)?;

    // OFFER phase --------------------------------------------------
    let offer = exchange::discover_until_offer(&sock, iface, xid, deadline)?;
    let server_id = offer.server_id;
    let offered_ip = offer.yiaddr;

    // REQUEST/ACK phase --------------------------------------------
    let ack = exchange::request_until_ack(&sock, iface, xid, offered_ip, server_id, deadline)?;
    Ok(ack)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
