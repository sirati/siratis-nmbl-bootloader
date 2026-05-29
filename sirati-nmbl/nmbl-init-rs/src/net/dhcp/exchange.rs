//! DISCOVER / REQUEST retry loops and timing helpers.

use std::net::Ipv4Addr;
use std::os::fd::OwnedFd;
use std::time::{Duration, Instant};

use dhcproto::v4::MessageType;

use crate::error::{NmblError, Result};
use crate::net::iface::Interface;

use super::options::{Offer, parsed_to_lease, parsed_to_offer};
use super::packet::{build_discover, build_request};
use super::socket::{recv_dhcp, send_l2_broadcast, set_recv_timeout};
use super::{DhcpLease, MAX_PER_RETRY, MAX_RETRIES, MIN_PER_RETRY};

// ---------------------------------------------------------------------------
// DISCOVER / REQUEST loops
// ---------------------------------------------------------------------------

/// Send DISCOVERs until the server responds with an OFFER (or we
/// exhaust the deadline / retry budget). The returned [`Offer`]
/// captures the two fields REQUEST needs.
pub(super) fn discover_until_offer(
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
pub(super) fn request_until_ack(
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
pub(super) fn per_retry_timeout(attempt: u32, remaining: Duration) -> Duration {
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
pub(super) fn remaining_or_timeout(deadline: Instant) -> Result<Duration> {
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
