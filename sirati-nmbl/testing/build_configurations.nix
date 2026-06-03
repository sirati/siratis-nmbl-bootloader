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
          # The default bootstrap kernel module list targets SATA/NVMe.
          # VMs use VirtIO block devices, so replace it with the VirtIO
          # drivers plus the FAT32 stack needed to read the boot partition.
          boot.nmbl.bootstrap.kernelModules.explicit = [
            "virtio_pci"
            "virtio_blk"
            "vfat"
            "nls_cp437"
            "nls_iso8859_1"
          ];
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
          # Static busybox provides /bin/sh without needing glibc in the
          # squashfs chroot. pkgsStatic.strace is the F.4 smoke-test target;
          # both must be statically linked since /nix/store is absent after
          # switch_root into the squashfs.
          boot.nmbl.rescue.squashfsContents = [ pkgs.busybox-sandbox-shell pkgs.pkgsStatic.strace ];
          # External rescue requires bootstrap mode so that Phase 0.5
          # mounts /boot and sets runtime_boot_mountpoint, which
          # rescue::locate_sfs needs to find nmbl-rescue.sfs.
          boot.nmbl.configLocation = "external";
          boot.nmbl.bootstrap.bootFs.device = "/dev/vda1";
          boot.nmbl.bootstrap.kernelModules.explicit = [
            "virtio_pci"
            "virtio_blk"
            "vfat"
            "nls_cp437"
            "nls_iso8859_1"
          ];
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
          boot.nmbl.rescue.squashfsContents = [ pkgs.busybox-sandbox-shell pkgs.pkgsStatic.strace ];
          boot.nmbl.rescue.network = true;
          boot.nmbl.rescue.defaultUrl = "";
          # External rescue requires bootstrap mode so that Phase 0.5
          # mounts /boot and sets runtime_boot_mountpoint.
          boot.nmbl.configLocation = "external";
          boot.nmbl.bootstrap.bootFs.device = "/dev/vda1";
          boot.nmbl.bootstrap.kernelModules.explicit = [
            "virtio_pci"
            "virtio_blk"
            "vfat"
            "nls_cp437"
            "nls_iso8859_1"
          ];
        })
      ];
    };
    # Stateful boot-tracking variant: same UEFI+GRUB chain as
    # test-external-config, but with stateful boot tracking enabled.
    # NMBL records boot attempts to /boot/nmbl/state.bin and recovers to
    # a known-good generation after consecutive failures.
    test-stateful = {
      name = "test-stateful";
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
          boot.nmbl.stateful.enable = true;
          # Default maxRecoveryAttempts = 5, successTarget = "multi-user.target"
          boot.nmbl.bootstrap.bootFs.device = "/dev/vda1";
          boot.nmbl.bootstrap.kernelModules.explicit = [
            "virtio_pci"
            "virtio_blk"
            "vfat"
            "nls_cp437"
            "nls_iso8859_1"
          ];
        })
      ];
    };

    # Splash background sidecar variant: same UEFI+GRUB chain as
    # test-external-config, but the graphical splash is enabled with the
    # background staged on the boot partition (`nmblsplash.png` next to
    # the initrd) instead of embedded in the initramfs. Requires
    # bootstrap mode so Phase 0.5 mounts /boot before the splash comes
    # up. Used to verify the image lands on /boot and is NOT in the
    # initrd.
    test-external-splash-bg = {
      name = "test-external-splash-bg";
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
          boot.nmbl.splash.enable = true;
          boot.nmbl.splash.backgroundLocation = "boot-partition";
          # Sidecar background requires bootstrap mode so Phase 0.5
          # mounts /boot and sets runtime_boot_mountpoint, which the
          # splash loader needs to find nmblsplash.png.
          boot.nmbl.configLocation = "external";
          boot.nmbl.bootstrap.bootFs.device = "/dev/vda1";
          boot.nmbl.bootstrap.kernelModules.explicit = [
            "virtio_pci"
            "virtio_blk"
            "vfat"
            "nls_cp437"
            "nls_iso8859_1"
          ];
        })
      ];
    };

    # Secure-boot GOOD variant: enables the signing + secure-boot tables so
    # the `secure-boot` Cargo feature is compiled into /init and the SB boot
    # path (priority gate, TPM transport, driver-image loop-mount) is routed
    # through the dry-run. The baked trust anchor is the committed
    # insecure-test ML-DSA-87 public key (testing/keys/) — signatures
    # themselves are produced at INSTALL time, so the BUILD-time dry-run only
    # validates the initramfs SHAPE the SB path needs (the SB-required kernel
    # modules — tpm_crb/tpm_tis early, loop/squashfs for the driver-image
    # loop-mount — must SHIP in /lib/modules). Its `nmblInitrmCheck` PASSES.
    test-secure-boot-good = {
      name = "test-secure-boot-good";
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
      # The SB path force-loads tpm_crb/tpm_tis early and loop-mounts a signed
      # driver image, so 6.6's dm-crypt/crypto-init quirks are best avoided —
      # use the latest kernel as the LUKS configs do.
      nmblKernelPackage = (nixpkgs.legacyPackages.x86_64-linux).linuxPackages_latest.kernel;
      extraModules = [
        ({ ... }: {
          # Bootstrap/external mode so Phase 0.5 records runtime_boot_mountpoint
          # (driver images + priority file are boot-partition material).
          boot.nmbl.configLocation = "external";
          boot.nmbl.bootstrap.bootFs.device = "/dev/vda1";
          boot.nmbl.bootstrap.kernelModules.explicit = [
            "virtio_pci"
            "virtio_blk"
            "vfat"
            "nls_cp437"
            "nls_iso8859_1"
          ];
          # Signature verification, fail-closed, against the committed
          # insecure-test ML-DSA-87 trust anchor.
          boot.nmbl.signing.enable = true;
          boot.nmbl.signing.enforce = true;
          boot.nmbl.signing.algorithm = "ml-dsa-87";
          boot.nmbl.signing.publicKeys = [ ./keys/insecure-test-ml-dsa-87.pub ];
          # The driver-image private key is read IMPURELY at install time; a
          # STRING path keeps it out of the store. It is never read at build.
          boot.nmbl.signing.imageKeyFile = "/run/secrets/nmbl-insecure-test.key";
          # Secure-boot priority gate, fail-closed.
          boot.nmbl.secureBoot.enable = true;
          boot.nmbl.secureBoot.enforce = true;
          # A signed driver image whose squashfs the SB path loop-mounts +
          # finit_module's. `loop`+`squashfs` must therefore ship in the
          # initramfs for the loop-mount to work (the GOOD config gets them
          # via boot.nmbl.kernelModules below); the squashfs itself is staged
          # on the boot partition at install time.
          boot.nmbl.driverImages.enable = true;
          boot.nmbl.driverImages.images.testdrv = {
            # An in-tree module rebuilt out-of-tree into the image is overkill
            # for a build-shape proof; a firmware-only (no-module) image is a
            # valid signed image and keeps the squashfs build hermetic.
            modules = [ ];
          };
          # SB-required initramfs modules: the driver-image loop-mount needs
          # loop+squashfs in NMBL's OWN initramfs (the boot-partition squashfs
          # is mounted by NMBL's kernel before switch_root). tpm_crb/tpm_tis
          # are added to earlyKernelModules automatically by the tpm module
          # when secureBoot.enable is set.
          boot.nmbl.kernelModules = [ "loop" "squashfs" ];
        })
      ];
    };

    # Secure-boot BROKEN variant: identical SB posture to test-secure-boot-good,
    # but the initramfs is DELIBERATELY incomplete — the `squashfs` module the
    # driver-image loop-mount needs is REQUESTED at boot (it stays in the
    # config.toml explicit-load list via the explicitKernelModules override
    # below) yet its `.ko` is NOT staged into /lib/modules (it is dropped from
    # boot.nmbl.kernelModules, so kernel-modules.nix's closure omits it). At
    # runtime NMBL's phase 2b would try to modprobe squashfs and fail, so the
    # signed driver image could never be loop-mounted — a real broken
    # secure-boot initramfs. The build-time `--validate-initrm` gate
    # (nmblInitrmCheck) MUST catch this: its phase-2b module-load dry-run
    # presence-checks every requested module against the extracted initrd and
    # reports squashfs as a missing file, so this config's nmblInitrmCheck
    # FAILS the build. This proves the gate's SB-path coverage is load-bearing.
    test-secure-boot-broken = {
      name = "test-secure-boot-broken";
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
      nmblKernelPackage = (nixpkgs.legacyPackages.x86_64-linux).linuxPackages_latest.kernel;
      extraModules = [
        ({ lib, ... }: {
          boot.nmbl.configLocation = "external";
          boot.nmbl.bootstrap.bootFs.device = "/dev/vda1";
          boot.nmbl.bootstrap.kernelModules.explicit = [
            "virtio_pci"
            "virtio_blk"
            "vfat"
            "nls_cp437"
            "nls_iso8859_1"
          ];
          boot.nmbl.signing.enable = true;
          boot.nmbl.signing.enforce = true;
          boot.nmbl.signing.algorithm = "ml-dsa-87";
          boot.nmbl.signing.publicKeys = [ ./keys/insecure-test-ml-dsa-87.pub ];
          boot.nmbl.signing.imageKeyFile = "/run/secrets/nmbl-insecure-test.key";
          boot.nmbl.secureBoot.enable = true;
          boot.nmbl.secureBoot.enforce = true;
          boot.nmbl.driverImages.enable = true;
          boot.nmbl.driverImages.images.testdrv = {
            modules = [ ];
          };
          # THE BREAKAGE. `loop` is staged normally (kept in kernelModules so it
          # is both requested AND in the initramfs closure), but `squashfs` is
          # dropped from the staged closure (NOT in kernelModules) while it is
          # still force-REQUESTED in the runtime explicit-load list. The two
          # lists are computed independently — config.toml `explicit` reads the
          # `explicitKernelModules` OPTION, the closure reads kernel-modules.nix
          # from `kernelModules` — so this override makes squashfs requested but
          # un-staged, exactly the missing-module bug the gate must catch.
          boot.nmbl.kernelModules = [ "loop" ];
          boot.nmbl.explicitKernelModules = lib.mkForce [ "loop" "squashfs" ];
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
