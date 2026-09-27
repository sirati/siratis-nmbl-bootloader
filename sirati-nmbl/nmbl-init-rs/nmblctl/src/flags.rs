//! Flag-file formats `nmblctl` writes for NMBL to honour at the next boot.
//!
//! The one-shot (`boot-once`) and persistent-default (`boot-default`) formats,
//! and the boot-time read/consume logic, live in [`nmbl_init::boot_selection`]
//! — the SINGLE source of truth shared with the boot-time selector, so writer
//! and reader cannot drift. `nmblctl` re-exports them here and adds only the
//! rescue-sentinel helper (whose format is the existing
//! `nmbl_init::policy::sentinel` empty-file marker).
//!
//! # Signature safety (the load-bearing design point)
//!
//! On **profile hosts** (stateful BIOS/GRUB with a normal Nix store) the boot
//! selection is the Nix `system` profile symlink, which is NOT signed — so a
//! remembered default or a one-shot is a plain hint the selector reads, and
//! these files live next to `state.bin`.
//!
//! On **signed-EROFS hosts** the boot selection IS the `active` symlink, whose
//! target's image + config are ML-DSA-verified before mount. `nmblctl` REFUSES
//! to write these files there (see `crate::state::System::validate_generation`)
//! and points at the verified `nmbl-erofsctl activate`/`rollback` path, which
//! re-verifies the target's signature before the atomic rename. The flag files
//! never become an alternative, unverified selector, and the boot-time verifier
//! consumes them only on profile hosts (`resolve_default_index` is a no-op on
//! EROFS). The baked public-key trust anchor is untouched.

pub use nmbl_init::boot_selection::{
    DEFAULT_BASENAME, DefaultSelection, ONE_SHOT_BASENAME, OneShotSelection,
};

/// The rescue request: presence of the sentinel file forces a rescue boot.
/// The file is empty (presence is the whole signal), matching
/// `nmbl_init::security_consts::SENTINEL_PATH` / `nmbl_init::policy::sentinel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RescueRequest;

impl RescueRequest {
    /// The empty sentinel body NMBL checks for by presence.
    #[must_use]
    pub fn body() -> &'static [u8] {
        b""
    }
}

#[cfg(test)]
#[allow(clippy::panic, reason = "tests assert")]
mod tests {
    use super::*;

    #[test]
    fn rescue_sentinel_body_is_empty() {
        assert!(RescueRequest::body().is_empty());
    }

    #[test]
    fn reexported_formats_round_trip() {
        // Sanity that the shared types are usable through the re-export.
        let one = OneShotSelection { generation: 4 };
        assert_eq!(OneShotSelection::parse(&one.render()), Some(one));
        let def = DefaultSelection::Latest;
        assert_eq!(DefaultSelection::parse(&def.render()), Some(def));
    }
}
