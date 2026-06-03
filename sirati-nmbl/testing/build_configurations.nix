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

    # Secure-boot chain variant (#57 F6b): wires the WHOLE measured/signed
    # chain end-to-end for the VM matrix —
    #   * boot.nmbl.signing.{enable,enforce,publicKeys,algorithm} — ML-DSA-87
    #     verification of every generation against the INSECURE-TEST trust
    #     anchor baked into nmbl-init (testing/keys/insecure-test-ml-dsa-87.pub),
    #     fail-closed (enforce). generationKeyFile points at an IMPURE on-disk
    #     copy of the matching private key so install-time gen-signing
    #     (lib/install-gen-signing.nix) emits /boot/nmbl/sigs/<gen-id>/{kernel,
    #     initrd}.sig the pre-kexec guard verifies.
    #   * boot.nmbl.tpm.{measure,requireTpm,pcrIndex=11} — extend the lock PCR
    #     with the boot handoff; requireTpm so a TPM-less VM FAILS CLOSED (a
    #     negative scenario can never false-green on a box without /dev/tpmrm0).
    #   * boot.nmbl.secureBoot.{enable,enforce,requireTpm} — the secure-boot
    #     posture; priorityVolume.device is left null so the CORE flow needs no
    #     priority mount (the priority-gate scenarios are wired next, see the
    #     matrix manifest).
    #   * a luks-tpm cryptroot device (unlock = "tpm", sealed to PCR 11+7) so
    #     the tpm-roundtrip scenario can seal+unseal the LUKS key.
    #   * loader = "efi-stub" so NMBL boots as a UKI (the install path the
    #     SB-OVMF firmware launches); run under mkRunner { tpm="tis";
    #     secureBoot=true; }.
    #
    # The committed INSECURE-TEST private key is PUBLICLY KNOWN (testing/keys/
    # README.md); it only ever signs TEST artifacts. generationKeyFile resolves
    # impurely (string path, never the store) so the closure-leak assert in
    # lib/install-gen-signing.nix passes: it reads $NMBL_GEN_KEY_FILE when set,
    # else the documented default /run/nmbl-test-keys/insecure-test-gen.key the
    # #57 runner stages the committed key to (see the matrix manifest).
    test-secure-boot = {
      name = "test-secure-boot";
      bootstrapper = {
        partition_table = "gpt";
        bootMode = "uefi";
        loader = "efi-stub";
        loader_extra_args = {
          timeout = 0;
        };
      };
      # LUKS + TPM need a newer kernel for the dm-crypt/tpm stack, matching
      # the luks-tpm target rationale.
      nmblKernelPackage = (nixpkgs.legacyPackages.x86_64-linux).linuxPackages_latest.kernel;
      diskoModule = ./disko-luks-password.nix;
      extraModules = [
        ({ lib, ... }: {
          # ---- signature enforcement (verify every generation) -------------
          boot.nmbl.signing = {
            enable = true;
            enforce = true;
            algorithm = "ml-dsa-87";
            # Baked trust anchor: the INSECURE-TEST public key. Path literal ⇒
            # imported into the store and include_bytes!-baked into nmbl-init.
            publicKeys = [ ./keys/insecure-test-ml-dsa-87.pub ];
            # IMPURE private-key path read at install time (never the store).
            generationKeyFile =
              let
                e = builtins.getEnv "NMBL_GEN_KEY_FILE";
              in
              if e != "" then e else "/run/nmbl-test-keys/insecure-test-gen.key";

            # ---- install-time UKI Secure-Boot signing (F1) ----------------
            # sbsign the NMBL UKI at install with the INSECURE-TEST db cert so
            # the SB-enforcing firmware (whose `db` we enroll that same cert
            # into — see the test-db OVMF VARS in flake.nix) ACCEPTS and
            # launches it. Without this the unsigned UKI is refused by the
            # firmware before NMBL ever runs, and every NMBL scenario times
            # out (audit F1). keyFile/certFile are IMPURE string paths read at
            # install time (the closure-leak assert in lib/install-signing.nix
            # rejects a store path); the #57 runner stages the committed
            # testing/keys/insecure-test-sb-db.{key,crt} there (or exports the
            # NMBL_SB_DB_{KEY,CERT}_FILE envs). The cert is PUBLICLY-KNOWN test
            # material — it only ever signs TEST UKIs.
            uki = {
              enable = true;
              keyFile =
                let
                  e = builtins.getEnv "NMBL_SB_DB_KEY_FILE";
                in
                if e != "" then e else "/run/nmbl-test-keys/insecure-test-sb-db.key";
              certFile =
                let
                  e = builtins.getEnv "NMBL_SB_DB_CERT_FILE";
                in
                if e != "" then e else "/run/nmbl-test-keys/insecure-test-sb-db.crt";
            };
          };

          # ---- measured boot (extend PCR 11, require a real TPM) -----------
          boot.nmbl.tpm = {
            measure = true;
            requireTpm = true;
            pcrIndex = 11;
          };

          # ---- secure-boot posture (no priority volume in the core flow) ---
          boot.nmbl.secureBoot = {
            enable = true;
            enforce = true;
            requireTpm = true;
          };

          # ---- luks-tpm cryptroot (seal the key to PCR 11+7) ---------------
          boot.initrd.kernelModules = [
            "dm_mod"
            "dm-crypt"
            "aesni_intel"
            "tpm"
            "tpm_tis"
            "tpm_crb"
          ];
          boot.nmbl.activation.luks = [
            {
              name = "cryptroot";
              device = "/dev/vda3";
              unlock = "tpm";
              tpmPcrs = [ 11 7 ];
              promptLabel = "Enter LUKS passphrase for cryptroot (TPM fallback)";
              passToStage1 = "/etc/nmbl-luks/cryptroot";
            }
          ];
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
  # Exposed so callers can build a one-off VARIANT of a known config (e.g. the
  # TPM-roundtrip enroll twin: the test-secure-boot config with the cryptroot
  # unlock overridden to a passphrase) without re-deriving the whole builder.
  inherit (vmConfig) mkTestVM;
}
