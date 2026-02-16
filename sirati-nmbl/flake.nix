{
  description = "Linux-as-bootloader (NMBL-style) for NixOS";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    {
      self,
      nixpkgs,
    }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};

      # Import testing utilities
      testing = import ./testing/build_configurations.nix { inherit self nixpkgs; };

      # Import test runners
      testRunners = import ./testing/test-runners.nix { inherit nixpkgs system; };

      # Import vm-serial-man package directly
      vmSerialManFlake = import ../vm-serial-man-rs/flake.nix;
      vmSerialMan =
        (vmSerialManFlake.outputs {
          self = vmSerialManFlake;
          inherit nixpkgs;
        }).packages.${system}.default;

      # Build test runner apps for each configuration
      testApps = builtins.listToAttrs (
        builtins.concatLists (
          builtins.attrValues (
            builtins.mapAttrs (
              name: cfg:
              let
                config = testing.mkTestConfigurations.${name};
                directApp = {
                  name = "test-${name}-direct";
                  value = {
                    type = "app";
                    program = "${testRunners.mkDirectKernelRunner {
                      inherit name config vmSerialMan;
                    }}";
                  };
                };
                uefiApp =
                  if cfg.bootMode == "gpt-uefi" then
                    [
                      {
                        name = "test-${name}-uefi";
                        value = {
                          type = "app";
                          program = "${testRunners.mkUefiRunner {
                            inherit name config vmSerialMan;
                          }}";
                        };
                      }
                    ]
                  else
                    [ ];
              in
              [ directApp ] ++ uefiApp
            ) testing.configs
          )
        )
      );
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
      nixosConfigurations = testing.mkTestConfigurations;

      # Test runner apps
      # Run with: nix run .#test-mbr-serial-direct
      # Run with: nix run .#test-mbr-serial-direct -- --debug-shell  (drops to emergency shell)
      # Run with: nix run .#test-gpt-uefi-direct
      # Run with: nix run .#test-gpt-uefi-uefi
      apps.${system} = testApps;
    };
}
