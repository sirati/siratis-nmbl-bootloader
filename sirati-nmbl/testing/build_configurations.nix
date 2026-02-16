# Testing configurations for NMBL bootloader
# Combines VM configurations and test runners

{ self, nixpkgs }:

let
  system = "x86_64-linux";

  vmConfig = import ./vm-config.nix { inherit self nixpkgs system; };

  # Define test configurations with new bootstrapper structure
  configs = {
    test-gpt-bios = {
      name = "test-gpt-bios";
      bootstrapper = {
        partition_table = "gpt";
        bootMode = "bios";
        loader = "grub";
        loader_extra_args = {
          timeout = 0;
          extraConfig = ''
            serial --unit=0 --speed=115200
            terminal_input serial
            terminal_output serial
          '';
        };
      };
    };

    test-gpt-uefi-grub = {
      name = "test-gpt-uefi-grub";
      bootstrapper = {
        partition_table = "gpt";
        bootMode = "uefi";
        loader = "grub";
        loader_extra_args = {
          timeout = 0;
          extraConfig = ''
            serial --unit=0 --speed=115200
            terminal_input serial
            terminal_output serial
          '';
        };
      };
    };

    test-gpt-uefi-systemd = {
      name = "test-gpt-uefi-systemd";
      bootstrapper = {
        partition_table = "gpt";
        bootMode = "uefi";
        loader = "systemd";
        loader_extra_args = {
          timeout = 0;
        };
      };
    };

    test-gpt-qemu-kernel-invoke = {
      name = "test-gpt-qemu-kernel-invoke";
      bootstrapper = {
        partition_table = "gpt";
        bootMode = "qemu_kernel_invoke";
        # loader and loader_extra_args are null by default for qemu_kernel_invoke
      };
    };
  };

  # Build VM configurations
  mkTestConfigurations = builtins.mapAttrs (name: cfg: vmConfig.mkTestVM cfg) configs;

in
{
  inherit mkTestConfigurations;
  inherit configs;
}
