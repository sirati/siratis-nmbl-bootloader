# VM configuration builder for NMBL testing
# Creates NixOS system configurations for different boot modes

{ self, nixpkgs, system ? "x86_64-linux" }:

let
  pkgs = nixpkgs.legacyPackages.${system};

  # Create a test VM configuration
  mkTestVM =
    {
      name,
      bootMode,
      diskLayout ? {
        boot = {
          size = "200M";
          fsType = "vfat";
        };
        root = {
          size = "5G";
          fsType = "ext4";
        };
      },
    }:
    nixpkgs.lib.nixosSystem {
      inherit system;
      modules = [
        self.nixosModules.default
        {
          # Use NMBL bootloader
          boot.nmbl = {
            enable = true;
            inherit bootMode;
            kernelPackage = pkgs.linux_6_6;

            kernelModules = [
              "ext4"
              "vfat"
              "virtio_blk"
              "virtio_pci"
              "virtio_net"
              "ata_piix"
              "ahci"
              "sd_mod"
              "crc32c"
              "crc32c_generic"
              "crc32c_intel"
            ];

            mountPrefix = "/mnt";
            kernelParams = [
              "console=ttyS0,115200"
              "earlyprintk=serial,ttyS0,115200"
            ];
            timeoutSeconds = 5;
            serialConsole = "ttyS0,115200";
          };

          # System configuration
          boot.kernelParams = [
            "console=ttyS0,115200"
            "earlyprintk=serial,ttyS0,115200"
          ];

          boot.loader.grub.enable = false;
          boot.loader.systemd-boot.enable = false;

          fileSystems."/" = {
            device = "/dev/sda1";
            fsType = diskLayout.root.fsType;
          };

          environment.defaultPackages = [ ];
          environment.systemPackages = with pkgs; [
            vim
            htop
          ];

          services.openssh.enable = true;
          services.openssh.settings.PermitRootLogin = "yes";
          users.users.root.password = "test";
          services.getty.autologinUser = "root";

          networking.hostName = name;
          networking.useDHCP = true;

          system.stateVersion = "24.05";
        }
      ];
    };

in
{
  inherit mkTestVM;
}
