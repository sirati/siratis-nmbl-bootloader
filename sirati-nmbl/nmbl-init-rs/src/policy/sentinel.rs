//! The rescue sentinel: an on-`/boot` marker that forces a rescue boot and
//! keeps the TPM locked (R-1 / FIX-21 / FIX-49). ALWAYS-COMPILED — the read
//! side gates the force-rescue decision on every build, and the relock
//! terminus (`secure-boot`-only) writes it.
//!
//! ## Force-rescue (FIX-49, ADDITIVE)
//!
//! [`should_force_rescue`] is the ADDITIVE union of the existing
//! force-on-boot trigger and sentinel presence:
//!
//! ```text
//! should_force_rescue(force_external, cfg)
//!   = force_external                 // the peer-hot rescue.force_on_boot gate
//!     || sentinel_present(cfg)       // an empty /boot/nmbl/rescue marker
//! ```
//!
//! The caller passes its already-computed `force_external` flag (the binary
//! keeps owning `should_force_external_rescue`, untouched — FIX-49), so this
//! module only adds the sentinel arm. When the sentinel forces the rescue,
//! the boot takes the SAME `rescue::dispatch` path as the force-on-boot
//! trigger, whose G4 seal is the authoritative sentinel-seal (cap PCR +
//! close mappers) — so a sentinel-forced rescue keeps the TPM locked too.
//!
//! ## Write target + ordering (FIX-21)
//!
//! [`write_sentinel`] writes an EMPTY file at the configured sentinel path,
//! resolved against the WRITABLE `runtime_boot_mountpoint` when set (a
//! bootstrap RW boot FS) and otherwise taken as the absolute configured
//! path. It is called from the refuse terminus BEFORE the relock closes the
//! sentinel's backing device, and is best-effort: a write failure is logged
//! and never blocks the cap/reboot (the cap is the real boundary; the next
//! boot re-refuses safely on the still-bad image).

use std::path::{Path, PathBuf};

use crate::config::Config;

/// Whether a rescue boot must be forced this boot (FIX-49). `force_external`
/// is the caller's existing force-on-boot decision; this ORs in the
/// sentinel. ALWAYS-COMPILED so the plain-`luks-tpm` boot path consults the
/// sentinel even with `secure-boot` off.
#[must_use]
pub fn should_force_rescue(force_external: bool, config: &Config) -> bool {
    force_external || sentinel_present(config)
}

/// Whether the rescue sentinel file exists. An EMPTY `/boot/nmbl/rescue`
/// (the single-sourced default — `SENTINEL_PATH`) ⇒ force rescue: refuse a
/// measured boot, keep the TPM locked, go straight to rescue.
#[must_use]
pub fn sentinel_present(config: &Config) -> bool {
    resolve_sentinel_path(config).is_some_and(|p| p.exists())
}

/// Write the empty sentinel marker (best-effort). Called from the refuse
/// terminus BEFORE the relock tears down the backing device (FIX-21). Logs
/// and swallows every failure — the imminent reboot is the real boundary.
pub fn write_sentinel(config: &Config) {
    let Some(path) = resolve_sentinel_path(config) else {
        crate::nmbl_warn!(
            "refuse: no writable boot mountpoint and no absolute sentinel path; \
             cannot drop the rescue sentinel — the next boot will re-evaluate the image"
        );
        return;
    };
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        crate::nmbl_warn!(
            "refuse: could not create sentinel dir {}: {e}; continuing",
            parent.display()
        );
    }
    // An EMPTY file: presence is the whole signal (FIX-49). `write` with an
    // empty slice truncates-or-creates atomically enough for a marker.
    match std::fs::write(&path, b"") {
        Ok(()) => crate::nmbl_info!("refuse: wrote rescue sentinel {}", path.display()),
        Err(e) => crate::nmbl_warn!(
            "refuse: could not write rescue sentinel {}: {e}; \
             the next boot may not force rescue",
            path.display()
        ),
    }
}

/// Resolve the sentinel path for read/write. Prefers the WRITABLE
/// bootstrap `runtime_boot_mountpoint` joined with the sentinel's
/// boot-relative tail; falls back to the configured path used as-is when it
/// is absolute and no runtime mountpoint is known (embedded mode). Returns
/// `None` only when there is neither a runtime mountpoint nor an absolute
/// configured path to anchor against.
fn resolve_sentinel_path(config: &Config) -> Option<PathBuf> {
    let configured = configured_sentinel_path(config);
    match config.runtime_boot_mountpoint.as_deref() {
        Some(mp) => Some(join_boot_relative(mp, configured)),
        None if configured.is_absolute() => Some(configured.to_path_buf()),
        None => None,
    }
}

/// The configured sentinel path. `secure-boot` builds read it from
/// `[secure_boot].sentinel_path`; feature-free builds use the
/// single-sourced default directly so the read side still works.
fn configured_sentinel_path(config: &Config) -> &Path {
    #[cfg(feature = "secure-boot")]
    {
        config.secure_boot.sentinel_path.as_path()
    }
    #[cfg(not(feature = "secure-boot"))]
    {
        let _ = config;
        Path::new(crate::security_consts::SENTINEL_PATH)
    }
}

/// Join the sentinel's boot-relative tail onto the runtime boot mountpoint.
/// The configured default (`/boot/nmbl/rescue`) is written assuming `/boot`
/// is the boot FS root, so when the boot FS is mounted elsewhere at runtime
/// we strip a leading `/boot/` and re-anchor; a path that is not under
/// `/boot` is appended by its components after the leading `/`.
fn join_boot_relative(mountpoint: &Path, configured: &Path) -> PathBuf {
    let tail = configured
        .strip_prefix("/boot")
        .or_else(|_| configured.strip_prefix("/"))
        .unwrap_or(configured);
    mountpoint.join(tail)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert on contract failures"
)]
#[path = "sentinel_tests.rs"]
mod tests;
