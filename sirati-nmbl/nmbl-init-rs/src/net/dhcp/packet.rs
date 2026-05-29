//! Packet construction and IP/UDP framing helpers.

use std::net::Ipv4Addr;

use dhcproto::Encodable;
use dhcproto::v4::{
    CLIENT_PORT, DhcpOption, Flags, Message, MessageType, Opcode, OptionCode, SERVER_PORT,
};

use crate::error::{NmblError, Result};

use super::{IP_TTL_DEFAULT, IPPROTO_UDP_U8, IPV4_HEADER_LEN, UDP_HEADER_LEN};

// ---------------------------------------------------------------------------
// Packet construction
// ---------------------------------------------------------------------------

/// Build the DISCOVER payload (IP + UDP + DHCP) ready for `sendto`.
pub(super) fn build_discover(xid: u32, mac: &[u8; 6]) -> Result<Vec<u8>> {
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
pub(super) fn build_request(
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
pub(super) fn wrap_ip_udp(
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
pub(super) fn ipv4_checksum(bytes: &[u8]) -> u16 {
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
pub(super) fn strip_ip_udp(frame: &[u8]) -> Option<&[u8]> {
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
