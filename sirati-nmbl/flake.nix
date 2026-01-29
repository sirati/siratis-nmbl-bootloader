{
  description = "Linux-as-bootloader (NMBL-style) for NixOS";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    {
      # The main NixOS module
      nixosModules.default =
        {
          config,
          lib,
          pkgs,
          ...
        }:
        {
          imports = [
            ./lib/options.nix
            ./lib/config.nix
          ];
        };

      # Example configuration for testing
      nixosConfigurations.example = nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        modules = [
          self.nixosModules.default
          {
            boot.nmbl = {
              enable = true;
              bootMode = "gpt-uefi";
              kernelPackage = nixpkgs.legacyPackages.x86_64-linux.linux_6_6;

              kernelModules = [
                "ext4"
                "virtio_blk"
                "virtio_pci"
                "ahci"
                "sd_mod"
              ];

              fileSystems = {
                "/mnt-root" = {
                  device = "/dev/sda1";
                  fsType = "ext4";
                  options = [ "ro" ];
                };
              };

              kernelParams = [
                "console=ttyS0,115200"
                "quiet"
              ];

              timeoutSeconds = 3;
              serialConsole = "ttyS0,115200";
            };

            system.stateVersion = "24.05";
          }
        ];
      };
    };
}
