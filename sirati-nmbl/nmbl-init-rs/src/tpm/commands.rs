//! TPM 2.0 `PcrExtend` / `PcrRead` and the lock-PCR cap, built on
//! `tpm2-protocol` marshaling with EXPLICIT response-code checking (FIX-27).
//!
//! Every transact's response is unmarshaled with `tpm_unmarshal_response`,
//! whose `Ok(Err(rc))` arm carries a NON-success TPM response code. We treat
//! ANY such RC — and any transport/marshal failure — as a hard error
//! (`NmblError::TpmProto`); the cap path converts that into
//! [`CapOutcome::Failed`], never a benign no-TPM (FIX-27).

use tpm2_protocol::TpmWriter;
use tpm2_protocol::basic::TpmHandle;
use tpm2_protocol::data::{
    TpmAlgId, TpmCc, TpmSt, TpmsAuthCommand, TpmsPcrSelection, TpmtHa, TpmuHa,
};
use tpm2_protocol::frame::{
    TpmFrame, TpmPcrExtendCommand, TpmPcrReadCommand, TpmResponseValue, tpm_marshal_command,
    tpm_unmarshal_response,
};

use crate::error::{NmblError, Result};

use super::presence::tpm_present;
use super::transport::TpmDevice;
use super::{CapOutcome, LOCK_PCR, RELOCK_POISON};

/// `TPM_RS_PW` — the well-known password authorization session handle. A
/// `PcrExtend` needs a session; the platform-default empty-password session
/// authorizes an extend of a normal PCR without a started HMAC session.
const TPM_RS_PW: u32 = 0x4000_0009;

/// Scratch buffer for a marshaled command frame. A `PcrExtend` of a single
/// SHA-256 digest is ~60 bytes; 512 is comfortable headroom and keeps the
/// frame on the stack (no heap allocation on the cap path).
const COMMAND_SCRATCH: usize = 512;

/// Builds the empty-password (`TPM_RS_PW`) authorization session used to
/// authorize a `PcrExtend`. All sub-fields default to empty; only the
/// session handle is set.
fn pw_session() -> TpmsAuthCommand {
    TpmsAuthCommand {
        session_handle: TpmHandle::new(TPM_RS_PW),
        ..Default::default()
    }
}

/// Wraps a raw digest into a `TpmtHa` for the SHA-256 bank, failing closed if
/// the digest does not fit the TPM digest buffer.
fn sha256_digest(digest: &[u8]) -> Result<TpmtHa> {
    let buf =
        tpm2_protocol::basic::TpmBuffer::try_from(digest).map_err(|e| NmblError::TpmProto {
            context: "pcr_extend".to_string(),
            reason: format!(
                "digest length {} exceeds TPM digest buffer: {e}",
                digest.len()
            ),
        })?;
    Ok(TpmtHa {
        hash_alg: TpmAlgId::Sha256,
        digest: TpmuHa::Digest(buf),
    })
}

/// Marshals `command` (with the given `tag` + `sessions`) into a stack frame
/// and returns the owned wire bytes.
fn marshal<C>(
    context: &str,
    command: &C,
    tag: TpmSt,
    sessions: &[TpmsAuthCommand],
) -> Result<Vec<u8>>
where
    C: TpmFrame,
{
    let mut scratch = [0u8; COMMAND_SCRATCH];
    let mut writer = TpmWriter::new(&mut scratch);
    tpm_marshal_command(command, tag, sessions, &mut writer).map_err(|e| NmblError::TpmProto {
        context: context.to_string(),
        reason: format!("marshal command: {e}"),
    })?;
    Ok(writer.as_bytes().to_vec())
}

/// Transacts `request` and unmarshals the response for command code `cc`,
/// surfacing a NON-success TPM response code as a hard `TpmProto` error
/// (FIX-27). Returns the typed response value on success.
fn transact_checked(
    dev: &TpmDevice,
    context: &str,
    cc: TpmCc,
    request: &[u8],
) -> Result<TpmResponseValue> {
    let response = dev.transact(request)?;
    parse_checked_response(context, cc, &response)
}

/// Unmarshals `response` for command code `cc` and ENFORCES the response code
/// (FIX-27): a malformed frame OR a non-success RC ⇒ `Err(TpmProto)`. Pure
/// (no IO) so the error-RC-⇒-Failed test can exercise it directly.
pub(crate) fn parse_checked_response(
    context: &str,
    cc: TpmCc,
    response: &[u8],
) -> Result<TpmResponseValue> {
    let parsed = tpm_unmarshal_response(cc, response).map_err(|e| NmblError::TpmProto {
        context: context.to_string(),
        reason: format!("unmarshal response: {e}"),
    })?;
    // FIX-27: `Ok(Err(rc))` is a well-formed response carrying a non-success
    // response code. It is NOT a parse error — it is the TPM telling us the
    // command failed — and MUST be treated as fail-closed.
    let (body, _sessions) = match parsed {
        Ok(ok) => ok,
        Err(rc) => {
            return Err(NmblError::TpmProto {
                context: context.to_string(),
                reason: format!("tpm response code {rc} (0x{:08x})", rc.value()),
            });
        }
    };
    Ok(body)
}

/// Marshals a `TPM2_PCR_Extend` request frame (SHA-256 bank) for
/// `pcr_index`/`digest`, with the empty-password session. Pure (no IO) so the
/// golden-vector test can assert the exact wire bytes (FIX-27 vector pinning).
pub(crate) fn build_pcr_extend_request(pcr_index: u32, digest: &[u8]) -> Result<Vec<u8>> {
    let ha = sha256_digest(digest)?;
    let mut digests = tpm2_protocol::data::TpmlDigestValues::new();
    digests.try_push(ha).map_err(|e| NmblError::TpmProto {
        context: "pcr_extend".to_string(),
        reason: format!("push digest value: {e}"),
    })?;
    let command = TpmPcrExtendCommand {
        handles: [TpmHandle::new(pcr_index)],
        digests,
    };
    // PcrExtend requires an authorization session ⇒ `TpmSt::Sessions`.
    marshal("pcr_extend", &command, TpmSt::Sessions, &[pw_session()])
}

/// `TPM2_PCR_Extend` of `digest` (SHA-256 bank) into PCR `pcr_index`.
///
/// Builds the command + empty-password session, marshals it, transacts, and
/// checks the response code (a non-success RC ⇒ `Err` — FIX-27). The extend
/// itself returns no parameters; success is the RC check passing.
pub fn pcr_extend(dev: &TpmDevice, pcr_index: u32, digest: &[u8]) -> Result<()> {
    let request = build_pcr_extend_request(pcr_index, digest)?;
    let body = transact_checked(dev, "pcr_extend", TpmCc::PcrExtend, &request)?;
    // Defensive: confirm the response really was a PcrExtend response.
    body.PcrExtend().map_err(|_| NmblError::TpmProto {
        context: "pcr_extend".to_string(),
        reason: "unexpected response body for PcrExtend".to_string(),
    })?;
    Ok(())
}

/// Marshals a `TPM2_PCR_Read` request frame (SHA-256 bank) for `pcr_index`,
/// no session. Pure (no IO) for golden-vector pinning.
pub(crate) fn build_pcr_read_request(pcr_index: u32) -> Result<Vec<u8>> {
    let selection = pcr_selection(pcr_index)?;
    let mut pcr_selection_in = tpm2_protocol::data::TpmlPcrSelection::new();
    pcr_selection_in
        .try_push(selection)
        .map_err(|e| NmblError::TpmProto {
            context: "pcr_read".to_string(),
            reason: format!("push pcr selection: {e}"),
        })?;
    let command = TpmPcrReadCommand {
        handles: [],
        pcr_selection_in,
    };
    marshal("pcr_read", &command, TpmSt::NoSessions, &[])
}

/// `TPM2_PCR_Read` of a single PCR `pcr_index` (SHA-256 bank). Returns the
/// concatenation of the returned PCR values (one 32-byte digest for a single
/// allocated bank, empty if the bank/PCR is not allocated). No session
/// (`TpmSt::NoSessions`).
pub fn pcr_read(dev: &TpmDevice, pcr_index: u32) -> Result<Vec<u8>> {
    let request = build_pcr_read_request(pcr_index)?;
    let body = transact_checked(dev, "pcr_read", TpmCc::PcrRead, &request)?;
    let read = body.PcrRead().map_err(|_| NmblError::TpmProto {
        context: "pcr_read".to_string(),
        reason: "unexpected response body for PcrRead".to_string(),
    })?;
    let mut out = Vec::new();
    for value in read.pcr_values.iter() {
        out.extend_from_slice(value.as_ref());
    }
    Ok(out)
}

/// Builds a `TpmsPcrSelection` selecting exactly `pcr_index` in the SHA-256
/// bank. The selection bitmap is `ceil((pcr_index+1)/8)` bytes with one bit
/// set; the TPM expects at least the bytes covering the requested PCR.
fn pcr_selection(pcr_index: u32) -> Result<TpmsPcrSelection> {
    let byte = (pcr_index / 8) as usize;
    let bit = (pcr_index % 8) as u8;
    let mut bitmap = vec![0u8; byte + 1];
    if let Some(slot) = bitmap.get_mut(byte) {
        *slot = 1u8 << bit;
    }
    let pcr_select =
        tpm2_protocol::data::TpmsPcrSelect::try_from(bitmap.as_slice()).map_err(|e| {
            NmblError::TpmProto {
                context: "pcr_read".to_string(),
                reason: format!("build pcr select bitmap: {e}"),
            }
        })?;
    Ok(TpmsPcrSelection {
        hash: TpmAlgId::Sha256,
        pcr_select,
    })
}

/// Caps (poisons) PCR `pcr_index` by extending `poison` (SHA-256 bank),
/// returning the rich [`CapOutcome`] the policy layer consumes (R-7 /
/// FIX-27).
///
/// Order of decisions:
/// 1. No TPM present (deterministic sysfs check — FIX-28) ⇒ [`CapOutcome::NoTpm`].
/// 2. TPM present but the device cannot be opened, OR the extend transact /
///    marshal / response-code check fails ⇒ [`CapOutcome::Failed`]
///    (fail-closed; a present-but-uncappable TPM is NEVER `NoTpm`).
/// 3. Extend succeeds with `TPM_RC_SUCCESS` ⇒ [`CapOutcome::Capped`].
#[must_use]
pub fn cap_pcr_outcome(pcr_index: u32, poison: &[u8; 32]) -> CapOutcome {
    if !tpm_present() {
        return CapOutcome::NoTpm;
    }
    let dev = match TpmDevice::open() {
        Ok(dev) => dev,
        // Present per sysfs but unopenable ⇒ fail-closed, not NoTpm (FIX-27).
        Err(e) => return CapOutcome::Failed(e),
    };
    match pcr_extend(&dev, pcr_index, poison) {
        Ok(()) => CapOutcome::Capped,
        Err(e) => CapOutcome::Failed(e),
    }
}

/// Thin `Result`-shaped wrapper over [`cap_pcr_outcome`] for callers that
/// only care about cap-or-not. `NoTpm` maps to `Ok(())` (nothing to cap is
/// not an error); `Failed(e)` propagates `e`. The rich [`cap_pcr_outcome`]
/// is what the policy layer consumes for the degrade-open-vs-fail-closed
/// decision; this exists for the simple "best-effort cap" call sites.
pub fn cap_pcr(pcr_index: u32, poison: &[u8; 32]) -> Result<()> {
    match cap_pcr_outcome(pcr_index, poison) {
        CapOutcome::Capped | CapOutcome::NoTpm => Ok(()),
        CapOutcome::Failed(e) => Err(e),
    }
}

/// Caps the configured [`LOCK_PCR`] with the committed [`RELOCK_POISON`]
/// (the common call shape for the guard). Equivalent to
/// `cap_pcr_outcome(LOCK_PCR, &RELOCK_POISON)`.
#[must_use]
pub fn cap_lock_pcr() -> CapOutcome {
    cap_pcr_outcome(LOCK_PCR, &RELOCK_POISON)
}
