//! Stub for the generation-discovery module.
//!
//! The full implementation (phase 4 scanner) lands in a sibling PR; this
//! file exposes only the [`Generation`] data type that other phases depend
//! on. Keeping the struct here means `boot.rs` and the TUI can compile
//! against the same field layout the scanner will populate.

use std::path::PathBuf;

/// A single NixOS system generation discovered under
/// `config.paths.nix_profiles_dir`.
///
/// Field semantics mirror what `scripts/find-generations.sh.nix`
/// surfaced in shell variables (`SELECTED_GEN`, `SELECTED_KERNEL`, …).
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
    /// Contents of `profile_link/kernel-params`, split on whitespace.
    pub kernel_params: Vec<String>,
    /// Best-effort label from `profile_link/nixos-version`. Empty when the
    /// file is missing or unreadable.
    pub label: String,
}
