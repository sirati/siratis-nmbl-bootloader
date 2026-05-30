//! Scan `/nix/var/nix/profiles` for NixOS system generations.
//!
//! Replaces `scripts/find-generations.sh.nix`. Each `system-<N>-link` symlink
//! describes one bootable generation; we resolve its kernel/initrd targets,
//! read its kernel-params file, and surface the result as [`Generation`].

use std::path::PathBuf;

mod readiness;
mod resolve;
mod scan;

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests are allowed to assert with panics"
)]
mod tests;

/// Single NixOS system generation discovered under
/// `Config::paths::nix_profiles_dir`.
#[derive(Debug, Clone)]
pub struct Generation {
    /// Generation number parsed from `system-<N>-link`.
    pub number: u32,
    /// Full path to the profile symlink itself
    /// (e.g. `/mnt/system/nix/var/nix/profiles/system-42-link`).
    pub profile_link: PathBuf,
    /// Resolved path to the kernel image.
    pub kernel: PathBuf,
    /// Resolved path to the initrd.
    pub initrd: PathBuf,
    /// Path to the NixOS stage-2 `init` script as referenced from
    /// `<profile_link>/init`. Intentionally NOT canonicalized: the chained
    /// kernel needs the path through the profile symlink so the store path
    /// it executes matches what we hand it on the cmdline.
    pub init_path: PathBuf,
    /// Contents of `profile_link/kernel-params`, split on whitespace.
    pub kernel_params: Vec<String>,
    /// Best-effort label from `profile_link/nixos-version`. Empty when the
    /// file is missing or unreadable.
    pub label: String,
}

pub use scan::{active_generation_index, scan_generations};
