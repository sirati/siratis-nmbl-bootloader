//! The shared content-addressed generation id (`gen_id`) — FIX-07.
//!
//! ONE canonical derivation of the per-generation identifier, used by BOTH the
//! in-initramfs verify path (the sidecar-scan in [`crate::sig::scan`] and
//! `sig::verify`) AND — via the `--print-gen-id` CLI — the install-time signer
//! (`nmbl-sign`, #53). Keeping a single definition is load-bearing: the signer
//! writes `/boot/nmbl/sigs/<gen-id>/{kernel,initrd}.sig` and the verifier looks
//! the sidecars up under the SAME `<gen-id>`; any drift would silently break
//! verification on every generation.
//!
//! ## The scheme (frozen, FIX-07)
//!
//! `gen_id(toplevel) = file_name(canonicalize(toplevel))`
//!
//! i.e. the content-addressed Nix store basename of the generation's resolved
//! system toplevel — e.g. `abc123…-nixos-system-host-24.11`. Two properties
//! make this the right id:
//!
//! * **Content-addressed.** The store basename changes iff the generation's
//!   contents change, so a given kernel/initrd pair maps to exactly one id.
//! * **Rollback-stable.** It is derived from the toplevel the profile link
//!   *points at*, not the mutable `system-N-link` number, so rolling back to an
//!   older generation resolves the same id its sidecars were signed under.

use std::path::Path;

use crate::error::{NmblError, Result};

use super::Generation;

/// Derive the shared `gen_id` for a discovered [`Generation`] (FIX-07).
///
/// Canonicalizes the generation's resolved system toplevel and returns its
/// store basename. The toplevel was already resolved (mount-aware) during the
/// scan and is carried on [`Generation::toplevel`]; canonicalizing it here
/// collapses any remaining symlink indirection to the real store path before
/// taking the basename, so the id matches what `nix-env --list-generations`
/// (and thus the install signer) sees for the same generation.
///
/// Fails (`NmblError::Io` / `NmblError::Signature`) when the toplevel cannot be
/// canonicalized or has no usable basename — the caller treats either as a
/// hard "cannot locate this generation's sidecars" error, never an allow-all.
pub fn gen_id(generation: &Generation) -> Result<String> {
    gen_id_of_path(&generation.toplevel)
}

/// The basename-of-canonicalize core, factored so both [`gen_id`] and the
/// `--print-gen-id` CLI (which is handed a raw toplevel/profile path) share one
/// derivation. Any caller passing a profile-link path gets the same id as one
/// passing the already-resolved toplevel, because `canonicalize` follows the
/// link to the same store path either way.
pub fn gen_id_of_path(toplevel: &Path) -> Result<String> {
    let canonical = std::fs::canonicalize(toplevel).map_err(|source| NmblError::Io {
        source,
        context: format!("canonicalize generation toplevel {}", toplevel.display()),
    })?;
    canonical
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_owned)
        .ok_or_else(|| NmblError::Signature {
            stage: "gen-id",
            detail: format!(
                "generation toplevel {} has no store basename",
                canonical.display()
            ),
        })
}
