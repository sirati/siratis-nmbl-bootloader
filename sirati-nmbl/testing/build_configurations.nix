# Testing configurations for NMBL bootloader
# Combines VM configurations and test runners

{ self, nixpkgs, disko ? null }:

let
  system = "x86_64-linux";

  vmConfig = import ./vm-config.nix { inherit self nixpkgs disko system; };

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
  } // nixpkgs.lib.optionalAttrs (disko != null) {
    # LUKS-password variant: same UEFI+GRUB chain as test-gpt-uefi-grub,
    # but vda3 is a LUKS container that NMBL unlocks via the TUI passphrase
    # modal before mounting /. The post-kexec NixOS initrd unlocks it a
    # second time (no key handoff yet).
    test-gpt-uefi-grub-luks-password = {
      name = "test-gpt-uefi-grub-luks-password";
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
      # Linux 6.6's dm-crypt module pulls in trusted-keys + encrypted-keys
      # which fail to init when the crypto-API ecb(aes) cipher isn't
      # available at that exact moment. Newer kernels avoid the issue.
      nmblKernelPackage = (nixpkgs.legacyPackages.x86_64-linux).linuxPackages_latest.kernel;
      diskoModule = ./disko-luks-password.nix;
      extraModules = [
        {
          # disko already wires boot.initrd.luks.devices.cryptroot for the
          # post-kexec NixOS stage-1; we only need to teach NMBL itself to
          # unlock the volume in stage-0 before mounting /.
          boot.nmbl.activation.luks = [
            {
              name = "cryptroot";
              device = "/dev/vda3";
              unlock = "password";
              promptLabel = "Enter LUKS passphrase for cryptroot";
            }
          ];
        }
      ];
    };
  };

  # Build VM configurations. `cfg` may carry diskoModule / extraModules
  # which the disko-backed variants use; mkTestVM handles their absence.
  mkTestConfigurations = builtins.mapAttrs (_name: cfg: vmConfig.mkTestVM cfg) configs;

in
{
  inherit mkTestConfigurations;
  inherit configs;
}
