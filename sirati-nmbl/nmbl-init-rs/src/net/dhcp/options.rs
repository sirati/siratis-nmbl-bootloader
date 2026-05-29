//! DHCP option extraction and RNG helpers.

use std::net::Ipv4Addr;

use dhcproto::v4::{DhcpOption, Message, OptionCode};

use crate::error::{NmblError, Result};
use crate::nmbl_warn;

use super::DhcpLease;

// ---------------------------------------------------------------------------
// Option extraction
// ---------------------------------------------------------------------------

/// Internal handoff between the DISCOVER and REQUEST phases.
pub(super) struct Offer {
    pub(super) yiaddr: Ipv4Addr,
    pub(super) server_id: Ipv4Addr,
}

/// Pull the two fields we need from an OFFER. `server_id` (option 54)
/// is mandatory per RFC 2131; a missing value here means the server
/// is non-compliant. We fall back to `siaddr` only if option 54 was
/// absent — and warn loudly so the operator can chase the broken
/// server instead of silently relying on the fallback.
pub(super) fn parsed_to_offer(msg: &Message) -> Offer {
    let server_id = msg
        .opts()
        .get(OptionCode::ServerIdentifier)
        .and_then(|o| match o {
            DhcpOption::ServerIdentifier(ip) => Some(*ip),
            _ => None,
        })
        .unwrap_or_else(|| {
            let siaddr = msg.siaddr();
            nmbl_warn!(
                "dhcp: OFFER missing option 54 (Server Identifier); falling back to siaddr {siaddr}"
            );
            siaddr
        });
    Offer {
        yiaddr: msg.yiaddr(),
        server_id,
    }
}

/// Materialize a [`DhcpLease`] from an ACK. The caller provides the
/// offered IP / server ID as fallbacks in case the server omitted
/// the corresponding options in the ACK (some servers only put them
/// in the OFFER).
pub(super) fn parsed_to_lease(
    msg: &Message,
    offered_ip: Ipv4Addr,
    server_id: Ipv4Addr,
) -> DhcpLease {
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
pub(super) fn random_xid() -> Result<u32> {
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
