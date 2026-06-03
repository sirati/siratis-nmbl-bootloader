//! Per-role domain selection — a thin map over the verifier's frozen consts.
//!
//! The six role domains are the EXACT `&[u8]` byte strings pinned in
//! `nmbl_init::sig` (`verify.rs`): a signature minted under one role can never
//! verify under another (the domain-cross-reject property — FIX-01). This
//! module does NOT redefine them; it re-exports them by a stable `--domain`
//! CLI token so the operator selects a role and the signer threads the SAME
//! byte string the verifier recomputes its tag from.

use nmbl_init::sig::{
    DOMAIN_DRIVER_IMAGE, DOMAIN_GEN_INITRD, DOMAIN_GEN_KERNEL, DOMAIN_PRIORITY_FILE,
    DOMAIN_RESCUE_SFS, DOMAIN_STAGED_FRAGMENT,
};

/// One selectable signing role, paired with its CLI token and the frozen
/// verifier domain const it threads.
struct Role {
    /// The `--domain <token>` string the operator passes.
    token: &'static str,
    /// The frozen domain byte string from `nmbl_init::sig` (NOT redefined).
    domain: &'static [u8],
}

/// The full role table. The `domain` fields are the verifier's own consts, so
/// adding a role here can never drift from what the verify path accepts.
const ROLES: &[Role] = &[
    Role {
        token: "gen-kernel",
        domain: DOMAIN_GEN_KERNEL,
    },
    Role {
        token: "gen-initrd",
        domain: DOMAIN_GEN_INITRD,
    },
    Role {
        token: "driver-image",
        domain: DOMAIN_DRIVER_IMAGE,
    },
    Role {
        token: "staged-fragment",
        domain: DOMAIN_STAGED_FRAGMENT,
    },
    Role {
        token: "priority-file",
        domain: DOMAIN_PRIORITY_FILE,
    },
    Role {
        token: "rescue-sfs",
        domain: DOMAIN_RESCUE_SFS,
    },
];

/// Resolve a `--domain` CLI token to the frozen verifier domain byte string.
/// Returns `None` for an unknown token; the caller turns that into a usage
/// error listing the accepted roles via [`role_tokens`].
#[must_use]
pub fn domain_for(token: &str) -> Option<&'static [u8]> {
    ROLES.iter().find(|r| r.token == token).map(|r| r.domain)
}

/// The comma-separated list of accepted `--domain` tokens, for help/errors.
#[must_use]
pub fn role_tokens() -> String {
    ROLES.iter().map(|r| r.token).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_token_maps_to_its_frozen_const() {
        assert_eq!(domain_for("gen-kernel"), Some(DOMAIN_GEN_KERNEL));
        assert_eq!(domain_for("gen-initrd"), Some(DOMAIN_GEN_INITRD));
        assert_eq!(domain_for("driver-image"), Some(DOMAIN_DRIVER_IMAGE));
        assert_eq!(domain_for("staged-fragment"), Some(DOMAIN_STAGED_FRAGMENT));
        assert_eq!(domain_for("priority-file"), Some(DOMAIN_PRIORITY_FILE));
        assert_eq!(domain_for("rescue-sfs"), Some(DOMAIN_RESCUE_SFS));
    }

    #[test]
    fn unknown_token_is_none() {
        assert_eq!(domain_for("nope"), None);
        assert_eq!(domain_for(""), None);
    }

    #[test]
    fn all_six_roles_present() {
        assert_eq!(ROLES.len(), 6);
        // Every role token must be distinct.
        for (i, a) in ROLES.iter().enumerate() {
            for b in ROLES.iter().skip(i + 1) {
                assert_ne!(a.token, b.token);
            }
        }
    }
}
