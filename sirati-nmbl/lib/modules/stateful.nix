{ lib, pkgs, config, ... }:
let
  cfg = config.boot.nmbl.stateful;
in
{
  options.boot.nmbl.stateful = {
    enable = lib.mkEnableOption "NMBL stateful boot tracking with rollback";

    maxRecoveryAttempts = lib.mkOption {
      type = lib.types.ints.positive;
      default = 5;
      description = lib.mdDoc ''
        How many failed boots NMBL tries before dropping to rescue.
      '';
    };

    successTarget = lib.mkOption {
      type = lib.types.str;
      default = "multi-user.target";
      description = lib.mdDoc ''
        systemd target after which boot is declared successful.
      '';
    };

    stateDir = lib.mkOption {
      type = lib.types.str;
      default = "/boot/nmbl";
      description = lib.mdDoc ''
        Directory on /boot holding state.bin (relative to the booted
        system's root).
      '';
    };

    rwMountpoint = lib.mkOption {
      type = lib.types.str;
      default = "/mnt/boot-state";
      description = lib.mdDoc ''
        Initramfs mountpoint NMBL uses for the RW twin mount of the
        boot partition.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    # Userspace notifier: once the booted system reaches the operator's
    # chosen target, tell NMBL the attempt succeeded so the next boot
    # uses the current generation as the known-good entry instead of
    # decrementing the retry counter.
    systemd.services.nmbl-boot-succeeded = {
      description = "Notify NMBL bootloader that boot succeeded";
      wantedBy = [ cfg.successTarget ];
      after = [ cfg.successTarget ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = "${config.system.build.nmblInit}/bin/nmbl-init --boot-succeeded ${cfg.stateDir}";
      };
    };
  };
}
