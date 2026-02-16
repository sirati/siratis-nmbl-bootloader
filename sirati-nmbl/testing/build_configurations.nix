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

    test-gpt-uefi-grub = {
      name = "test-gpt-uefi-grub";
      bootMode = "gpt-uefi";
      uefiBootloader = "grub";
    };

    test-gpt-uefi-systemd = {
      name = "test-gpt-uefi-systemd";
      bootMode = "gpt-uefi";
      uefiBootloader = "systemd-boot";
    };

    test-gpt-uefi-efi = {
      name = "test-gpt-uefi-efi";
      bootMode = "gpt-uefi";
      uefiBootloader = "efi-stub";
    };
  };

  # Build VM configurations
  mkTestConfigurations = builtins.mapAttrs (name: cfg: vmConfig.mkTestVM cfg) configs;

in
{
  inherit mkTestConfigurations;
  inherit configs;
}
