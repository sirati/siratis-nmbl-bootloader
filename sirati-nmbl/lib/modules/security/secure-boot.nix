# NMBL secure-boot policy options (#10 — the top-level secure-boot slice).
#
# Self-registering: dropped into `lib/modules/security/`, auto-imported by
# `../default.nix` (FIX-60), so adding this group edits no shared `imports`
# line. Declares `boot.nmbl.secureBoot.*` — the ONE priority-volume concept
# (R-3) plus the refuse-screen countdown, rescue sentinel, and the
# enforcement / TPM posture. Wire-mirrored by the Rust `SecureBootConfig`
# (`src/config/secure_boot.rs`) and emitted by `lib/config-toml.nix`.
#
# Declaring `boot.nmbl.secureBoot.enable` here makes the `or false`
# fallbacks in security-consts.nix / signing.nix / tpm.nix / staged-boot.nix
# real: `mkSecureBootActive` already ORs `secureBoot.enable`, so this option
# completes the implication (FIX-16) without rewriting security-consts.nix.
#
# Assertions (build-time, fail before any install):
#   * `enable ⇒ enforce` UNLESS `allowAuditModeInsecure` (FIX-31) — audit
#     mode needs two deliberate flags, never one.
#   * `enable ⇒ (signing.enable || signing.publicKeys != [])` so a #5
#     priority gate has a baked trust anchor to verify against.
#   * `allowedKeyIds` must be non-empty when MORE THAN ONE signing key is
#     baked (FIX-54) — best-effort build WARNING, since the baked-key count
#     is only known once #13 wires `publicKeys`.

{ lib, config, ... }:

let
  cfg = config.boot.nmbl.secureBoot;

  # The signing slice (#6) contributes `signing.enable`/`signing.publicKeys`.
  # Read directly — both slices land together in F1, so by the F1 exit gate
  # these always exist. The `or` fallbacks keep eval robust if the secure
  # boot table is exercised in isolation during the landing window (FIX-60).
  signing = config.boot.nmbl.signing or { };
  signingEnabled = signing.enable or false;
  signingKeys = signing.publicKeys or [ ];

  # Single-sourced security defaults (refuseCountdownSeconds = 30, sentinel
  # path). Same file the Rust `security_consts` mirrors and the round-trip
  # test guards (FIX-38).
  sc = import ../../security-consts.nix { inherit lib; };

  priorityVolumeOptions = {
    device = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = lib.mdDoc ''
        Block device (or `/dev/mapper/<x>` node) backing the priority
        volume NMBL mounts read-only and verifies the signed file on
        before proceeding (R-3). `null` (the default) means no priority
        volume is configured and the `[secure_boot.priority_volume]`
        sub-table is not emitted.
      '';
    };

    mountpoint = lib.mkOption {
      type = lib.types.str;
      default = "/mnt/nmbl-priority";
      description = lib.mdDoc ''
        Initramfs mountpoint the gate binds the priority volume at before
        reading the signed file.
      '';
    };

    fstype = lib.mkOption {
      type = lib.types.str;
      default = "ext4";
      description = lib.mdDoc ''
        Filesystem type NMBL mounts the priority volume as.
      '';
    };

    options = lib.mkOption {
      type = lib.types.str;
      default = "ro,nosuid,nodev,noexec";
      description = lib.mdDoc ''
        Mount options for the priority volume. Defaults to the hardened
        read-only set; the gate enforces `ro` regardless.
      '';
    };

    insideLuks = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = lib.mdDoc ''
        Whether the priority volume is a LUKS-backed mapper node. When
        `true` the volume only appears after a LUKS activation and the
        refuse path closes the mapper as part of sealing (FIX-03/FIX-21).
      '';
    };
  };
in
{
  options.boot.nmbl.secureBoot = {
    enable = lib.mkEnableOption ''
      NMBL secure boot: mount and verify a signed file on a priority volume
      before proceeding to a measured boot or consuming a staged fragment
      (R-3). Enabling this pulls the `secure-boot` verifier into /init via
      `secureBootActive` (FIX-16)
    '';

    priorityVolume = priorityVolumeOptions;

    signedFilePath = lib.mkOption {
      type = lib.types.str;
      default = "nmbl/priority.signed";
      description = lib.mdDoc ''
        Path of the signed file NMBL reads off the priority volume and
        verifies against the baked trust anchor (domain
        `nmbl:priority-file:v1`), relative to the priority-volume
        mountpoint.
      '';
    };

    allowedKeyIds = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = lib.mdDoc ''
        Trust-anchor key fingerprints the priority gate narrows to before
        verifying (full `fp()` fingerprints, R-3/FIX-08). Empty narrows to
        the whole baked set. Required to be non-empty when more than one
        signing key is baked (FIX-54) — otherwise a warning is emitted.
      '';
    };

    sentinelPath = lib.mkOption {
      type = lib.types.str;
      default = sc.defaults.sentinelPath;
      defaultText = lib.literalMD "`security-consts.nix` `defaults.sentinelPath` (= `/boot/nmbl/rescue`).";
      description = lib.mdDoc ''
        Sentinel file whose presence forces a rescue boot and keeps the TPM
        capped (FIX-21/FIX-38). Single-sourced from `security-consts.nix`.
      '';
    };

    enforce = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = lib.mdDoc ''
        Fail-closed enforcement. When `true`, a priority-gate / signature
        failure refuses the boot (reboots into rescue). When `false` with
        `enable = true` the checks run but only warn (audit mode), which the
        assertion below gates behind `allowAuditModeInsecure` (FIX-31).
      '';
    };

    requireTpm = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = lib.mdDoc ''
        Hard-require a working TPM for the secure-boot path: abort instead of
        degrading to an unmeasured boot (FIX-28). Mirrors `tpm.requireTpm`;
        surfaced here so the secure-boot table can demand TPM presence even
        when `[tpm].measure` is left at its default.
      '';
    };

    refuseCountdownSeconds = lib.mkOption {
      type = lib.types.int;
      default = sc.defaults.refuseCountdownSeconds;
      defaultText = lib.literalMD "`security-consts.nix` `defaults.refuseCountdownSeconds` (= 30).";
      description = lib.mdDoc ''
        Countdown, in seconds, for the non-interactive refuse screen before
        auto-reboot (R-13/FIX-39). The ONLY path; `policy.*` is superseded.
        Single-sourced from `security-consts.nix`.
      '';
    };

    allowAuditModeInsecure = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = lib.mdDoc ''
        Deliberate opt-in to insecure audit mode (`enable && !enforce`).
        Required by the `enable ⇒ enforce` assertion so audit mode needs two
        distinct flags, never one (FIX-31). Leave `false` for production.
      '';
    };
  };

  config = {
    assertions = [
      {
        # FIX-31: enabling secure boot implies fail-closed enforcement,
        # UNLESS the operator deliberately opts into insecure audit mode via
        # the SEPARATE `allowAuditModeInsecure` flag. Direction is
        # `enable ⇒ enforce` — never the reverse.
        assertion = cfg.enable -> (cfg.enforce || cfg.allowAuditModeInsecure);
        message = ''
          boot.nmbl.secureBoot.enable is set but enforce is false. The
          secure-boot priority gate without enforcement is AUDIT MODE
          (warn-only), which is insecure for production. Set
          boot.nmbl.secureBoot.enforce = true to fail closed, or deliberately
          opt into audit mode by ALSO setting
          boot.nmbl.secureBoot.allowAuditModeInsecure = true.
        '';
      }
      {
        # A #5 priority gate must have a baked trust anchor to verify the
        # signed priority file against. Require that signing is enabled (its
        # publicKeys are baked) or that keys are configured directly.
        assertion = cfg.enable -> (signingEnabled || signingKeys != [ ]);
        message = ''
          boot.nmbl.secureBoot.enable is set but no trust anchor is
          configured: the priority gate verifies a signed file against the
          baked ML-DSA public keys, so it needs at least one. Set
          boot.nmbl.signing.enable = true (and boot.nmbl.signing.publicKeys),
          or provide boot.nmbl.signing.publicKeys directly.
        '';
      }
    ];

    # FIX-54 (best-effort build WARNING): with more than one signing key
    # baked, an empty allowedKeyIds means the gate narrows to the whole set
    # — usually a mis-configuration (the operator likely meant to pin which
    # key signs the priority file). Warn, don't fail: the baked-key count is
    # only fully known once #13 wires publicKeys, so this is advisory.
    warnings = lib.optional (cfg.enable && lib.length signingKeys > 1 && cfg.allowedKeyIds == [ ]) ''
      boot.nmbl.secureBoot: ${toString (lib.length signingKeys)} signing keys are baked but
      boot.nmbl.secureBoot.allowedKeyIds is empty, so the priority gate narrows to the whole
      baked key set. Pin the key(s) allowed to sign the priority file by listing their
      fingerprints in allowedKeyIds (FIX-54).
    '';
  };
}
