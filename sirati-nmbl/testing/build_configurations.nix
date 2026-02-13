# Testing configurations for NMBL bootloader
# Combines VM configurations and test runners

{ self, nixpkgs }:

let
  system = "x86_64-linux";

  vmConfig = import ./vm-config.nix { inherit self nixpkgs system; };

  # Define test configurations
  configs = {
    test-mbr-serial = {
      name = "test-mbr-serial";
      bootMode = "mbr";
    };

    test-gpt-bios = {
      name = "test-gpt-bios";
      bootMode = "gpt-bios";
    };

    test-gpt-uefi = {
      name = "test-gpt-uefi";
      bootMode = "gpt-uefi";
    };
  };

  # Build VM configurations
  mkTestConfigurations = builtins.mapAttrs
    (name: cfg: vmConfig.mkTestVM cfg)
    configs;

in
{
  inherit mkTestConfigurations;
  inherit configs;
}
