# NMBL measured-boot (TPM) options module — slice #7.
#
# Self-registers via ../default.nix (FIX-60): dropping this file into
# ./security/ is the only wiring needed. Declares `boot.nmbl.tpm.*`,
# derives the `requireTpm` default (FIX-28), force-loads the TPM kernel
# modules early when measuring (R-8), and asserts the configured PCR set
# stays sane (a threat-model guard).
#
# The options here are wire-mirrored by `src/config/tpm.rs` (the
# always-compiled `TpmConfig`, FIX-09) and emitted by
# `lib/config-toml.nix`'s `[tpm]` block.

{ config, lib, ... }:

let
  cfg = config.boot.nmbl;
  tpm = cfg.tpm;

  # Single-sourced security defaults (lockPcr = 11). Same file the Rust
  # `security_consts::LOCK_PCR` mirrors and the round-trip test guards.
  sc = import ../../security-consts.nix { inherit lib; };

  # FIX-28: hard-require a TPM whenever we measure into it, or when
  # secure boot is enabled (which depends on measured PCRs). The
  # `secureBoot.enable or false` fallback keeps this evaluable before
  # slice #10 contributes that option (mirrors security-consts.nix).
  requireTpmDefault = tpm.measure || (cfg.secureBoot.enable or false);

  # PCR 7 (firmware/secure-boot state) is always part of a sane sealing
  # policy alongside NMBL's own lock PCR. The expected measured set is
  # {pcrIndex, 7}; it degenerates when `pcrIndex` IS 7 (the two collapse
  # into a single PCR that no longer separately binds NMBL's boot
  # events). Warn — don't fail — in that case.
  firmwarePcr = 7;
  expectedPcrs = lib.unique [ tpm.pcrIndex firmwarePcr ];
  pcrSetDegenerate = lib.length expectedPcrs != 2;

  # Threat-model guard over the OPERATOR'S sealing PCR set (FIX-11): the
  # per-device `boot.nmbl.activation.luks.<name>.tpmPcrs` lists the PCRs a
  # TPM-unlocked LUKS volume is sealed against. A sane policy seals to BOTH
  # NMBL's measure PCR (`pcrIndex`, the lock PCR NMBL extends the boot handoff
  # into) AND PCR 7 (firmware / secure-boot state). Sealing without PCR 7 does
  # NOT bind the SB-state, so a secret stays unsealable across a firmware that
  # silently stopped enforcing Secure Boot; sealing without the measure PCR
  # does not bind NMBL's boot handoff. We inspect only `unlock = "tpm"`
  # devices (tpmPcrs is meaningless otherwise) whose list is non-empty (an
  # empty list is "informational/unset", not a positive mis-seal). Warn — never
  # fail — naming each device + which required PCR it omits.
  tpmLuks = lib.filter (d: d.unlock == "tpm" && d.tpmPcrs != [ ]) (cfg.activation.luks or [ ]);
  sealMissesPcr = d: pcr: !(lib.elem pcr d.tpmPcrs);
  sealUnboundDevices = lib.filter (
    d: sealMissesPcr d tpm.pcrIndex || sealMissesPcr d firmwarePcr
  ) tpmLuks;
  describeUnbound =
    d:
    let
      missing = lib.optional (sealMissesPcr d tpm.pcrIndex) (toString tpm.pcrIndex)
        ++ lib.optional (sealMissesPcr d firmwarePcr) (toString firmwarePcr);
    in
    "${d.name} (sealed to {${lib.concatStringsSep ", " (map toString d.tpmPcrs)}}, "
    + "missing PCR ${lib.concatStringsSep " and " missing})";

  sealedSecretSubmodule = lib.types.submodule {
    options = {
      name = lib.mkOption {
        type = lib.types.str;
        description = lib.mdDoc "Stable identifier for the sealed secret (logs / TUI).";
      };
      sealedPath = lib.mkOption {
        type = lib.types.str;
        description = lib.mdDoc ''
          Boot-partition-relative path to the sealed blob, resolved
          against the runtime boot mountpoint at boot.
        '';
      };
      unsealTo = lib.mkOption {
        type = lib.types.path;
        description = lib.mdDoc "Absolute initramfs path the unsealed plaintext is written to.";
      };
    };
  };
in
{
  options.boot.nmbl.tpm = {
    measure = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = lib.mdDoc ''
        Extend the lock PCR (`pcrIndex`) with NMBL's boot events (R-7).
        When enabled NMBL also force-loads the TPM kernel modules early
        (R-8). Turning this on implies the `secure-boot` Cargo feature
        via `secureBootActive`.
      '';
    };

    pcrIndex = lib.mkOption {
      type = lib.types.int;
      default = sc.defaults.lockPcr;
      defaultText = lib.literalMD "`security-consts.nix` `defaults.lockPcr` (= 11).";
      description = lib.mdDoc ''
        PCR index NMBL measures into and caps to poison TPM-sealed
        secrets on a refuse. Single-sourced from `security-consts.nix`.
      '';
    };

    requireTpm = lib.mkOption {
      type = lib.types.bool;
      default = requireTpmDefault;
      defaultText = lib.literalMD ''
        `true` when `boot.nmbl.tpm.measure` or `boot.nmbl.secureBoot.enable`
        is set, `false` otherwise (FIX-28).
      '';
      description = lib.mdDoc ''
        Abort the boot if no usable TPM is present instead of degrading
        to an unmeasured boot. Defaults on whenever the security posture
        depends on the TPM (FIX-28).
      '';
    };

    device = lib.mkOption {
      type = lib.types.path;
      default = "/dev/tpmrm0";
      description = lib.mdDoc ''
        TPM device NMBL talks to. Defaults to the in-kernel resource
        manager `/dev/tpmrm0` — never the raw `/dev/tpm0`.
      '';
    };

    sealedSecrets = lib.mkOption {
      type = lib.types.listOf sealedSecretSubmodule;
      default = [ ];
      description = lib.mdDoc ''
        Secrets sealed to the TPM that NMBL unseals at boot (gated on the
        PCR policy). Each entry names a sealed blob on the boot partition
        and the initramfs path its plaintext is materialised at.
      '';
    };
  };

  config = {
    # R-8: when measuring — OR on a secure-boot box — the TPM transport
    # modules must be live before the measured-boot / seal-on-rescue path
    # runs, INDEPENDENT of any luks-tpm activation (which adds them to the
    # `explicit`/phase-2b list only). A secure-boot config may measure
    # nothing yet still needs `/dev/tpmrm0` to exist so the lock-on-rescue
    # guard can cap the PCR, so broaden the predicate to include
    # `secureBoot.enable` (the `or false` keeps it evaluable before slice
    # #10 contributes that option, mirroring `requireTpmDefault`).
    # Extending `earlyKernelModules` here means `earlyExplicitKernelModules`
    # (and thus `kernel_modules.early`) picks them up with the blacklist
    # applied. `mkIf` keeps the list untouched otherwise.
    boot.nmbl.earlyKernelModules =
      lib.mkIf (tpm.measure || (cfg.secureBoot.enable or false)) [ "tpm_crb" "tpm_tis" ];

    # Threat-model guards (warn — never fail):
    #
    # (1) A sane sealing policy binds both NMBL's lock PCR and PCR 7 (firmware
    #     / secure-boot state) — the set {pcrIndex, 7}. Warn when measuring
    #     with `pcrIndex = 7`, which collapses the two into one PCR and stops
    #     separately binding NMBL's boot events; this usually signals a
    #     mis-pinned `pcrIndex`.
    #
    # (2) Each TPM-unlocked LUKS device's sealing `tpmPcrs` should include
    #     BOTH the measure PCR (`pcrIndex`) AND PCR 7 (FIX-11). Sealing
    #     without PCR 7 does not bind the secure-boot state; sealing without
    #     the measure PCR does not bind NMBL's boot handoff.
    #
    # NOTE (FIX-42): what NMBL measures into the lock PCR is
    # `{kernel, pristine-initrd, driver-images, cmdline}` — explicitly NOT the
    # NMBL-injected cpio fragment (the log transcript + typed key material
    # spliced into the initrd at kexec). The pristine initrd is measured over
    # the same pinned fd the verifier hashed, so the attacker-uninfluenceable
    # fragment is intentionally outside the attested set; a sealing policy
    # bound to `pcrIndex` binds the pristine handoff, not the fragment.
    warnings =
      lib.optional (tpm.measure && pcrSetDegenerate) ''
        boot.nmbl.tpm: measured-boot pcrIndex = ${toString tpm.pcrIndex} collapses the
        expected PCR set {pcrIndex, ${toString firmwarePcr}} into a single register, so
        the sealing policy no longer separately binds NMBL's boot events from firmware /
        secure-boot state (PCR ${toString firmwarePcr}). Pick a distinct lock PCR.
      ''
      ++ lib.optional (sealUnboundDevices != [ ]) ''
        boot.nmbl.tpm: ${toString (lib.length sealUnboundDevices)} TPM-unlocked LUKS
        device(s) seal against a PCR set that does not bind BOTH NMBL's measure PCR
        (${toString tpm.pcrIndex}) and the secure-boot-state PCR (${toString firmwarePcr}):
        ${lib.concatStringsSep "; " (map describeUnbound sealUnboundDevices)}.
        Sealing without PCR ${toString firmwarePcr} does not bind the secure-boot state
        (a secret stays unsealable across a firmware that stopped enforcing Secure Boot);
        sealing without PCR ${toString tpm.pcrIndex} does not bind NMBL's boot handoff.
        Add both to each device's tpmPcrs. (NMBL measures {kernel, pristine-initrd,
        driver-images, cmdline} into PCR ${toString tpm.pcrIndex}, not the injected cpio
        fragment — FIX-42.)
      '';
  };
}
