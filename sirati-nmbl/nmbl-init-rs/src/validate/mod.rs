//! Build-time / installer-time config validation entry points that go
//! beyond the pure `--validate-config` schema check.
//!
//! Two distinct validators live here, and they MUST NOT be conflated:
//!
//! * [`hardware`] — `--validate-hardware`. Runs on the REAL target
//!   machine during bootloader install. Read-only, zero side effects.
//!   Uses ONLY the NMBL TOML against the actual hardware (device
//!   existence + LUKS headers). NOT NixOS-specific.
//!
//! * [`closure`] — `--validate-nix-filesystem-closure`. Pure sandbox
//!   check, NixOS-only. Compares the NMBL TOML against the JSON dump of
//!   the NixOS `config.fileSystems` closure. No hardware access.
//!
//! The pre-existing `--validate-config` mode (in `main_parts`) stays
//! untouched; it is toml-only, sandboxed and target-agnostic.

pub mod closure;
pub mod hardware;
pub mod tools;

pub use closure::validate_nix_filesystem_closure;
pub use hardware::validate_hardware;
pub use tools::ToolPaths;
