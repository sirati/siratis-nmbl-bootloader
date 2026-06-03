# NMBL driver-image group (#8). Self-registering security module: dropped
# into lib/modules/security/ so lib/modules/default.nix auto-imports it with
# no shared `imports +=` edit (FIX-60).
#
# A driver image is a detached, *verified* squashfs of out-of-tree kernel
# modules + their firmware that NMBL loop-mounts and `finit_module`s before
# kexec. Because such an image injects code into the running kernel, it MUST
# only ever be loaded when its signature can be verified — i.e. when the
# secure-boot posture is active. FIX-05 enforces that here at eval time:
# `driverImages.enable = true` WITHOUT an active secure-boot table is a build
# error, so an UNVERIFIED driver image can never reach the runtime loader.
#
# The driver-image FEATURE is otherwise always-compiled (no Cargo gate of its
# own); only the verify step needs `secure-boot`, which `secureBootActive`
# already pulls in whenever a signing/tpm/secureBoot table is enabled.

{ lib, config, ... }:

let
  cfg = config.boot.nmbl.driverImages;

  # The ONE `secureBootActive` implication boolean (FIX-16). True iff some
  # security table (signing/tpm/secureBoot) is enabled — which is also the
  # exact condition under which the `secure-boot` Cargo feature (and thus the
  # driver-image VERIFY path) is compiled into /init. Read from the single
  # source so this assertion can never drift from the feature derive.
  securityConsts = import ../../security-consts.nix { inherit lib; };
  secureBootActive = securityConsts.mkSecureBootActive config;

  imageModule =
    { name, config, ... }:
    {
      options = {
        modules = lib.mkOption {
          type = lib.types.listOf lib.types.str;
          default = [ ];
          description = lib.mdDoc ''
            Out-of-tree kernel module names this image provides, in the
            order NMBL `finit_module`s them after verifying and loop-mounting
            the squashfs. Dependencies must precede their dependents.
          '';
        };

        firmware = lib.mkOption {
          type = lib.types.listOf lib.types.package;
          default = [ ];
          description = lib.mdDoc ''
            Firmware packages staged into the image's `/lib/firmware`. These
            are build-time inputs only — they are baked into the squashfs, not
            referenced from the runtime config.toml.
          '';
        };

        blacklist = lib.mkOption {
          type = lib.types.listOf lib.types.str;
          default = [ ];
          description = lib.mdDoc ''
            In-tree module names to blacklist before loading this image's
            drivers, so a conflicting built-in does not claim the device
            first (e.g. blacklist `nouveau` before loading a proprietary GPU
            module).
          '';
        };

        path = lib.mkOption {
          type = lib.types.str;
          default = "nmbl/driver-${name}.sfs";
          defaultText = lib.literalExpression ''"nmbl/driver-<name>.sfs"'';
          description = lib.mdDoc ''
            Location of the driver squashfs RELATIVE TO THE BOOT PARTITION
            ROOT. NMBL joins this against the runtime boot mountpoint, so a
            leading `/` is unnecessary.
          '';
        };

        sigPath = lib.mkOption {
          type = lib.types.str;
          default = "${config.path}.sig";
          defaultText = lib.literalExpression ''"''${path}.sig"'';
          description = lib.mdDoc ''
            Location of the detached signature for this image, RELATIVE TO THE
            BOOT PARTITION ROOT. Verified against `boot.nmbl.signing.publicKeys`
            before the image is loop-mounted. Defaults to the image `path`
            with a `.sig` suffix.
          '';
        };
      };
    };
in
{
  options.boot.nmbl.driverImages = {
    enable = lib.mkEnableOption "loading verified NMBL driver-image squashfs blobs";

    images = lib.mkOption {
      type = lib.types.attrsOf (lib.types.submodule imageModule);
      default = { };
      description = lib.mdDoc ''
        Named driver images NMBL verifies, loop-mounts and `finit_module`s
        before kexec. Each entry is a detached, signed squashfs of out-of-tree
        kernel modules and their firmware.
      '';
    };
  };

  # FIX-05: an UNVERIFIED driver image must never be loadable. Enabling the
  # group without an active secure-boot table (which is what compiles the
  # verify path) is therefore a hard build error — fail closed rather than
  # silently load unsigned kernel code.
  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = secureBootActive;
        message = ''
          boot.nmbl.driverImages.enable = true requires an active secure-boot
          posture so each driver image's signature can be verified before its
          kernel modules are loaded. Enable one of boot.nmbl.signing.enable,
          boot.nmbl.tpm.measure or boot.nmbl.secureBoot.enable — otherwise NMBL
          would have no verified-load path and an unsigned driver image could
          inject arbitrary code into the kernel (FIX-05).
        '';
      }
    ];
  };
}
