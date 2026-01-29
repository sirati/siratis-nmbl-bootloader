{
  description = "Linux-as-bootloader (NMBL-style) for NixOS";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      # Import testing utilities
      testing = import ./testing/build_configurations.nix { inherit self nixpkgs; };
    in
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

      # Test configurations
      # Build VMs with: nix build .#nixosConfigurations.test-mbr-serial.config.system.build.vm
      # Run VMs with: ./result/bin/run-test-mbr-serial-vm
      nixosConfigurations = testing.mkTestConfigurations;
    };
}
