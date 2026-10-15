//! Host-side inspection of the installed external full-system rescue.
//!
//! `nmblctl verify-rescue` uses this after an update to check, from the booted
//! system, what NMBL would check before entering the rescue: each image the
//! runtime config names (the stage-2 rescue image, the networking stage, the
//! tools image) is a regular file under the boot root, its SHA-512 matches the
//! config's pin, and, with `signing.enable`, its sibling sidecar verifies under
//! the image's own domain with this build's baked keys. Paths, sidecars, pins
//! and domains come from the same helpers the boot path uses ([`super::locate`],
//! [`super::image`], [`crate::sig`]), so the two cannot drift.
//!
//! Unlike the boot path this never stops at the first failure and never maps a
//! failure through the audit posture: every image is checked and every problem
//! reported.

use std::fs::File;
use std::io::ErrorKind;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::sig::keys::BakedKey;
use crate::sig::verify::{VerifyPolicy, verify_digest};
use crate::sig::{DOMAIN_NETWORK_STAGE, DOMAIN_RESCUE_SFS, DOMAIN_RESCUE_TOOLS, SigSidecar};

use super::{RescueImageFormat, RescueMode};

/// Which configured image a check is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageRole {
    /// The stage-2 rescue image at `[rescue].sfs_path`.
    RescueImage,
    /// `[rescue.network_stage]`.
    NetworkStage,
    /// `[rescue.tools]`.
    Tools,
}

impl ImageRole {
    /// Stable machine-readable name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RescueImage => "rescue-image",
            Self::NetworkStage => "network-stage",
            Self::Tools => "tools",
        }
    }

    /// The signature domain NMBL verifies this image under at boot.
    fn domain(self) -> &'static [u8] {
        match self {
            Self::RescueImage => DOMAIN_RESCUE_SFS,
            Self::NetworkStage => DOMAIN_NETWORK_STAGE,
            Self::Tools => DOMAIN_RESCUE_TOOLS,
        }
    }
}

/// Outcome of the SHA-512 pin check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinStatus {
    /// The image's digest equals the pin.
    Matches,
    /// The digest differs, or the pin is malformed.
    Mismatch,
    /// The config pins no digest.
    Unpinned,
    /// A pin exists but the image could not be read.
    Unreadable,
}

impl PinStatus {
    /// Stable machine-readable name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Matches => "matches",
            Self::Mismatch => "mismatch",
            Self::Unpinned => "unpinned",
            Self::Unreadable => "unreadable",
        }
    }
}

/// Outcome of the detached-signature check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureStatus {
    /// The sidecar verifies under the image's domain with a baked key.
    Verified,
    /// No sidecar next to the image.
    Missing,
    /// The sidecar exists but does not verify (or cannot be checked).
    Invalid,
    /// `signing.enable` is off; NMBL checks no signature.
    NotRequired,
}

impl SignatureStatus {
    /// Stable machine-readable name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Missing => "missing",
            Self::Invalid => "invalid",
            Self::NotRequired => "not-required",
        }
    }
}

/// The result for one configured image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageCheck {
    pub role: ImageRole,
    /// Where the image was looked for (the configured value when unsafe).
    pub path: PathBuf,
    /// The path opens as a regular file.
    pub present: bool,
    pub sha512: PinStatus,
    pub signature: SignatureStatus,
}

/// The result for the whole rescue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RescueInspection {
    pub images: Vec<ImageCheck>,
    /// Every problem found, in image order. Empty means the rescue is usable.
    pub problems: Vec<String>,
}

impl RescueInspection {
    /// No problem was found.
    #[must_use]
    pub fn ok(&self) -> bool {
        self.problems.is_empty()
    }
}

/// Whether `config` describes an external full-system rescue: an external
/// rescue with `[rescue.system]` host data or the EROFS stage-2 image.
#[must_use]
pub fn is_external_full_system(config: &Config) -> bool {
    config.rescue.mode == RescueMode::External
        && (config.rescue.system.is_some()
            || config.rescue.image.format == RescueImageFormat::Erofs)
}

/// Inspect the external full-system rescue images `config` names, resolved
/// against `boot_root` (the mounted boot partition), with this build's baked
/// signing keys.
#[must_use]
pub fn inspect_external_rescue(config: &Config, boot_root: &Path) -> RescueInspection {
    let keys = crate::sig::keys::parse_baked_keys().map_err(|e| e.to_string());
    inspect_with_keys(config, boot_root, keys.as_deref())
}

/// [`inspect_external_rescue`] with an explicit trust anchor.
pub(crate) fn inspect_with_keys(
    config: &Config,
    boot_root: &Path,
    keys: Result<&[BakedKey], &String>,
) -> RescueInspection {
    let mut problems = Vec::new();
    if !is_external_full_system(config) {
        problems.push(
            "the config has no external full-system rescue (rescue.mode = \"external\" with \
             [rescue.system] or an EROFS [rescue.image])"
                .to_string(),
        );
        return RescueInspection {
            images: Vec::new(),
            problems,
        };
    }
    let checker = Checker {
        config,
        boot_root,
        keys,
    };
    let mut images = vec![checker.check(
        ImageRole::RescueImage,
        Ok(super::locate::sfs_relative(config)),
        config.rescue.image.sha512.as_deref(),
        &mut problems,
    )];
    if let Some(stage) = &config.rescue.network_stage {
        images.push(checker.check(
            ImageRole::NetworkStage,
            boot_relative(&stage.path),
            stage.sha512.as_deref(),
            &mut problems,
        ));
    }
    if let Some(tools) = &config.rescue.tools {
        images.push(checker.check(
            ImageRole::Tools,
            boot_relative(&tools.path),
            Some(&tools.sha512),
            &mut problems,
        ));
    }
    RescueInspection { images, problems }
}

/// The boot-relative form NMBL accepts for the network stage and tools paths,
/// or the configured value when it could leave the boot filesystem.
fn boot_relative(configured: &Path) -> Result<PathBuf, PathBuf> {
    super::image::boot_relative(&configured.to_string_lossy()).ok_or_else(|| configured.into())
}

struct Checker<'a> {
    config: &'a Config,
    boot_root: &'a Path,
    keys: Result<&'a [BakedKey], &'a String>,
}

impl Checker<'_> {
    fn check(
        &self,
        role: ImageRole,
        relative: Result<PathBuf, PathBuf>,
        pin: Option<&str>,
        problems: &mut Vec<String>,
    ) -> ImageCheck {
        let name = role.as_str();
        let path = match relative {
            Ok(relative) => self.boot_root.join(relative),
            Err(configured) => {
                problems.push(format!(
                    "{name}: unsafe path `{}` outside the boot partition",
                    configured.display()
                ));
                return ImageCheck {
                    role,
                    path: configured,
                    present: false,
                    sha512: if pin_value(pin).is_some() {
                        PinStatus::Unreadable
                    } else {
                        PinStatus::Unpinned
                    },
                    signature: self.unchecked_signature(),
                };
            }
        };
        // One descriptor for the type check, the pin and the signature, as at
        // boot. The pre-open type check keeps a FIFO from blocking the open.
        let file = std::fs::metadata(&path)
            .is_ok_and(|m| m.is_file())
            .then(|| File::open(&path).ok())
            .flatten()
            .filter(|f| f.metadata().is_ok_and(|m| m.is_file()));
        let present = file.is_some();
        if !present {
            problems.push(format!(
                "{name}: {} is missing or not a regular file",
                path.display()
            ));
        }
        let digest = file
            .as_ref()
            .and_then(|f| match crate::util::hash::sha512_fd(f.as_fd()) {
                Ok((digest, _len)) => Some(digest),
                Err(error) => {
                    problems.push(format!("{name}: reading {}: {error}", path.display()));
                    None
                }
            });
        let sha512 = pin_status(role, pin, digest, problems);
        let signature = self.signature_status(role, &path, digest, problems);
        ImageCheck {
            role,
            path,
            present,
            sha512,
            signature,
        }
    }

    fn unchecked_signature(&self) -> SignatureStatus {
        if self.config.signing.enable {
            SignatureStatus::Missing
        } else {
            SignatureStatus::NotRequired
        }
    }

    fn signature_status(
        &self,
        role: ImageRole,
        image: &Path,
        digest: Option<[u8; 64]>,
        problems: &mut Vec<String>,
    ) -> SignatureStatus {
        if !self.config.signing.enable {
            return SignatureStatus::NotRequired;
        }
        let name = role.as_str();
        let sidecar = super::image::sidecar_path(image, &self.config.signing.sig_path_suffix);
        let bytes = match std::fs::read(&sidecar) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                problems.push(format!(
                    "{name}: signature {} is missing",
                    sidecar.display()
                ));
                return SignatureStatus::Missing;
            }
            Err(error) => {
                problems.push(format!(
                    "{name}: reading signature {}: {error}",
                    sidecar.display()
                ));
                return SignatureStatus::Invalid;
            }
        };
        let verdict = digest
            .ok_or_else(|| "the image is unreadable".to_string())
            .and_then(|digest| {
                let keys = self
                    .keys
                    .map_err(|error| format!("the baked signing keys are unusable: {error}"))?;
                let parsed = SigSidecar::parse(&bytes).map_err(|error| error.to_string())?;
                verify_digest(&digest, role.domain(), &parsed, keys, VerifyPolicy::Enforce)
                    .map_err(|error| error.to_string())
            });
        match verdict {
            Ok(()) => SignatureStatus::Verified,
            Err(reason) => {
                problems.push(format!(
                    "{name}: signature {} does not verify: {reason}",
                    sidecar.display()
                ));
                SignatureStatus::Invalid
            }
        }
    }
}

fn pin_value(pin: Option<&str>) -> Option<&str> {
    pin.map(str::trim).filter(|p| !p.is_empty())
}

fn pin_status(
    role: ImageRole,
    pin: Option<&str>,
    digest: Option<[u8; 64]>,
    problems: &mut Vec<String>,
) -> PinStatus {
    let name = role.as_str();
    let Some(pin) = pin_value(pin) else {
        if role == ImageRole::Tools {
            problems.push(format!(
                "{name}: no SHA-512 pin; NMBL never mounts an unpinned tools image"
            ));
        }
        return PinStatus::Unpinned;
    };
    let Some(actual) = digest else {
        return PinStatus::Unreadable;
    };
    match super::image::parse_pin(pin, name) {
        Ok(expected) if expected == actual => PinStatus::Matches,
        Ok(_) => {
            problems.push(format!(
                "{name}: SHA-512 {} does not match the pin in the config",
                crate::util::hex::hex_lower(&actual)
            ));
            PinStatus::Mismatch
        }
        Err(_) => {
            problems.push(format!("{name}: the pinned SHA-512 is not 128 hex digits"));
            PinStatus::Mismatch
        }
    }
}

#[cfg(test)]
#[path = "inspect_tests.rs"]
mod tests;
