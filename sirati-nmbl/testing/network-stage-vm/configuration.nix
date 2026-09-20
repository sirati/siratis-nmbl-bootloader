{
  nixpkgs,
  nmblModule,
  publicKey,
  sshPublicKey,
  system ? "x86_64-linux",
}:

nixpkgs.lib.nixosSystem {
  inherit system;
  modules = [
    nmblModule
    "${nixpkgs}/nixos/modules/profiles/qemu-guest.nix"
    ({ lib, pkgs, ... }: {
      boot.nmbl = {
        enable = true;
        configLocation = "external";
        bootstrapper = {
          partition_table = "gpt";
          bootMode = "qemu_kernel_invoke";
        };
        bootstrap.bootFs = {
          device = "/dev/vda";
          fstype = "ext4";
          options = "ro,nosuid,nodev,noexec";
          mountpoint = "/mnt/boot";
        };
        bootstrap.kernelModules.explicit = [
          "virtio_pci"
          "virtio_blk"
          "ext4"
        ];
        kernelPackage = pkgs.linuxPackages_latest.kernel;
        kernelParams = [ "console=ttyS0,115200" ];
        serialConsole = "ttyS0,115200";
        refuseInvalidHardwareOnInstall = false;

        signing = {
          enable = true;
          enforce = true;
          algorithm = "ml-dsa-65";
          publicKeys = [ publicKey ];
          imageKeyFile = "/run/nmbl-operator/image.key";
          deferInstallSigning = true;
        };

        rescue = {
          mode = "external";
          forceOnBoot = true;
          nicDrivers = [ "dummy" ];
          fullSystem = {
            enable = true;
            sshdPort = 22222;
            rootAuthorizedKeys = [ sshPublicKey ];
            hostKeyPath = "/mnt/boot/rescue-host-ed25519";
            networkStage = {
              enable = true;
              addressFamily = "dual-stack";
              dnsServers = [ "10.0.2.3" "fec0::3" ];
              staticProfiles = [
                {
                  macAddress = "52:54:00:12:34:56";
                  ipv4 = {
                    addresses = [ "10.0.2.15/32" ];
                    gateway = "10.0.2.2";
                    gatewayOnLink = true;
                    routes = [ {
                      destination = "198.51.100.0/24";
                      via = "10.0.2.2";
                      onLink = true;
                    } ];
                  };
                  ipv6 = {
                    addresses = [ "fec0::15/64" ];
                    gateway = "fe80::2";
                    gatewayOnLink = true;
                    routes = [ {
                      destination = "2001:db8:1::/64";
                      via = "fe80::2";
                      onLink = true;
                    } ];
                  };
                }
              ];
            };
          };
        };
      };

      boot.initrd.kernelModules = [
        "virtio_pci"
        "virtio_blk"
        "virtio_net"
      ];
      fileSystems."/" = {
        device = "/dev/vdb";
        fsType = "ext4";
      };
      fileSystems."/boot" = {
        device = "/dev/vda";
        fsType = "ext4";
        options = [ "ro" "nosuid" "nodev" "noexec" ];
      };

      boot.loader.grub.enable = false;
      boot.loader.systemd-boot.enable = false;
      system.stateVersion = "24.05";
    })
  ];
}
