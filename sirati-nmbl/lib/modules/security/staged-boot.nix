# Staged-boot option module (slice #9). Self-registers into the NMBL
# module tree by being dropped under lib/modules/security/ — the
# auto-import hub (lib/modules/default.nix) picks it up with no shared
# `imports +=` edit (FIX-60).
#
# Declares `boot.nmbl.staged.*` (the priority-volume pointer set: image +
# signed config fragment + detached signature) and the matching
# `boot.nmbl.bootstrap.staged.*` sub-options consumed by the frozen
# bootstrap stage. The Rust mirrors are `StagedConfig` (config/staged.rs)
# and `BootstrapStaged` (config/bootstrap.rs), both gated behind the
# `staged-boot` Cargo feature.
#
# CRITICAL (FIX-26): `staged.enable = true` REQUIRES
# `boot.nmbl.secureBoot.enable`. Staged boot mounts a config fragment off a
# priority volume and applies it on top of the base config; doing that
# without signature verification would let an attacker inject config, so
# the staged path may never self-mount unverified. We enforce this as a
# build-time assertion (it fails `nixos-rebuild`/`--validate-config` before
# any install). `secureBoot.enable` is referenced via `or false` because
# the #10 secure-boot slice that declares it may not have landed in this
# tree yet; once it has, the F1 exit gate tightens the reference.
#
# NO `has_config_fragment` boolean anywhere (FIX-56): whether a fragment is
# actually present is a runtime file-existence check on the Rust side, not
# a build-time flag that could drift from on-disk reality.

{ lib, config, ... }:

let
  cfg = config.boot.nmbl.staged;
  # `or false` scaffolding for the #5p→#10 window (mirrors
  # security-consts.nix). The #10 slice contributes `secureBoot.enable`.
  secureBootEnabled = config.boot.nmbl.secureBoot.enable or false;
in
{
  options.boot.nmbl = {
    staged = {
      enable = lib.mkEnableOption ''
        staged boot: apply a signed config fragment (and drivers) from a
        verified priority volume on top of the base config. Requires
        boot.nmbl.secureBoot.enable
      '';

      image = lib.mkOption {
        type = lib.types.str;
        default = "nmbl-staged.img";
        description = lib.mdDoc ''
          Priority-volume image holding the signed config fragment and
          staged drivers, as a path relative to the priority-volume
          mountpoint. Emitted verbatim into `[staged].image`.
        '';
      };

      fragment = lib.mkOption {
        type = lib.types.str;
        default = "nmbl/fragment.toml";
        description = lib.mdDoc ''
          Signed config fragment inside the priority volume, relative to
          the priority-volume mountpoint. Applied on top of the base
          config once its signature verifies. Emitted into
          `[staged].fragment`.
        '';
      };

      sig = lib.mkOption {
        type = lib.types.str;
        default = "nmbl/fragment.toml.sig";
        description = lib.mdDoc ''
          Detached ML-DSA signature over the staged fragment, relative to
          the priority-volume mountpoint. Emitted into `[staged].sig`.
        '';
      };
    };

    bootstrap.staged = {
      mountpoint = lib.mkOption {
        type = lib.types.str;
        default = "/mnt/staged";
        description = lib.mdDoc ''
          Initramfs mountpoint where the bootstrap stage binds the
          verified priority volume before reading the staged fragment.
          Emitted into `[bootstrap.staged].mountpoint`.
        '';
      };

      fragment = lib.mkOption {
        type = lib.types.str;
        default = "nmbl/fragment.toml";
        description = lib.mdDoc ''
          Signed config fragment, relative to the bootstrap staged
          mountpoint. Emitted into `[bootstrap.staged].fragment`.
        '';
      };

      sig = lib.mkOption {
        type = lib.types.str;
        default = "nmbl/fragment.toml.sig";
        description = lib.mdDoc ''
          Detached ML-DSA signature over the bootstrap staged fragment,
          relative to the bootstrap staged mountpoint. Emitted into
          `[bootstrap.staged].sig`.
        '';
      };
    };
  };

  config = {
    # FIX-26: staged boot may never self-mount an unverified fragment, so
    # enabling it requires the signature-verifying secure-boot path.
    assertions = [
      {
        assertion = !cfg.enable || secureBootEnabled;
        message = ''
          boot.nmbl.staged.enable requires boot.nmbl.secureBoot.enable.
          Staged boot applies a config fragment from a priority volume on
          top of the base config; without secure boot's signature
          verification that fragment would be trusted unverified, which
          NMBL refuses (the staged path can never self-mount unverified).
          Enable boot.nmbl.secureBoot.enable, or unset
          boot.nmbl.staged.enable.
        '';
      }
    ];
  };
}
