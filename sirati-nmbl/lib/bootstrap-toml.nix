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
      };
    } // lib.optionalAttrs rescueSet {
      rescue = {
        default_url = bootstrap.rescue.defaultUrl;
        default_sha256 = bootstrap.rescue.defaultSha256;
      };
    };
  };
in
tomlFormat.generate "nmbl-bootstrap.toml" tomlValue
