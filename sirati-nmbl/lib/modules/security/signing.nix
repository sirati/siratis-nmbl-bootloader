# NMBL signature-enforcement options (#6 — secure-boot signing slice).
#
# Self-registering: dropped into `lib/modules/security/`, auto-imported by
# `../default.nix` (FIX-60), so adding this group edits no shared `imports`
# line. Declares `boot.nmbl.signing.*` — the POLICY for ML-DSA signature
# verification of generations/images and for install-time UKI Secure-Boot
# signing.
#
# Trust-material vs policy split (R-5/FIX-04):
#   * `publicKeys` are the trust anchor. They are `include_bytes!`-baked
#     into the `nmbl-init` binary (generated `src/sig/baked_keys.rs`, wired
#     by F2 #13), NEVER emitted to config.toml — a writable-boot artifact
#     must not be able to swap the trust anchor. `lib/config-toml.nix`
#     therefore emits `enable`/`enforce`/`algorithm`/`sigPathSuffix`/`uki`
#     but NOT `publicKeys`.
#   * Everything else here is enforcement POLICY mirrored by the Rust
#     `SigningConfig` (`src/config/signing.rs`).
#
# There is deliberately NO `allowUnsignedGenerations` option (FIX-04). The
# only relaxation of enforcement is audit mode (`enable && !enforce`), which
# is itself gated behind a SEPARATE deliberate `secureBoot.allowAuditModeInsecure`
# flag (FIX-31) so a production config is fail-closed by construction.

{ lib, config, ... }:
let
  cfg = config.boot.nmbl.signing;

  # `secureBoot.allowAuditModeInsecure` is contributed by the #10 secure-boot
  # slice and may not exist yet while the slices land in parallel (FIX-60).
  # `or false` keeps this module evaluable in that window; the F1 exit gate
  # tightens it to a direct reference once #10 has landed (FIX-57).
  allowAuditModeInsecure = config.boot.nmbl.secureBoot.allowAuditModeInsecure or false;
in
{
  options.boot.nmbl.signing = {
    enable = lib.mkEnableOption ''
      NMBL signature verification of boot generations and images. Enabling
      this compiles the `secure-boot` verifier into the /init binary. On its
      own (without `enforce`) it is AUDIT MODE: signatures are checked and
      mismatches logged, but a bad/missing signature does not refuse the
      boot — audit mode additionally requires `secureBoot.allowAuditModeInsecure`
    '';

    enforce = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = lib.mdDoc ''
        Fail-closed enforcement. When `true`, a bad or missing signature on
        a generation or image refuses the boot (reboots into rescue). When
        `false` with `enable = true` the verifier runs in audit mode (warn
        only). The assertion below requires `enable → enforce` unless
        `secureBoot.allowAuditModeInsecure` is set, so production configs are
        fail-closed (FIX-31). There is no per-generation unsigned-allow knob
        (FIX-04).
      '';
    };

    publicKeys = lib.mkOption {
      type = lib.types.listOf lib.types.path;
      default = [ ];
      description = lib.mdDoc ''
        Trust-anchor ML-DSA public keys (raw key bytes). These are
        `include_bytes!`-baked into the `nmbl-init` binary at build time
        (generated `src/sig/baked_keys.rs`), NOT written to config.toml — the
        config lives on a writable boot partition and must not be able to
        swap the trust anchor. Any one matching key verifies a signature
        (fail-closed any-of). At least one key is required when `enable` is
        set on a real secure-boot build (enforced where the keys are baked).
      '';
    };

    algorithm = lib.mkOption {
      type = lib.types.enum [ "ml-dsa-65" "ml-dsa-87" ];
      default = "ml-dsa-65";
      description = lib.mdDoc ''
        Signature algorithm the verifier expects. Mirrors the Rust-side
        `AlgId`; the per-key length check lands with the baked-key wiring.
      '';
    };

    sigPathSuffix = lib.mkOption {
      type = lib.types.str;
      default = ".sig";
      description = lib.mdDoc ''
        Filename suffix of the detached signature sidecars NMBL looks up
        next to each signed blob. Sidecar LOCATION is fixed by the boot flow
        (`/boot/nmbl/sigs/<gen-id>/…`); this only controls the suffix.
      '';
    };

    imageKeyFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = lib.mdDoc ''
        ML-DSA signing PRIVATE key, read IMPURELY at install time to sign
        detached image sidecars (currently driver images — `nmbl-sign sign
        --domain driver-image`). The pair of a baked `publicKeys` entry. Never
        embedded in the store or emitted to config.toml: pass it as a STRING
        path to an on-disk secret (e.g. `"/run/secrets/nmbl-img.key"`), NOT a
        Nix path literal like `./img.key` — a path literal would be imported
        into the store, and the build FAILS the eval (closure-leak assertion)
        if it resolves under the store dir. Required when
        `boot.nmbl.driverImages.enable` is set.
      '';
    };

    generationKeyFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = lib.mdDoc ''
        ML-DSA signing PRIVATE key, read IMPURELY at install time to sign each
        bootable NixOS generation's kernel and initrd (`nmbl-sign sign --domain
        gen-kernel` / `--domain gen-initrd`). The detached sidecars are written
        to the writable boot partition at
        `/boot/nmbl/sigs/<gen-id>/kernel<sigPathSuffix>` and `…/initrd…`, where
        `<gen-id>` is the content-addressed id NMBL computes at runtime via
        `nmbl-init --print-gen-id`; without these sidecars an ENFORCING install
        would refuse every generation at boot (the pre-kexec verify guard has
        nothing to check). This must be the PRIVATE half of a baked
        `publicKeys` entry (the operator is responsible for that pairing — the
        public key the in-initramfs verifier trusts is whichever `publicKeys`
        entry was baked into `nmbl-init`).

        Never embedded in the store or emitted to config.toml: pass it as a
        STRING path to an on-disk secret (e.g. `"/run/secrets/nmbl-gen.key"`),
        NOT a Nix path literal like `./gen.key` — a path literal would be
        imported into the store, and `lib/install-signing.nix` FAILS the eval
        (closure-leak assertion) if it resolves under the store dir. Required
        when `signing.enable` is set on a build that has bootable generations
        to sign.
      '';
    };

    uki = {
      enable = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = lib.mdDoc ''
          Sign the NMBL UKI with a firmware `db`-enrolled Secure-Boot key at
          INSTALL time (`sbsign`/`ukify`, R-9). The `nmblUki` derivation
          itself stays pure and unsigned; signing happens in
          `lib/install-signing.nix`.
        '';
      };

      keyFile = lib.mkOption {
        type = lib.types.nullOr lib.types.path;
        default = null;
        description = lib.mdDoc ''
          Secure-Boot signing private key, read IMPURELY at install time.
          Never embedded in the store or emitted to config.toml. Required
          when `signing.uki.enable` is set. Pass it as a STRING path to an
          on-disk secret (e.g. `"/run/secrets/nmbl-db.key"`), NOT a Nix path
          literal like `./db.key` — a path literal would be imported into the
          store, and `lib/install-signing.nix` FAILS the eval (closure-leak
          assertion) if `keyFile`/`certFile` resolves under the store dir.
        '';
      };

      certFile = lib.mkOption {
        type = lib.types.nullOr lib.types.path;
        default = null;
        description = lib.mdDoc ''
          Secure-Boot signing certificate matching `keyFile` (the `db`
          certificate the firmware enforces against). Required when
          `signing.uki.enable` is set.
        '';
      };

      refuseInstallIfNotEnforcing = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = lib.mdDoc ''
          Install-time db-enrollment policy (FIX-11). When `false` (default)
          the installer signs and installs the UKI but only WARNS LOUDLY if
          the running firmware would not actually refuse an unsigned UKI
          (Secure Boot off / setup-mode, or this cert not enrolled in `db`).
          When `true` the install ABORTS in that case, so a machine where the
          firmware->NMBL chain is not yet enforceable cannot be provisioned by
          accident. The detection degrades gracefully (warns, never blocks) if
          the firmware-state tools are unavailable. This is the INSTALL-TIME
          check only; the RUNTIME PCR-7 / Secure-Boot-state read at the start
          of the measured path is handled separately in NMBL's Rust init.
        '';
      };
    };
  };

  config = {
    assertions = [
      {
        # FIX-31: enabling verification implies fail-closed enforcement,
        # UNLESS the operator deliberately opts into insecure audit mode via
        # the SEPARATE `secureBoot.allowAuditModeInsecure` flag (two flags,
        # not one). Direction is `enable ⇒ enforce` — never the reverse.
        assertion = cfg.enable -> (cfg.enforce || allowAuditModeInsecure);
        message = ''
          boot.nmbl.signing.enable is set but enforce is false. Signature
          verification without enforcement is AUDIT MODE (warn-only), which
          is insecure for production. Set boot.nmbl.signing.enforce = true to
          fail closed, or deliberately opt into audit mode by ALSO setting
          boot.nmbl.secureBoot.allowAuditModeInsecure = true.
        '';
      }
      {
        # UKI signing needs both halves of the keypair at install time.
        assertion =
          cfg.uki.enable -> (cfg.uki.keyFile != null && cfg.uki.certFile != null);
        message = ''
          boot.nmbl.signing.uki.enable is set but keyFile and/or certFile is
          null. Install-time UKI Secure-Boot signing (sbsign/ukify) needs both
          the private key and its db certificate. Set
          boot.nmbl.signing.uki.keyFile and boot.nmbl.signing.uki.certFile.
        '';
      }
    ];
  };
}
