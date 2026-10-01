{ config, lib, ... }:

let
  cfg = config.boot.nmbl.generationImage;
  enabled = cfg.enable && (cfg.automaticRollback || config.boot.nmbl.rescue.automatic);
  checker = "${config.systemd.package}/lib/systemd/systemd-boot-check-no-failures";
in
{
  config = lib.mkIf enabled {
    systemd.services.systemd-boot-check-no-failures = {
      description = "Check whether any system unit failed";
      after = [ "default.target" "graphical.target" "multi-user.target" ];
      before = [ "boot-complete.target" ];
      requiredBy = [ "boot-complete.target" ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = checker;
      };
    };

    systemd.services.nmbl-generation-success = {
      description = "Mark the selected NMBL generation tested";
      after = [ "boot-complete.target" ];
      requires = [ "boot-complete.target" ];
      unitConfig.ConditionPathIsMountPoint = "/boot";
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        # Secrets may arrive after boot-complete.target was first reached.
        # Assess current health when marking, rather than reuse that snapshot.
        ExecStartPre = checker;
        ExecStart = "${config.system.build.nmblInit}/bin/nmbl-generation-state mark-success ${lib.escapeShellArg cfg.stateRoot}";
        User = "root";
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ReadWritePaths = [ cfg.stateRoot ];
        PrivateTmp = true;
      };
    };

    systemd.timers.nmbl-generation-success = {
      description = "Assess NMBL generation boot success";
      wantedBy = [ "timers.target" ];
      timerConfig = {
        OnBootSec = "${toString cfg.successDelaySec}s";
        # Dependency failures never run the service, so its activation and
        # inactivity timestamps cannot reliably schedule another attempt.
        OnCalendar = "*-*-* *:*:00/30";
        AccuracySec = "1s";
        Unit = "nmbl-generation-success.service";
      };
    };
  };
}
