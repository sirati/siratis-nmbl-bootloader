{ lib, config, ... }:

let
  cfg = config.boot.nmbl.rescue.fullSystem;
  stage = cfg.networkStage;
  hostKey = cfg.hostKeyPath;
in
{
  options.boot.nmbl.rescue.fullSystem = {
    hostKeyPath = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "/mnt/boot-state/secrets/rescue-ssh-host-ed25519";
      description = lib.mdDoc ''
        Persistent Ed25519 host private key path in NMBL's pre-chroot mount
        namespace. Rescue exposes it below `/nmbl-root`, verifies root:root
        ownership and mode 0600, then copies it into the writable rescue `/etc`.
        The key must be provisioned outside Nix on a volume mounted before
        rescue dispatch (for example the bootstrap state volume), and must not
        be a store path.
      '';
    };

    networkStage = {
      enable = lib.mkEnableOption "the signed rescue networking EROFS stage";

      imagePath = lib.mkOption {
        type = lib.types.str;
        default = "nmbl/network.erofs";
        description = "Path of the networking EROFS image relative to /boot.";
      };

      addressFamily = lib.mkOption {
        type = lib.types.enum [
          "dual-stack"
          "ipv4-only"
          "ipv6-only"
        ];
        default = "dual-stack";
        description = "Address families requested from dhcpcd in rescue.";
      };

      interfaces = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        example = [ "enp1s0" ];
        description = "Interfaces to configure; an empty list discovers all NICs.";
      };
    };
  };

  config = lib.mkIf stage.enable {
    assertions = [
      {
        assertion = cfg.enable && config.boot.nmbl.rescue.mode == "external";
        message = "The rescue networking stage requires external full-system rescue.";
      }
      {
        assertion = config.boot.nmbl.signing.enable && config.boot.nmbl.signing.enforce;
        message = "The networking EROFS stage requires enforced NMBL signing.";
      }
      {
        assertion = hostKey != null && !(lib.hasPrefix builtins.storeDir hostKey);
        message = ''
          The networking rescue requires fullSystem.hostKeyPath outside
          /nix/store so SSH has a stable, impermanence-safe host identity.
        '';
      }
      {
        assertion = hostKey != null && lib.hasPrefix "/" hostKey && !(lib.hasInfix ".." hostKey);
        message = "fullSystem.hostKeyPath must be an absolute path without `..`.";
      }
      {
        assertion = lib.all (iface: builtins.match "[A-Za-z0-9_.:-]+" iface != null) stage.interfaces;
        message = "networkStage.interfaces entries must be plain interface names.";
      }
      {
        assertion = !(lib.hasPrefix "/" stage.imagePath) && !(lib.hasInfix ".." stage.imagePath);
        message = "networkStage.imagePath must be a safe path relative to /boot.";
      }
    ];
  };
}
