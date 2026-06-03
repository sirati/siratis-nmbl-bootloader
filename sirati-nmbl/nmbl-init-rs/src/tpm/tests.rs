//! Unit tests for the always-compiled TPM core: golden marshal vectors for
//! `pcr_extend`/`pcr_read` request frames, the FIX-27 error-RC ⇒ `Failed`
//! response path, the FIX-38 poison self-check, and the deterministic
//! presence probe (FIX-28).
//!
//! The golden vectors are reconstructed BY HAND in the test from the TCG
//! TPM 2.0 wire layout, then compared against what `tpm2-protocol` produced
//! through our `build_*_request` helpers. The two paths must agree, so an
//! upstream marshaling change (or a bug in our command shape) is caught here
//! rather than at the wire to a real TPM.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "tests assert on exact wire bytes and contract failures"
)]

use tpm2_protocol::data::TpmCc;

use super::commands::{build_pcr_extend_request, build_pcr_read_request, parse_checked_response};
use super::presence::tpm_present_at;
use super::{LOCK_PCR, RELOCK_POISON};

/// `TPM_ST_SESSIONS` / `TPM_ST_NO_SESSIONS` tag bytes.
const ST_SESSIONS: [u8; 2] = [0x80, 0x02];
const ST_NO_SESSIONS: [u8; 2] = [0x80, 0x01];
/// `TPM_CC_PCR_Extend` / `TPM_CC_PCR_Read`.
const CC_PCR_EXTEND: [u8; 4] = [0x00, 0x00, 0x01, 0x82];
const CC_PCR_READ: [u8; 4] = [0x00, 0x00, 0x01, 0x7E];
/// `TPM_ALG_SHA256`.
const ALG_SHA256: [u8; 2] = [0x00, 0x0B];
/// `TPM_RS_PW`.
const RS_PW: [u8; 4] = [0x40, 0x00, 0x00, 0x09];

/// Hand-built `TPM2_PCR_Extend` request for a single SHA-256 digest into
/// `pcr_index`. This is the independent reconstruction the production
/// marshaler must match.
fn golden_pcr_extend(pcr_index: u32, digest: &[u8; 32]) -> Vec<u8> {
    let mut body = Vec::new();
    // handle area: the PCR handle.
    body.extend_from_slice(&pcr_index.to_be_bytes());
    // authorization area: size-prefixed single empty-password session.
    let mut session = Vec::new();
    session.extend_from_slice(&RS_PW); // session handle
    session.extend_from_slice(&[0x00, 0x00]); // nonce (TPM2B size 0)
    session.push(0x00); // session attributes
    session.extend_from_slice(&[0x00, 0x00]); // hmac (TPM2B size 0)
    body.extend_from_slice(&(session.len() as u32).to_be_bytes());
    body.extend_from_slice(&session);
    // parameters: TPML_DIGEST_VALUES { count, TPMT_HA { alg, digest } }.
    body.extend_from_slice(&1u32.to_be_bytes());
    body.extend_from_slice(&ALG_SHA256);
    body.extend_from_slice(digest);

    let mut frame = Vec::new();
    frame.extend_from_slice(&ST_SESSIONS);
    frame.extend_from_slice(&((10 + body.len()) as u32).to_be_bytes());
    frame.extend_from_slice(&CC_PCR_EXTEND);
    frame.extend_from_slice(&body);
    frame
}

/// Hand-built `TPM2_PCR_Read` request selecting one PCR in the SHA-256 bank.
fn golden_pcr_read(pcr_index: u32) -> Vec<u8> {
    let byte = (pcr_index / 8) as usize;
    let bit = (pcr_index % 8) as u8;
    let mut bitmap = vec![0u8; byte + 1];
    bitmap[byte] = 1u8 << bit;

    let mut body = Vec::new();
    // TPML_PCR_SELECTION { count, TPMS_PCR_SELECTION { hash, sizeofSelect, pcrSelect } }.
    body.extend_from_slice(&1u32.to_be_bytes());
    body.extend_from_slice(&ALG_SHA256);
    body.push(bitmap.len() as u8);
    body.extend_from_slice(&bitmap);

    let mut frame = Vec::new();
    frame.extend_from_slice(&ST_NO_SESSIONS);
    frame.extend_from_slice(&((10 + body.len()) as u32).to_be_bytes());
    frame.extend_from_slice(&CC_PCR_READ);
    frame.extend_from_slice(&body);
    frame
}

#[test]
fn pcr_extend_golden_vector() {
    let got = build_pcr_extend_request(LOCK_PCR, &RELOCK_POISON).expect("build pcr_extend");
    let want = golden_pcr_extend(LOCK_PCR, &RELOCK_POISON);
    assert_eq!(got, want, "\n got: {}\nwant: {}", hex(&got), hex(&want));
    // Spot-check the load-bearing header fields explicitly.
    assert_eq!(&got[0..2], &ST_SESSIONS, "tag must be TPM_ST_SESSIONS");
    assert_eq!(&got[6..10], &CC_PCR_EXTEND, "cc must be PcrExtend");
    assert_eq!(&got[10..14], &LOCK_PCR.to_be_bytes(), "handle == LOCK_PCR");
    // The last 32 bytes are exactly the poison.
    assert_eq!(&got[got.len() - 32..], &RELOCK_POISON);
}

#[test]
fn pcr_read_golden_vector() {
    let got = build_pcr_read_request(LOCK_PCR).expect("build pcr_read");
    let want = golden_pcr_read(LOCK_PCR);
    assert_eq!(got, want, "\n got: {}\nwant: {}", hex(&got), hex(&want));
    assert_eq!(
        &got[0..2],
        &ST_NO_SESSIONS,
        "tag must be TPM_ST_NO_SESSIONS"
    );
    assert_eq!(&got[6..10], &CC_PCR_READ, "cc must be PcrRead");
}

/// FIX-27: a well-formed response carrying a NON-success response code must
/// be surfaced as an error (which the cap path turns into `Failed`), NOT
/// silently treated as success.
#[test]
fn error_rc_is_failed() {
    // A short error response: tag=NO_SESSIONS, size=10, rc=TPM_RC_FAILURE
    // (0x00000101). The TPM emits exactly this 10-byte frame on a hard error.
    let mut frame = Vec::new();
    frame.extend_from_slice(&ST_NO_SESSIONS);
    frame.extend_from_slice(&10u32.to_be_bytes());
    frame.extend_from_slice(&0x0000_0101u32.to_be_bytes()); // TPM_RC_FAILURE
    let err =
        parse_checked_response("pcr_extend", TpmCc::PcrExtend, &frame).expect_err("non-success RC");
    match err {
        crate::error::NmblError::TpmProto { context, reason } => {
            assert_eq!(context, "pcr_extend");
            assert!(
                reason.contains("response code") || reason.contains("0x"),
                "reason should carry the RC: {reason}"
            );
        }
        other => panic!("expected TpmProto, got {other:?}"),
    }
}

/// A success-RC response with a valid PcrExtend body parses cleanly.
#[test]
fn success_rc_parses() {
    // tag=NO_SESSIONS, size=10, rc=TPM_RC_SUCCESS (0). PcrExtend has an empty
    // parameter/handle area, so the 10-byte header is a complete response.
    let mut frame = Vec::new();
    frame.extend_from_slice(&ST_NO_SESSIONS);
    frame.extend_from_slice(&10u32.to_be_bytes());
    frame.extend_from_slice(&0u32.to_be_bytes()); // TPM_RC_SUCCESS
    parse_checked_response("pcr_extend", TpmCc::PcrExtend, &frame)
        .expect("success RC parses")
        .PcrExtend()
        .expect("body is a PcrExtend response");
}

/// FIX-38: `RELOCK_POISON` is exactly `SHA-256(RELOCK_POISON_PREIMAGE)`. The
/// committed literal is recomputed from the preimage and asserted equal, so
/// the two can never silently drift. Gated on the cfgs that actually pull
/// `sha2` (it is an optional dep — FIX-09); the `--all-features` test run
/// always satisfies it.
#[cfg(any(feature = "secure-boot", feature = "network-rescue"))]
#[test]
fn poison_self_check() {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(crate::security_consts::RELOCK_POISON_PREIMAGE);
    let computed = hasher.finalize();
    assert_eq!(
        computed.as_slice(),
        &RELOCK_POISON,
        "RELOCK_POISON must equal SHA-256(RELOCK_POISON_PREIMAGE)"
    );
}

/// The lock PCR is single-sourced from `security_consts` and must be 11.
#[test]
fn lock_pcr_is_eleven() {
    assert_eq!(LOCK_PCR, 11);
    assert_eq!(LOCK_PCR, crate::security_consts::LOCK_PCR);
}

/// FIX-28: presence is a deterministic sysfs fact. An existing node ⇒
/// present; a missing node ⇒ absent. No timing.
#[test]
fn presence_is_deterministic() {
    let dir = tempfile::tempdir().expect("tempdir");
    let present = dir.path().join("tpm0");
    let absent = dir.path().join("nope");
    std::fs::create_dir(&present).expect("create node");
    assert!(tpm_present_at(&present), "existing node ⇒ present");
    assert!(!tpm_present_at(&absent), "missing node ⇒ absent");
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}
