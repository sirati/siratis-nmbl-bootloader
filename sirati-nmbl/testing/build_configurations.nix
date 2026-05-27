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

    # External-config (Option 1) variant: same UEFI+GRUB chain as
    # test-gpt-uefi-grub, but the runtime config.toml lives on the boot
    # partition. The initramfs ships only the bootstrap.toml descriptor
    # emitted by lib/bootstrap-toml.nix; lib/install-bootloader.nix stages
    # the full config.toml to /boot/nmbl/config.toml at install time.
    test-external-config = {
      name = "test-external-config";
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
      extraModules = [
        ({ ... }: {
          boot.nmbl.configLocation = "external";
          # make-disk-image.nix does not set GPT partlabels, so override
          # the default `/dev/disk/by-partlabel/disk-main-ESP` to the raw
          # virtio device path. vda1 is the FAT32 ESP under the hybrid
          # partition layout used by vm-config.nix.
          boot.nmbl.bootstrap.bootFs.device = "/dev/vda1";
        })
      ];
    };

    # External-rescue (Option 2) variant: same UEFI+GRUB chain as
    # test-gpt-uefi-grub, but rescue tools live in nmbl-rescue.sfs on the
    # boot partition rather than embedded in the initramfs. F.4's VM test
    # boots into the rescue shell and runs `strace --version`.
    test-external-rescue = {
      name = "test-external-rescue";
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
      extraModules = [
        ({ pkgs, ... }: {
          boot.nmbl.rescue.mode = "external";
          # strace is the F.4 smoke-test target; busybox provides /bin/sh
          # which the Rust loader requires inside the squashfs tree.
          boot.nmbl.rescue.squashfsContents = [ pkgs.busybox pkgs.strace ];
        })
      ];
    };

    # External-rescue + network variant: extends test-external-rescue
    # with the network-rescue feature so F.5's VM test can verify the
    # HTTP fallback prompt comes up. defaultUrl is left empty so the
    # operator types the URL at the prompt; virtio_net is auto-pulled
    # by lib/modules/nic-modules.nix from the qemu-guest profile.
    test-external-rescue-network = {
      name = "test-external-rescue-network";
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
      extraModules = [
        ({ pkgs, ... }: {
          boot.nmbl.rescue.mode = "external";
          boot.nmbl.rescue.squashfsContents = [ pkgs.busybox pkgs.strace ];
          boot.nmbl.rescue.network = true;
          boot.nmbl.rescue.defaultUrl = "";
        })
      ];
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
        ({ lib, ... }: {
          # disko already wires boot.initrd.luks.devices.cryptroot for the
          # post-kexec NixOS stage-1; we only need to teach NMBL itself to
          # unlock the volume in stage-0 before mounting /. passToStage1
          # tells NMBL to inject the typed passphrase into the kexec'd
          # initrd at a fixed path (memory only) so stage-1 doesn't prompt
          # again — the NixOS keyFile setting below picks it up.
          boot.nmbl.activation.luks = [
            {
              name = "cryptroot";
              device = "/dev/vda3";
              unlock = "password";
              promptLabel = "Enter LUKS passphrase for cryptroot";
              passToStage1 = "/etc/nmbl-luks/cryptroot";
            }
          ];
          # Tell the post-kexec NixOS initrd to read the injected
          # passphrase instead of prompting. fallbackToPassword keeps the
          # operator able to recover if injection ever fails.
          boot.initrd.luks.devices.cryptroot = lib.mkForce {
            device = "/dev/disk/by-partlabel/disk-main-luks";
            keyFile = "/etc/nmbl-luks/cryptroot";
            fallbackToPassword = true;
            allowDiscards = true;
          };
        })
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
