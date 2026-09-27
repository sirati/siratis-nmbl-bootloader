//! On-disk flag files `nmblctl` writes for NMBL to honour at the next boot.
//!
//! Three operator intents map to three durable files under NMBL's state dir
//! (or `/boot/nmbl`): the rescue sentinel, a one-shot "boot this next"
//! selection, and the persistent default. This module owns the FORMATS
//! (parse + render) so they have one definition tested here and consumed by
//! both `nmblctl` and (for the one-shot / default) the boot-time selector.
//!
//! # Signature safety (the load-bearing design point)
//!
//! On **profile hosts** (stateful BIOS/GRUB with a normal Nix store) the boot
//! selection is the Nix `system` profile symlink, which is NOT signed — so a
//! remembered default or a one-shot override is a plain hint the selector
//! reads, and these files live next to `state.bin`.
//!
//! On **signed-EROFS hosts** the boot selection IS the `active` symlink, and
//! its target's image + config are ML-DSA-verified before mount. A default or
//! one-shot MUST NOT repoint `active` from userspace, because that is exactly
//! the signed selector; doing so from a plain flag file would let a compromised
//! root pick any on-disk generation while the boot path still trusts `active`.
//! Instead, on EROFS these files carry a generation number that `nmblctl`
//! resolves to a signed generation directory and applies THROUGH the verified
//! `nmbl-erofsctl activate`/`rollback` path (which re-verifies the target's
//! signature before the atomic rename). The flag files never become an
//! alternative, unverified selector — they only NAME an intent that the
//! verified tooling then carries out. The boot-time verifier's trust anchor
//! (the baked public key) is untouched.
//!
//! Every value here is a small, line-oriented text format so it is trivially
//! durable (write temp, fsync, rename, fsync dir — done by `main.rs`) and
//! human-inspectable.

use std::fmt;

/// The rescue request: presence of the sentinel file forces a rescue boot.
/// This type exists so `nmblctl reboot-rescue` and the tests speak the same
/// vocabulary as NMBL's `policy::sentinel`; the file itself is empty (presence
/// is the whole signal), matching `security_consts::SENTINEL_PATH`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RescueRequest;

impl RescueRequest {
    /// The empty sentinel body NMBL checks for by presence.
    #[must_use]
    pub fn body() -> &'static [u8] {
        b""
    }
}

/// A one-shot "boot this generation next" selection (`nmblctl reboot-into`).
/// NMBL honours it exactly once and then removes the file, like `grub-reboot`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OneShotSelection {
    /// The generation number to boot next.
    pub generation: u32,
}

/// The persistent default (`nmblctl default`). Either a fixed generation, or
/// the artificial "latest" that always resolves to the newest generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultSelection {
    /// Always boot the newest generation available at boot time.
    Latest,
    /// Always boot this specific generation number.
    Generation(u32),
}

/// Canonical file basenames under the state dir. Kept here so `nmblctl` and any
/// boot-time reader agree on one set of names.
pub const ONE_SHOT_BASENAME: &str = "boot-once";
pub const DEFAULT_BASENAME: &str = "boot-default";

impl OneShotSelection {
    /// Render the one-shot file body: a single line `generation <N>`.
    #[must_use]
    pub fn render(self) -> String {
        format!("generation {}\n", self.generation)
    }

    /// Parse a one-shot file body. Tolerates trailing whitespace / blank lines;
    /// returns `None` for anything that is not exactly one `generation <N>`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let line = text.lines().find(|l| !l.trim().is_empty())?;
        let n = line.trim().strip_prefix("generation ")?.trim();
        Some(Self {
            generation: n.parse().ok()?,
        })
    }
}

impl DefaultSelection {
    /// Render the default file body: `latest` or `generation <N>`.
    #[must_use]
    pub fn render(self) -> String {
        match self {
            DefaultSelection::Latest => "latest\n".to_string(),
            DefaultSelection::Generation(n) => format!("generation {n}\n"),
        }
    }

    /// Parse a default file body. `None` for an unrecognised body.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let line = text.lines().find(|l| !l.trim().is_empty())?.trim();
        if line == "latest" {
            return Some(DefaultSelection::Latest);
        }
        let n = line.strip_prefix("generation ")?.trim();
        Some(DefaultSelection::Generation(n.parse().ok()?))
    }
}

impl fmt::Display for DefaultSelection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DefaultSelection::Latest => write!(f, "latest (newest generation)"),
            DefaultSelection::Generation(n) => write!(f, "generation {n}"),
        }
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::expect_used, reason = "tests assert")]
mod tests {
    use super::*;

    #[test]
    fn one_shot_round_trips() {
        let sel = OneShotSelection { generation: 42 };
        let body = sel.render();
        assert_eq!(OneShotSelection::parse(&body), Some(sel));
    }

    #[test]
    fn one_shot_tolerates_whitespace() {
        assert_eq!(
            OneShotSelection::parse("\n  generation 7  \n"),
            Some(OneShotSelection { generation: 7 })
        );
    }

    #[test]
    fn one_shot_rejects_garbage() {
        assert_eq!(OneShotSelection::parse(""), None);
        assert_eq!(OneShotSelection::parse("boot 7"), None);
        assert_eq!(OneShotSelection::parse("generation abc"), None);
    }

    #[test]
    fn default_round_trips_latest_and_generation() {
        for sel in [DefaultSelection::Latest, DefaultSelection::Generation(3)] {
            let body = sel.render();
            assert_eq!(DefaultSelection::parse(&body), Some(sel));
        }
    }

    #[test]
    fn default_rejects_garbage() {
        assert_eq!(DefaultSelection::parse("newest"), None);
        assert_eq!(DefaultSelection::parse("generation "), None);
        assert_eq!(DefaultSelection::parse(""), None);
    }

    #[test]
    fn default_display_is_human_readable() {
        assert_eq!(
            DefaultSelection::Latest.to_string(),
            "latest (newest generation)"
        );
        assert_eq!(DefaultSelection::Generation(9).to_string(), "generation 9");
    }

    #[test]
    fn rescue_sentinel_body_is_empty() {
        assert!(RescueRequest::body().is_empty());
    }
}
