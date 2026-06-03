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

/// Decode a hex string of EXACTLY `N` bytes into a fixed array, fail-closed.
///
/// Returns `None` on any non-hex character, an odd length, or a length that is
/// not exactly `2 * N` characters (case-insensitive). Used by the secure-boot
/// priority gate to parse operator-supplied key-fingerprint strings into the
/// full 32-byte `FullFp` it narrows on (FIX-08) — a malformed fingerprint
/// narrows to nothing rather than being silently truncated or padded.
///
/// `allow(dead_code)`: the only caller is the `secure-boot`-gated priority
/// gate, so a feature-free build has no caller, but the helper must still
/// compile so `util::hex` is buildable in every configuration.
#[allow(
    dead_code,
    reason = "the sole caller is the secure-boot priority gate; must compile everywhere"
)]
pub(crate) fn decode_fixed<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    let bytes = s.as_bytes();
    for (i, slot) in out.iter_mut().enumerate() {
        // `i * 2 + 1 < s.len()` holds because `s.len() == N * 2` and `i < N`.
        let hi = nibble(*bytes.get(i * 2)?)?;
        let lo = nibble(*bytes.get(i * 2 + 1)?)?;
        *slot = (hi << 4) | lo;
    }
    Some(out)
}

/// One hex nibble (0–15) from an ASCII byte, or `None` if it is not a hex
/// digit. Case-insensitive.
#[allow(
    dead_code,
    reason = "the sole caller is the secure-boot priority gate; must compile everywhere"
)]
fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{decode_fixed, hex_lower};

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

    #[test]
    fn decode_round_trips_lower_and_upper() {
        let bytes = [0x00u8, 0x0f, 0xf0, 0xff];
        assert_eq!(decode_fixed::<4>(&hex_lower(&bytes)), Some(bytes));
        assert_eq!(decode_fixed::<4>("000FF0FF"), Some(bytes));
    }

    #[test]
    fn decode_rejects_wrong_length() {
        assert_eq!(decode_fixed::<4>("00"), None);
        assert_eq!(decode_fixed::<4>("000ff0ff00"), None);
        assert_eq!(decode_fixed::<4>("000ff0f"), None);
    }

    #[test]
    fn decode_rejects_non_hex() {
        assert_eq!(decode_fixed::<2>("zz00"), None);
        assert_eq!(decode_fixed::<2>("00 0"), None);
    }
}
