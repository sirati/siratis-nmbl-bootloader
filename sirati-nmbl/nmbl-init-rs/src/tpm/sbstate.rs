//! Runtime Secure-Boot state awareness (FIX-11 / R-9). ALWAYS-COMPILED.
//!
//! At the START of the measured path NMBL must know whether the firmware is
//! ACTUALLY enforcing UEFI Secure Boot. A measured boot whose chain was loaded
//! by a firmware that is NOT refusing unsigned images is unprotected: an
//! attacker can swap the UKI and the PCR-11 measurement attests their image
//! just as faithfully as ours. So before NMBL extends PCR-11 it reads the
//! authoritative SB-state and either WARNS LOUDLY (audit posture) or REFUSES
//! the measured boot (fail-closed posture — `secure_boot.enforce`).
//!
//! Two independent reads, both via std/rustix (NO new `unsafe`, no new dep):
//!
//! 1. **The `SecureBoot` UEFI variable** from efivarfs. Its GUID is the
//!    EFI global namespace `8be4df61-93ca-11d2-aa0d-00e098032b8c`; the file
//!    body is a 4-byte little-endian attribute header followed by the variable
//!    data — here a single byte that is `1` when Secure Boot is enabled and
//!    `0` when it is disabled. This is the authoritative "is the firmware
//!    enforcing" signal.
//! 2. **PCR-7** (the firmware/SB-state PCR) via the TPM transport. PCR-7 is
//!    what a sealing policy binds to so a sealed secret only unseals under the
//!    SAME SB-state; reading it here is INFORMATIONAL — we log its value so an
//!    operator can correlate the live PCR-7 with what they sealed against. The
//!    SB enforce/refuse decision is driven by the efivar, not PCR-7 (PCR-7
//!    alone cannot tell "enforcing" from "user-mode/setup-mode" without a full
//!    event-log replay we deliberately do not hand-roll — FIX-12).
//!
//! Degrade gracefully: efivarfs absent (a BIOS/CSM box, or efivarfs not
//! mounted) yields [`SbEfiState::Unreadable`] ⇒ a loud warning and PROCEED, NOT
//! a hard crash — we cannot prove SB is NOT enforcing, so we fail OPEN with a
//! warning exactly as the `requireTpm` posture degrades a no-TPM box. A
//! present-but-DISABLED SB under the enforce posture is the one fail-closed
//! case (we have positive proof the firmware is not enforcing).

use std::path::{Path, PathBuf};

use crate::error::{NmblError, Result};
use crate::{nmbl_info, nmbl_warn};

use super::presence::tpm_present;
use super::transport::TpmDevice;

/// efivarfs mount root. The kernel exposes each UEFI variable as a file named
/// `<Name>-<GUID>` under here; reading it returns the 4-byte attribute header
/// followed by the variable data.
const EFIVARS_DIR: &str = "/sys/firmware/efi/efivars";

/// The EFI global-variable namespace GUID (lowercase, hyphenated), the suffix
/// of the `SecureBoot`/`SetupMode` variable file names.
const EFI_GLOBAL_GUID: &str = "8be4df61-93ca-11d2-aa0d-00e098032b8c";

/// efivarfs prepends a 4-byte little-endian attributes word to every variable
/// body; the variable data (the SB-state byte) starts after it.
const EFIVAR_ATTR_LEN: usize = 4;

/// The firmware / Secure-Boot-state PCR. PCR-7 records the SB policy + the
/// keys/signatures used to validate the boot chain; a sealing policy binds it
/// so a secret only unseals under the SAME SB-state. Read here informationally.
pub const SB_STATE_PCR: u32 = 7;

/// The authoritative Secure-Boot state read from the `SecureBoot` efivar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SbEfiState {
    /// `SecureBoot == 1`: the firmware reports Secure Boot ENABLED (enforcing).
    Enabled,
    /// `SecureBoot == 0`: the firmware reports Secure Boot DISABLED — positive
    /// proof the firmware is not refusing unsigned images.
    Disabled,
    /// The variable could not be read (efivarfs absent / not mounted, the file
    /// missing, or a malformed body). We cannot prove SB is or is not
    /// enforcing, so this degrades to warn-and-proceed (never a hard crash).
    Unreadable,
}

/// The decision the SB-state gate reaches for the start of a measured boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SbAction {
    /// Secure Boot is enabled (enforcing) — proceed with the measured boot.
    Proceed,
    /// SB is not provably enforcing but the posture is not fail-closed (audit
    /// mode, or the state is unreadable): WARN LOUDLY and proceed.
    Warn,
    /// SB is DISABLED and the posture is fail-closed (`enforce`): REFUSE the
    /// unprotected measured boot (route to `refuse_unsigned`).
    Refuse,
}

/// Builds the efivarfs file path for the `SecureBoot` global variable under
/// `root` (parameterized for tests; production passes [`EFIVARS_DIR`]).
fn secure_boot_var_path(root: &Path) -> PathBuf {
    root.join(format!("SecureBoot-{EFI_GLOBAL_GUID}"))
}

/// Reads the `SecureBoot` efivar from the efivarfs rooted at `root` and
/// classifies it into [`SbEfiState`]. The file body is the 4-byte attribute
/// header followed by the state byte; any read failure / short body / out-of-
/// range value is [`SbEfiState::Unreadable`] rather than an error (degrade).
pub fn read_secure_boot_efivar_at(root: &Path) -> SbEfiState {
    let path = secure_boot_var_path(root);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        // ENOENT (BIOS/CSM box, efivarfs unmounted) and any other IO error
        // degrade to Unreadable — we do not have positive SB-state proof.
        Err(_) => return SbEfiState::Unreadable,
    };
    // The state byte sits immediately after the 4-byte attribute header.
    match bytes.get(EFIVAR_ATTR_LEN) {
        Some(0) => SbEfiState::Disabled,
        Some(1) => SbEfiState::Enabled,
        // A value other than 0/1, or a body too short to carry one, is not a
        // shape we can trust — treat as unreadable rather than guess.
        _ => SbEfiState::Unreadable,
    }
}

/// Reads the live `SecureBoot` efivar from the real efivarfs ([`EFIVARS_DIR`]).
#[must_use]
pub fn read_secure_boot_efivar() -> SbEfiState {
    read_secure_boot_efivar_at(Path::new(EFIVARS_DIR))
}

/// Reads PCR-7 (the SB-state PCR) via the TPM transport, returning the raw
/// digest bytes on success. Degrades to `None` when no TPM is present or the
/// read fails — PCR-7 here is informational (logged for correlation), so a
/// read failure must NOT abort the boot; the enforce decision is driven by the
/// efivar. A non-`None` result is the SHA-256-bank PCR-7 value.
#[must_use]
pub fn read_pcr7() -> Option<Vec<u8>> {
    if !tpm_present() {
        return None;
    }
    let dev = match TpmDevice::open() {
        Ok(dev) => dev,
        Err(e) => {
            nmbl_warn!("sb-state: PCR-{SB_STATE_PCR} read skipped (TPM open failed: {e})");
            return None;
        }
    };
    match super::commands::pcr_read(&dev, SB_STATE_PCR) {
        Ok(value) if !value.is_empty() => Some(value),
        Ok(_) => {
            nmbl_warn!("sb-state: PCR-{SB_STATE_PCR} not allocated in the SHA-256 bank");
            None
        }
        Err(e) => {
            nmbl_warn!("sb-state: PCR-{SB_STATE_PCR} read failed: {e}");
            None
        }
    }
}

/// Maps an efivar SB-state + the fail-closed posture onto the gate decision.
///
/// * [`SbEfiState::Enabled`] ⇒ [`SbAction::Proceed`] regardless of posture.
/// * [`SbEfiState::Disabled`] ⇒ [`SbAction::Refuse`] under `enforce` (positive
///   proof the firmware is not enforcing), else [`SbAction::Warn`] (audit).
/// * [`SbEfiState::Unreadable`] ⇒ [`SbAction::Warn`] ALWAYS: we lack positive
///   proof SB is NOT enforcing, so we degrade open with a loud warning rather
///   than refuse a BIOS/efivarfs-less box (mirrors the no-TPM degrade).
#[must_use]
pub fn decide_sb_action(state: SbEfiState, enforce: bool) -> SbAction {
    match state {
        SbEfiState::Enabled => SbAction::Proceed,
        SbEfiState::Disabled => {
            if enforce {
                SbAction::Refuse
            } else {
                SbAction::Warn
            }
        }
        SbEfiState::Unreadable => SbAction::Warn,
    }
}

/// The SB-state awareness gate (FIX-11), called at the START of the measured
/// path. Reads the authoritative `SecureBoot` efivar, logs PCR-7 for
/// correlation, and applies the `enforce` posture:
///
/// * SB enabled ⇒ `Ok(())`, proceed.
/// * SB disabled + audit (or state unreadable) ⇒ WARN LOUDLY, `Ok(())`.
/// * SB disabled + `enforce` ⇒ `Err(NmblError::Signature{stage:"secure-boot-state"})`,
///   which the caller wraps into `PolicyRefused` to route to the refuse
///   terminus rather than continue an unprotected measured boot.
///
/// `enforce` is the operator's fail-closed posture (`secure_boot.enforce`);
/// pass it pre-resolved so this stays config-shape-agnostic and unit-testable.
pub fn enforce_secure_boot_state(enforce: bool) -> Result<()> {
    let state = read_secure_boot_efivar();
    // Read PCR-7 for correlation only; never gates the decision (logged below).
    let pcr7 = read_pcr7();
    log_sb_state(state, pcr7.as_deref());

    match decide_sb_action(state, enforce) {
        SbAction::Proceed => {
            nmbl_info!("sb-state: Secure Boot is ENABLED (enforcing); measured boot proceeds");
            Ok(())
        }
        SbAction::Warn => {
            warn_not_enforcing(state, enforce);
            Ok(())
        }
        SbAction::Refuse => Err(NmblError::Signature {
            stage: "secure-boot-state",
            detail: "Secure Boot is DISABLED but secure_boot.enforce is set; refusing an \
                     unprotected measured boot (the firmware is not refusing unsigned images)"
                .to_string(),
        }),
    }
}

/// Logs the read SB-state + the PCR-7 value (hex, when present) at info level
/// so an operator can correlate the live firmware state with what they sealed
/// against. PCR-7 is informational here — the enforce decision is the efivar's.
fn log_sb_state(state: SbEfiState, pcr7: Option<&[u8]>) {
    let pcr7_desc = match pcr7 {
        Some(value) => crate::util::hex::hex_lower(value),
        None => "unavailable".to_string(),
    };
    nmbl_info!("sb-state: SecureBoot efivar = {state:?}; PCR-{SB_STATE_PCR} = {pcr7_desc}");
}

/// The LOUD warning for a measured boot that is NOT provably protected by
/// Secure Boot enforcement (FIX-11). Distinguishes the two warn cases so the
/// log says exactly why the boot is unprotected.
fn warn_not_enforcing(state: SbEfiState, enforce: bool) {
    match state {
        SbEfiState::Disabled => nmbl_warn!(
            "sb-state: ***** Secure Boot is DISABLED — this measured boot is UNPROTECTED; \
             an attacker can substitute the boot chain and the measurement still attests it. \
             Proceeding only because secure_boot.enforce is off (audit mode). *****"
        ),
        SbEfiState::Unreadable => nmbl_warn!(
            "sb-state: ***** could not read the SecureBoot efivar (efivarfs absent / not \
             mounted / BIOS-CSM box) — cannot confirm Secure Boot is enforcing; this measured \
             boot may be UNPROTECTED. Proceeding (degrade-open){}. *****",
            if enforce {
                "; enforce is set but SB-state is unprovable here"
            } else {
                ""
            }
        ),
        // Enabled never reaches the warn path.
        SbEfiState::Enabled => {}
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
#[path = "sbstate_tests.rs"]
mod tests;
