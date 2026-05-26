{
  description = "Linux-as-bootloader (NMBL-style) for NixOS";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    nmbl-init-rs = {
      url = "path:./nmbl-init-rs";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      nmbl-init-rs,
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

      # The Rust /init binary that replaces the bash script bundle.
      # Built by the sibling flake at ./nmbl-init-rs.
      nmblInit = nmbl-init-rs.packages.${system}.default;

      # Build test runner apps for each configuration
      testApps = builtins.listToAttrs (
        builtins.concatLists (
          builtins.attrValues (
            builtins.mapAttrs (
              name: cfg:
              let
                config = testing.mkTestConfigurations.${name};
                # Only create the main app - bootMode is derived from config.bootstrapper
                app = {
                  name = "${name}";
                  value = {
                    type = "app";
                    program = "${testRunners.mkRunner {
                      inherit name config vmSerialMan;
                      bootMode = null; # Will be derived from config.bootstrapper
                    }}";
                  };
                };
              in
              [ app ]
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
            # ./lib/options.nix already imports ./lib/modules/activation.nix,
            # so listing it here too would be redundant (NixOS dedups by
            # path but keeping a single import site avoids confusion).
            ./lib/options.nix
            ./lib/config.nix
          ];

          # Make the Rust /init binary available to lib/config.nix without
          # forcing every caller to pass it explicitly.
          _module.args.nmblInit = nmblInit;
        };

      # Test configurations
      nixosConfigurations = testing.mkTestConfigurations;

      # Debug info for each test configuration
      # Access with: nix build .#debugInfo.test-gpt-bios
      # Or view with: nix eval .#debugInfo.test-gpt-bios --raw
      debugInfo = builtins.mapAttrs (
        name: config: config.config.system.build.nmblDebugInfo
      ) testing.mkTestConfigurations;

      # Test runner apps
      # Run with: nix run .#test-gpt-bios
      # Run with: nix run .#test-gpt-uefi-grub
      # Run with: nix run .#test-gpt-uefi-systemd
      # Run with: nix run .#test-gpt-qemu-kernel-invoke
      # Run with: nix run .#test-gpt-qemu-kernel-invoke -- --debug-shell  (drops to emergency shell)
      apps.${system} = testApps;
    };
}
