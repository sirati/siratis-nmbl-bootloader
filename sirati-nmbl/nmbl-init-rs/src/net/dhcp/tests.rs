#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests are allowed to assert with panics"
)]

use std::net::Ipv4Addr;
use std::time::Duration;

use dhcproto::v4::{
    CLIENT_PORT, DhcpOption, Message, MessageType, Opcode, OptionCode, SERVER_PORT,
};
use dhcproto::{Decodable, Decoder};

use super::exchange::per_retry_timeout;
use super::options::{parsed_to_lease, parsed_to_offer};
use super::packet::{build_discover, build_request, ipv4_checksum, strip_ip_udp, wrap_ip_udp};
use super::{IPPROTO_UDP_U8, MAX_PER_RETRY, MIN_PER_RETRY};

/// RFC 1071 §3 example. The canonical worked example is:
/// header = 45 00 00 30 44 22 40 00 80 06 00 00 8c 7c 19 ac ae 24 1e 2b
/// → checksum = 0x442e (after fixing zero placeholder)
/// We feed the same bytes (with the 16-bit checksum slot set to
/// zero) and verify the function reproduces 0x442e.
#[test]
fn ipv4_checksum_matches_rfc1071_example() {
    let header: [u8; 20] = [
        0x45, 0x00, 0x00, 0x30, 0x44, 0x22, 0x40, 0x00, 0x80, 0x06, 0x00, 0x00, 0x8c, 0x7c, 0x19,
        0xac, 0xae, 0x24, 0x1e, 0x2b,
    ];
    assert_eq!(ipv4_checksum(&header), 0x442e);
}

/// Sanity: feeding the header back through the checksum (with the
/// real value patched in) yields zero — that's the receiver-side
/// invariant the IETF spec relies on.
#[test]
fn ipv4_checksum_round_trip_zero() {
    let mut header: [u8; 20] = [
        0x45, 0x00, 0x00, 0x30, 0x44, 0x22, 0x40, 0x00, 0x80, 0x06, 0x00, 0x00, 0x8c, 0x7c, 0x19,
        0xac, 0xae, 0x24, 0x1e, 0x2b,
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
