//! Lowercase-hex encoding (UNGATED, no `sha2` dependency).
//!
//! Lifted out of `rescue::net::download` (FIX-23 / master-plan §B.1) so both
//! the network-rescue digest path and the secure-boot signature/measure paths
//! share ONE encoder rather than each pulling in the `hex` crate for a
//! 64-byte string. This module compiles in every build — it touches no
//! optional dependency.

/// Lowercase hex encoder for digest bytes. Avoids pulling in the `hex` crate
/// for a fixed-size string.
///
/// `allow(dead_code)`: callers are feature-conditional — the `network-rescue`
/// download path consumes it, and the `secure-boot` digest/measure paths will
/// (F4+). In configurations where none of those modules compile (e.g. the
/// default feature-free build, or `secure-boot` before its consumers land)
/// this crate-internal helper has no caller, but it must still compile so
/// `util::hex` is buildable in every configuration (FIX-23).
#[inline]
#[allow(
    dead_code,
    reason = "callers are feature-conditional; must compile everywhere"
)]
pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let hi = TABLE.get(usize::from(b >> 4)).copied().unwrap_or(b'?');
        let lo = TABLE.get(usize::from(b & 0x0f)).copied().unwrap_or(b'?');
        out.push(hi as char);
        out.push(lo as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::hex_lower;

    #[test]
    fn pads_single_byte_with_zero() {
        assert_eq!(hex_lower(&[0x0a]), "0a");
        assert_eq!(hex_lower(&[0xff, 0x00, 0x10]), "ff0010");
    }

    #[test]
    fn empty_input_yields_empty_string() {
        assert_eq!(hex_lower(&[]), "");
    }

    #[test]
    fn encodes_full_byte_range_boundaries() {
        assert_eq!(hex_lower(&[0x00, 0x0f, 0xf0, 0xff]), "000ff0ff");
    }
}
