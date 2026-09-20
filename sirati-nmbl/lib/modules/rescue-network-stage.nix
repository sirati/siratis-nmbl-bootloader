{ lib, config, ... }:

let
  cfg = config.boot.nmbl.rescue.fullSystem;
  stage = cfg.networkStage;
  hostKey = cfg.hostKeyPath;
  routeOptions =
    { ... }:
    {
      options = {
        destination = lib.mkOption {
          type = lib.types.str;
          description = "CIDR destination or `default`.";
        };
        via = lib.mkOption {
          type = lib.types.nullOr lib.types.str;
          default = null;
          description = "Gateway address; null creates a device route.";
        };
        onLink = lib.mkOption {
          type = lib.types.bool;
          default = false;
          description = "Pass the route's onlink flag to iproute2.";
        };
      };
    };
  familyOptions =
    { ... }:
    {
      options = {
        addresses = lib.mkOption {
          type = lib.types.listOf lib.types.str;
          default = [ ];
          description = "Static addresses with prefix lengths.";
        };
        gateway = lib.mkOption {
          type = lib.types.nullOr lib.types.str;
          default = null;
          description = "Optional default gateway address.";
        };
        gatewayOnLink = lib.mkOption {
          type = lib.types.bool;
          default = false;
          description = "Treat the default gateway as directly reachable.";
        };
        routes = lib.mkOption {
          type = lib.types.listOf (lib.types.submodule routeOptions);
          default = [ ];
          description = "Additional static routes.";
        };
      };
    };
  profileOptions =
    { ... }:
    {
      options = {
        interfaceName = lib.mkOption {
          type = lib.types.nullOr lib.types.str;
          default = null;
          description = "Exact kernel interface name selector.";
        };
        macAddress = lib.mkOption {
          type = lib.types.nullOr lib.types.str;
          default = null;
          description = "Canonical six-octet MAC selector.";
        };
        ipv4 = lib.mkOption {
          type = lib.types.submodule familyOptions;
          default = { };
        };
        ipv6 = lib.mkOption {
          type = lib.types.submodule familyOptions;
          default = { };
        };
      };
    };
  validName = value: builtins.match "[A-Za-z0-9_.:-]+" value != null;
  validMac = value: builtins.match "[0-9A-Fa-f]{2}(:[0-9A-Fa-f]{2}){5}" value != null;
  profilesValid = lib.all (
    profile:
    ((profile.interfaceName != null) != (profile.macAddress != null))
    && (profile.interfaceName == null || validName profile.interfaceName)
    && (profile.macAddress == null || validMac profile.macAddress)
    && (profile.ipv4.addresses != [ ] || profile.ipv6.addresses != [ ])
  ) stage.staticProfiles;
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

      staticProfiles = lib.mkOption {
        type = lib.types.listOf (lib.types.submodule profileOptions);
        default = [ ];
        description = ''
          Data-only static network profiles. Each profile selects exactly one
          interface by kernel name or MAC address. An empty list preserves the
          existing DHCP behavior.
        '';
      };

      dnsServers = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        description = "DNS server IP addresses for static rescue networking.";
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
        assertion = stage.staticProfiles == [ ] || stage.interfaces == [ ];
        message = "networkStage.interfaces is DHCP-only and cannot be combined with staticProfiles.";
      }
      {
        assertion = profilesValid;
        message = "Each static profile needs one valid name/MAC selector and at least one address.";
      }
      {
        assertion =
          stage.staticProfiles == [ ]
          || (
            (stage.addressFamily != "ipv4-only" || lib.all (p: p.ipv6.addresses == [ ]) stage.staticProfiles)
            && (stage.addressFamily != "ipv6-only" || lib.all (p: p.ipv4.addresses == [ ]) stage.staticProfiles)
          );
        message = "Static profile addresses must match networkStage.addressFamily.";
      }
      {
        assertion = !(lib.hasPrefix "/" stage.imagePath) && !(lib.hasInfix ".." stage.imagePath);
        message = "networkStage.imagePath must be a safe path relative to /boot.";
      }
    ];
  };
}
