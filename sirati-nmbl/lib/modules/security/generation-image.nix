{ config, lib, pkgs, utils, ... }:

let
  cfg = config.boot.nmbl.generationImage;
  signing = config.boot.nmbl.signing;
  matching = lib.filter (fs: fs.mountPoint == cfg.mountPoint) (
    builtins.attrValues config.boot.nmbl.fileSystems
  );
  generationFs = config.fileSystems.${cfg.mountPoint} or { };
  imagePath = generationFs.device or "";
  store = cfg.stage1Store;
  storeMatches = lib.filter (fs: store != null && fs.mountPoint == store.targetMountPoint) (
    builtins.attrValues config.boot.nmbl.fileSystems
  );
  storeFileSystem =
    if store == null then null
    else config.fileSystems.${store.targetMountPoint} or null;
  storeNeededForBoot =
    storeFileSystem != null && utils.fsNeededForBoot storeFileSystem;
  storePrefix =
    if store == null then null
    else if store.targetMountPoint == "/" then "/"
    else "${store.targetMountPoint}/";
  relativeStateRoot =
    if store == null then null
    else lib.removePrefix storePrefix cfg.stateRoot;
  targetMountPoint = "/sysroot${cfg.mountPoint}";
  targetImagePath = "/sysroot${imagePath}";
  targetSignaturePath = "/sysroot${cfg.signaturePath}";
  verifiedDevice = "/dev/nmbl-verified-generation";
  targetMountUnit = "${utils.escapeSystemdPath targetMountPoint}.mount";
  stateEnabled = cfg.automaticRollback || cfg.automaticRescue;
  helperArgs = lib.escapeShellArgs [
    "/bin/nmbl-generation-mount"
    "/etc/nmbl/generation-mount.toml"
    targetImagePath
    targetSignaturePath
    targetMountPoint
    verifiedDevice
  ];
in
{
  options.boot.nmbl.generationImage = {
    enable = lib.mkEnableOption "pre- and post-kexec verification for an atomically selected EROFS generation image";

    mountPoint = lib.mkOption {
      type = lib.types.str;
      default = "/nix";
      description = ''
        Mountpoint of the loop-backed filesystem entry that contains the
        selected Nix generation. NMBL verifies its image over the same file
        descriptor passed to LOOP_CONFIGURE. The target initrd must repeat
        pinned-file verification because loop devices do not survive kexec.
      '';
    };

    signaturePath = lib.mkOption {
      type = lib.types.str;
      default = "/boot/nmbl-generations/active/nix.erofs.sig";
      description = ''
        Detached ML-DSA signature for the selected image. The conventional
        `active` directory symlink is replaced atomically by nmbl-erofsctl.
      '';
    };

    stateRoot = lib.mkOption {
      type = lib.types.str;
      default = "/boot/nmbl-generations";
      description = "Persistent generation selection state on the boot volume.";
    };

    automaticRollback = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Roll back an untested generation when its prior boot did not complete.";
    };

    automaticRescue = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Enter the configured external rescue after a tested generation fails.";
    };

    successDelaySec = lib.mkOption {
      type = lib.types.ints.positive;
      default = 30;
      description = "Delay before assessing normal boot completion and marking a generation tested.";
    };

    stage1Store = lib.mkOption {
      type = lib.types.nullOr (lib.types.submodule {
        options = {
          targetMountPoint = lib.mkOption {
            type = lib.types.str;
            description = "Final-system mount containing stateRoot and the generation images.";
          };
          runtimeMountPoint = lib.mkOption {
            type = lib.types.str;
            default = "/mnt/nmbl-generation-store";
            description = "Private stage-1 mountpoint for the generation backing filesystem.";
          };
        };
      });
      default = null;
      description = ''
        Mount the filesystem at targetMountPoint during NMBL stage 1, so
        large generation images can live outside the boot partition.
      '';
    };
  };

  config = lib.mkMerge [
    {
      boot.nmbl.rescue.sfsPath = lib.mkIf cfg.enable "${relativeStateRoot}/active/rescue.sfs";
      boot.nmbl.rescue.fullSystem.networkStage.imagePath = lib.mkIf cfg.enable "${relativeStateRoot}/active/network.erofs";
      assertions = lib.optionals cfg.enable [
        {
          assertion = signing.enable && signing.enforce;
          message = "boot.nmbl.generationImage requires signing.enable and signing.enforce";
        }
        {
          assertion = builtins.length matching == 1;
          message = "boot.nmbl.generationImage.mountPoint must match exactly one boot.nmbl.fileSystems entry";
        }
        {
          assertion = builtins.all (fs: lib.elem "loop" fs.options && fs.fsType == "erofs") matching;
          message = "the signed generation image filesystem must be loop-backed EROFS";
        }
        {
          assertion = config.boot.initrd.systemd.enable;
          message = "boot.nmbl.generationImage requires the systemd initrd for verified post-kexec mounting";
        }
        {
          assertion = lib.hasPrefix "/" cfg.mountPoint && cfg.mountPoint != "/";
          message = "boot.nmbl.generationImage.mountPoint must be an absolute non-root path";
        }
        {
          assertion = lib.hasPrefix "/" imagePath && !(lib.hasPrefix "/dev/" imagePath);
          message = "the signed generation image device must be an absolute backing-file path";
        }
        {
          assertion = lib.hasPrefix "/" cfg.signaturePath && !(lib.hasPrefix "/dev/" cfg.signaturePath);
          message = "boot.nmbl.generationImage.signaturePath must be an absolute backing-filesystem path";
        }
        {
          assertion =
            if store == null then lib.hasPrefix "/boot/" cfg.stateRoot
            else
              lib.hasPrefix storePrefix cfg.stateRoot
              && relativeStateRoot != ""
              && builtins.all (component: component != "." && component != "..") (
                lib.splitString "/" relativeStateRoot
              );
          message = "generationImage.stateRoot must be below /boot or the configured stage1Store target";
        }
        {
          assertion = store == null || (builtins.length storeMatches == 1 && storeNeededForBoot);
          message = "generationImage.stage1Store.targetMountPoint must match exactly one needed-for-boot filesystem";
        }
        {
          assertion = store == null || (
            lib.hasPrefix "/" store.runtimeMountPoint
            && store.runtimeMountPoint != "/"
            && !(lib.hasInfix ".." store.runtimeMountPoint)
          );
          message = "generationImage.stage1Store.runtimeMountPoint must be a safe absolute non-root path";
        }
        {
          assertion = !cfg.automaticRescue || config.boot.nmbl.rescue.mode == "external";
          message = "generationImage.automaticRescue requires an external NMBL rescue image";
        }
        {
          assertion = !stateEnabled || config.boot.nmbl.configLocation == "external";
          message = "generationImage rollback/rescue state requires external config so /boot is mounted writable before selection";
        }
      ];
    }
    (lib.mkIf cfg.enable {
      boot.nmbl.bootstrap.kernelModules.explicit = lib.mkIf (builtins.length storeMatches == 1) (
        lib.mkAfter [ (builtins.head storeMatches).fsType ]
      );
      # The helper and config are both public build products. The ML-DSA public
      # key is baked into nmblInit; no private key is evaluated or copied.
      boot.initrd.systemd.extraBin.nmbl-generation-mount =
        "${config.system.build.nmblInit}/bin/nmbl-generation-mount";
      boot.initrd.systemd.contents."/etc/nmbl/generation-mount.toml".source =
        config.system.build.nmblConfigToml;

      # A native unit at this path outranks systemd-fstab-generator's ordinary
      # loop mount. The helper publishes `verifiedDevice` only after signature
      # verification and LOOP_CONFIGURE both succeed. Failure cannot fall back
      # to the mutable path because the unit then has no source device.
      # `initrd.nix` resolves duplicate mount-unit names with the first list
      # entry, so mkBefore also supersedes any older hand-written /nix unit.
      boot.initrd.systemd.mounts = lib.mkBefore [
        {
          where = targetMountPoint;
          what = verifiedDevice;
          type = "erofs";
          options = "ro,nodev,nosuid";
          after = [ "nmbl-generation-mount.service" ];
          requires = [ "nmbl-generation-mount.service" ];
          before = [ "initrd-fs.target" ];
          wantedBy = [ "initrd-fs.target" ];
          unitConfig.DefaultDependencies = "no";
        }
      ];

      boot.initrd.systemd.services.nmbl-generation-mount = {
        description = "Verify and mount the selected NMBL generation image";
        after = [ "sysroot.mount" "sysroot-boot.mount" ];
        requires = [ "sysroot.mount" "sysroot-boot.mount" ];
        before = [ targetMountUnit "initrd-fs.target" ];
        requiredBy = [ "initrd-fs.target" ];
        unitConfig = {
          DefaultDependencies = false;
          # The selected image commonly lives on /boot. Ask systemd to mount
          # its containing filesystem before the helper opens either sidecar.
          RequiresMountsFor = [
            (builtins.dirOf targetImagePath)
            (builtins.dirOf targetSignaturePath)
          ];
        };
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
          ExecStart = helperArgs;
        };
      };

      # systemd's own checker fails when any normal boot unit failed. The
      # success marker is ordered after it and before boot-complete.target, so
      # degraded boots retain `attempted` and are handled on the next boot.
      systemd.services.systemd-boot-check-no-failures = lib.mkIf stateEnabled {
        description = "Check whether any system unit failed";
        after = [ "default.target" "graphical.target" "multi-user.target" ];
        before = [ "boot-complete.target" ];
        requiredBy = [ "boot-complete.target" ];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
          ExecStart = "${config.systemd.package}/lib/systemd/systemd-boot-check-no-failures";
        };
      };

      systemd.services.nmbl-generation-success = lib.mkIf stateEnabled {
        description = "Mark the selected NMBL generation tested";
        after = [ "boot-complete.target" ];
        requires = [ "boot-complete.target" ];
        unitConfig.ConditionPathIsMountPoint = "/boot";
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
          ExecStart = "${config.system.build.nmblInit}/bin/nmbl-generation-state mark-success ${lib.escapeShellArg cfg.stateRoot}";
          User = "root";
          NoNewPrivileges = true;
          ProtectSystem = "strict";
          ReadWritePaths = [ cfg.stateRoot ];
          PrivateTmp = true;
        };
      };

      systemd.timers.nmbl-generation-success = lib.mkIf stateEnabled {
        description = "Assess NMBL generation boot success";
        wantedBy = [ "timers.target" ];
        timerConfig = {
          OnBootSec = "${toString cfg.successDelaySec}s";
          AccuracySec = "1s";
          Unit = "nmbl-generation-success.service";
        };
      };

      systemd.targets.nmbl-rollback-after-untested-new-generation-failed = {
        description = "NMBL automatically rolled back an untested generation";
        wantedBy = [ "multi-user.target" ];
        unitConfig.ConditionKernelCommandLine =
          "nmbl.rollback-after-untested-new-generation-failed";
      };
    })
  ];
}
