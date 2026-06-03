//! Deterministic TPM presence detection (FIX-28).
//!
//! Presence is a DETERMINISTIC sysfs fact — `/sys/class/tpm/tpm0` exists iff
//! the kernel bound a TPM driver — NOT a wall-clock probe (we do NOT "wait
//! 500 ms and see if a response shows up"). After Phase-2a early-module load
//! (`tpm_crb`/`tpm_tis`, added by `lib/modules/tpm.nix` per R-8) the sysfs
//! node is either there or it never will be, so a single `stat(2)` is the
//! authoritative answer.
//!
//! The guard (`policy::guard`, task #17) latches this value into a
//! `thread_local!{Cell<bool>}` right after Phase-2a so later cap decisions
//! read a frozen snapshot rather than re-probing; this module supplies the
//! one-shot deterministic check that latch is seeded from.

use std::path::Path;

/// The sysfs directory the kernel creates for the first TPM chip when a TPM
/// driver (`tpm_crb` / `tpm_tis`) binds. Its existence is the deterministic
/// presence oracle (FIX-28).
pub const TPM_SYSFS_CLASS: &str = "/sys/class/tpm/tpm0";

/// Returns `true` iff a TPM is present, determined deterministically from the
/// sysfs class node [`TPM_SYSFS_CLASS`]. No timing, no transact, no
/// side effects.
#[must_use]
pub fn tpm_present() -> bool {
    tpm_present_at(Path::new(TPM_SYSFS_CLASS))
}

/// Path-parameterized core of [`tpm_present`] (test seam). Returns `true`
/// iff `sysfs_node` exists. We use a metadata probe rather than
/// [`Path::exists`] so a dangling symlink is treated as "present" exactly as
/// the kernel's own class link would be — but any other `stat` failure
/// (ENOENT, EACCES on the parent, …) is "absent".
#[must_use]
pub fn tpm_present_at(sysfs_node: &Path) -> bool {
    // `symlink_metadata` does not traverse the final symlink, so the kernel's
    // `tpm0 -> ../../devices/.../tpm/tpm0` class link counts as present even
    // if its target is momentarily unreadable. Any error ⇒ absent.
    sysfs_node.symlink_metadata().is_ok()
}
