{
  config,
  lib,
  pkgs,
  nmblBootUpdate ? null,
  ...
}:

let
  cfg = config.boot.nmbl;
  update = cfg.bootUpdate;
  user = config.users.users.${update.user} or { };
  publicKey = if update.publicKey == null then "" else toString update.publicKey;
in
{
  options.boot.nmbl.bootUpdate = {
    enable = lib.mkEnableOption "the authenticated two-party A/B boot-set updater";
    user = lib.mkOption {
      type = lib.types.str;
      default = "nmbl-update";
      description = "Unprivileged account allowed to request boot-set activation.";
    };
    publicKey = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = "Public ML-DSA key used independently by the privileged receiver.";
    };
    socketPath = lib.mkOption {
      type = lib.types.str;
      default = "/run/nmbl-boot-update/update.sock";
    };
    spoolPath = lib.mkOption {
      type = lib.types.str;
      default = "/var/lib/nmbl-boot-update/spool";
    };
    bootRoots = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ "/boot" ];
      description = "Boot filesystem mountpoints, updated and verified one complete mirror at a time.";
    };
  };

  config = lib.mkIf (cfg.enable && update.enable) {
    assertions = [
      {
        assertion = nmblBootUpdate != null;
        message = "boot.nmbl.bootUpdate requires the nmbl-boot-update package";
      }
      {
        assertion = update.publicKey != null;
        message = "boot.nmbl.bootUpdate.publicKey is required";
      }
      {
        assertion = (user.uid or null) != null;
        message = "boot.nmbl.bootUpdate.user must name a user with an explicit numeric uid";
      }
      {
        assertion = (cfg.bootstrapper.loader or "grub") == "grub";
        message = "A/B boot sets currently require GRUB as the stable selector-aware dispatcher";
      }
      {
        assertion = cfg.signing.enable && cfg.signing.enforce;
        message = "A/B boot sets require enforced signed external configuration";
      }
    ];

    environment.systemPackages = [ nmblBootUpdate ];
    systemd.tmpfiles.rules = [
      "d /var/lib/nmbl-boot-update 0710 root ${update.user} - -"
      "d ${update.spoolPath} 0700 ${update.user} ${update.user} - -"
    ];
    systemd.services.nmbl-boot-update = {
      description = "Privileged NMBL whole-boot-set update receiver";
      wantedBy = [ "multi-user.target" ];
      after = [ "local-fs.target" ];
      serviceConfig = {
        Type = "simple";
        User = "root";
        Group = update.user;
        RuntimeDirectory = "nmbl-boot-update";
        RuntimeDirectoryMode = "02770";
        ExecStart = lib.concatStringsSep " " (
          [
            "${nmblBootUpdate}/bin/nmbl-boot-update"
            "serve"
            (lib.escapeShellArg update.socketPath)
            (lib.escapeShellArg update.spoolPath)
            (lib.escapeShellArg publicKey)
            (toString user.uid)
          ] ++ map lib.escapeShellArg update.bootRoots
        );
        Restart = "on-failure";
        NoNewPrivileges = true;
        PrivateTmp = true;
        ProtectSystem = "strict";
        ReadWritePaths = update.bootRoots ++ [ update.spoolPath "/run/nmbl-boot-update" ];
        ProtectHome = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        RestrictAddressFamilies = [ "AF_UNIX" ];
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
      };
    };
  };
}
