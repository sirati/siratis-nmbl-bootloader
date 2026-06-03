# Emits /etc/nmbl/bootstrap.toml — the minimal pre-stage config
# embedded directly into the NMBL initramfs when
# `boot.nmbl.configLocation = "external"`.
#
# Schema mirrors the Rust `BootstrapConfig` struct in
# `sirati-nmbl/nmbl-init-rs/src/config.rs` (which uses
# `serde(deny_unknown_fields)`, so the wire shape must match exactly:
# `[bootstrap]` plus the sub-tables `boot_fs`, `kernel_modules`, and
# optionally `rescue`).
#
# Used as a pure function: callers `import` this file and apply it
# with `{ pkgs, lib, config }` to get a derivation containing the
# rendered TOML. The B.3 staging step (lib/config.nix) decides whether
# to embed the result in the initramfs.

{
  pkgs,
  lib,
  config,
}:

let
  cfg = config.boot.nmbl;
  bootstrap = cfg.bootstrap;

  tomlFormat = pkgs.formats.toml { };

  # Single source of the staged-boot emit/feature gate (FIX-40): the SAME
  # boolean drives the `staged-boot` Cargo feature in lib/signing-build.nix
  # and the `[bootstrap.staged]` emit below, so a feature-free binary never
  # receives a `[bootstrap.staged]` table it cannot parse.
  stagedBootActive = (import ./security-consts.nix { inherit lib; }).mkStagedBootActive config;

  # `rescue` is optional in the Rust schema (`#[serde(default)]`).
  # Only emit the sub-table when one of its fields is non-empty so the
  # generated TOML matches the "both empty" path of `BootstrapConfig::validate`
  # without adding a no-op section.
  rescueSet =
    bootstrap.rescue.defaultUrl != "" || bootstrap.rescue.defaultSha256 != "";

  tomlValue = {
    bootstrap = {
      config_path = toString bootstrap.configPath;

      boot_fs = {
        device = bootstrap.bootFs.device;
        fstype = bootstrap.bootFs.fstype;
        options = bootstrap.bootFs.options;
        mountpoint = toString bootstrap.bootFs.mountpoint;
      };

      kernel_modules = {
        explicit = bootstrap.kernelModules.explicit;
        modules_dir = bootstrap.kernelModules.modulesDir;
      };
    } // lib.optionalAttrs rescueSet {
      rescue = {
        default_url = bootstrap.rescue.defaultUrl;
        default_sha256 = bootstrap.rescue.defaultSha256;
      };
    } // lib.optionalAttrs cfg.stateful.enable {
      state = {
        mountpoint = toString cfg.stateful.rwMountpoint;
      };
    } // lib.optionalAttrs stagedBootActive {
      # Staged-boot pointer set for the frozen bootstrap stage. Emitted
      # ONLY when staged boot is active so a binary built WITHOUT
      # `staged-boot` (which `#[cfg]`s the `staged` field off
      # `BootstrapSection`) never parses a table its `deny_unknown_fields`
      # rejects (FIX-40). Same `stagedBootActive` gate as the Cargo
      # feature. No `has_config_fragment` key (FIX-56).
      staged = {
        mountpoint = toString bootstrap.staged.mountpoint;
        fragment = bootstrap.staged.fragment;
        sig = bootstrap.staged.sig;
      };
    };
  };
in
tomlFormat.generate "nmbl-bootstrap.toml" tomlValue
