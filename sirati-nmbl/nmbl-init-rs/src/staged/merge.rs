//! The TRANSACTIONAL staged-fragment merge (#33 — FIX-32).
//!
//! A [`crate::config::ConfigFragment`] is a partial overlay: each top-level
//! table is an `Option<T>` where `Some(_)` means "replace the base table with
//! this one" and `None` means "leave the base untouched" (the same shape
//! `config::fragment` documents, FIX-56). The fragment OMITS every
//! security-policy table (`[signing]`, `[secure_boot]`, `[staged]`) by
//! construction, so a fragment can never relax the enforcement posture or
//! re-point the staged source it was loaded through (FIX-53) — there is no
//! field here through which it could.
//!
//! [`merge_fragment`] applies the overlay TRANSACTIONALLY (FIX-32): it swaps
//! each carried table into the base `Config`, runs the SAME full
//! [`Config::validate`] the loader runs, and only KEEPS the swap if validation
//! passes. On ANY validation failure it swaps every table back, so the base
//! `Config` is left BYTE-FOR-BYTE as it was before the call — there is no
//! partial apply. The caller routes that failure to the refuse terminus
//! against the still-pristine base config.
//!
//! The swap-then-rollback shape (rather than clone-validate-replace) is
//! deliberate: the `Config` sub-tables are not `Clone`, and an in-place swap
//! that restores the originals on failure gives the exact same
//! transactional guarantee without requiring a deep copy of the base.

use crate::config::{Config, ConfigFragment};
use crate::error::Result;

/// Transactionally merge `fragment` into `config` (FIX-32).
///
/// Swaps each table the fragment carries (`Some(_)`) into `config`, keeping the
/// displaced base table, then runs [`Config::validate`] over the candidate. On
/// success the swap stays and the displaced tables are dropped. On a validation
/// failure EVERY swapped table is restored, so `config` is left exactly as it
/// entered — the transactional no-partial-apply guarantee. A table the fragment
/// did not mention (`None`) is never touched.
///
/// # Errors
/// Returns the [`Config::validate`] error when the MERGED candidate is invalid;
/// `config` is unchanged in that case (the caller refuses against the pristine
/// base).
pub(super) fn merge_fragment(config: &mut Config, fragment: ConfigFragment) -> Result<()> {
    // Decompose the fragment so each `Option<T>` is moved out by value; a
    // missing table contributes nothing to the rollback record.
    let ConfigFragment {
        general,
        kernel_modules,
        filesystems,
        activations,
        tui,
        paths,
        #[cfg(feature = "image-splash")]
        splash,
        rescue,
        emergency_shell,
        driver_images,
        tpm,
    } = fragment;

    // The rollback record: for each table the fragment carried, hold the base
    // value we displaced so a failed validate can put it back. `None` entries
    // are tables the fragment did not mention — nothing to undo.
    let mut undo = Undo::default();

    if let Some(v) = general {
        undo.general = Some(std::mem::replace(&mut config.general, v));
    }
    if let Some(v) = kernel_modules {
        undo.kernel_modules = Some(std::mem::replace(&mut config.kernel_modules, v));
    }
    if let Some(v) = filesystems {
        undo.filesystems = Some(std::mem::replace(&mut config.filesystems, v));
    }
    if let Some(v) = activations {
        undo.activations = Some(std::mem::replace(&mut config.activations, v));
    }
    if let Some(v) = tui {
        undo.tui = Some(std::mem::replace(&mut config.tui, v));
    }
    if let Some(v) = paths {
        undo.paths = Some(std::mem::replace(&mut config.paths, v));
    }
    #[cfg(feature = "image-splash")]
    if let Some(v) = splash {
        undo.splash = Some(std::mem::replace(&mut config.splash, v));
    }
    if let Some(v) = rescue {
        undo.rescue = Some(std::mem::replace(&mut config.rescue, v));
    }
    if let Some(v) = emergency_shell {
        undo.emergency_shell = Some(std::mem::replace(&mut config.emergency_shell, v));
    }
    if let Some(v) = driver_images {
        undo.driver_images = Some(std::mem::replace(&mut config.driver_images, v));
    }
    if let Some(v) = tpm {
        undo.tpm = Some(std::mem::replace(&mut config.tpm, v));
    }

    // Validate the CANDIDATE in full (same pass the loader runs). On failure
    // restore every displaced table so the base config is pristine (FIX-32).
    match config.validate() {
        Ok(()) => Ok(()),
        Err(e) => {
            undo.restore(config);
            Err(e)
        }
    }
}

/// The displaced base tables, kept so a failed validate can roll the merge back
/// to a byte-for-byte-identical base (FIX-32). Each field is `Some` only for a
/// table the fragment actually replaced.
#[derive(Default)]
struct Undo {
    general: Option<crate::config::General>,
    kernel_modules: Option<crate::config::KernelModules>,
    filesystems: Option<Vec<crate::config::FilesystemEntry>>,
    activations: Option<Vec<crate::config::Activation>>,
    tui: Option<crate::config::Tui>,
    paths: Option<crate::config::Paths>,
    #[cfg(feature = "image-splash")]
    splash: Option<crate::config::Splash>,
    rescue: Option<crate::config::RescueConfig>,
    emergency_shell: Option<crate::config::EmergencyShellConfig>,
    driver_images: Option<crate::config::DriverImagesConfig>,
    tpm: Option<crate::config::TpmConfig>,
}

impl Undo {
    /// Put every displaced table back, restoring the pristine base config.
    fn restore(self, config: &mut Config) {
        if let Some(v) = self.general {
            config.general = v;
        }
        if let Some(v) = self.kernel_modules {
            config.kernel_modules = v;
        }
        if let Some(v) = self.filesystems {
            config.filesystems = v;
        }
        if let Some(v) = self.activations {
            config.activations = v;
        }
        if let Some(v) = self.tui {
            config.tui = v;
        }
        if let Some(v) = self.paths {
            config.paths = v;
        }
        #[cfg(feature = "image-splash")]
        if let Some(v) = self.splash {
            config.splash = v;
        }
        if let Some(v) = self.rescue {
            config.rescue = v;
        }
        if let Some(v) = self.emergency_shell {
            config.emergency_shell = v;
        }
        if let Some(v) = self.driver_images {
            config.driver_images = v;
        }
        if let Some(v) = self.tpm {
            config.tpm = v;
        }
    }
}
