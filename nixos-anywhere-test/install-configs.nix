{
  nixpkgs,
  disko,
  siratiNmbl,
  system ? "x86_64-linux",
}:

let
  pkgs = nixpkgs.legacyPackages.${system};

  mkInstall =
    {
      hostName,
      bootstrapper,
      diskoModule ? ./disko-single.nix,
      extraInitrdKernelModules ? [ ],
      # Allow per-config override of the NMBL bootloader kernel.
      # RAID1 configs need a kernel >= 6.12 because the md v1.2 superblocks
      # written by modern kernels (6.18+) are not accepted by linux_6_6.
      nmblKernelPackage ? pkgs.linux_6_6,
    }:
    nixpkgs.lib.nixosSystem {
      inherit system;
      modules = [
        disko.nixosModules.disko
        diskoModule
        siratiNmbl.nixosModules.default
        "${nixpkgs}/nixos/modules/profiles/qemu-guest.nix"
        (
          { lib, ... }:
          {
            boot.nmbl = {
              enable = true;
              inherit bootstrapper;
              kernelPackage = nmblKernelPackage;
              kernelModules = [ ];
              mountPrefix = "/mnt";
              kernelParams = [
                "console=ttyS0,115200"
                "earlyprintk=serial,ttyS0,115200"
                "dyndbg=file super1.c +p"
              ];
              timeoutSeconds = 5;
              serialConsole = "ttyS0,115200";
            };

            boot.kernelParams = [
              "console=ttyS0,115200"
              "earlyprintk=serial,ttyS0,115200"
              "loglevel=7"
            ];

            boot.initrd.availableKernelModules = [ "crc32c" ] ++ extraInitrdKernelModules;
            boot.initrd.kernelModules = [
              "virtio_pci"
              "virtio_blk"
            ] ++ extraInitrdKernelModules;

            # NixOS scripted stage-1 (which this config uses post-kexec) has
            # no built-in `mdadm --assemble` call: the swraid module only
            # ships mdadm into systemd-stage-1 initrd (via initrdBin), and
            # scripted-stage-1 relies on mdadm-from-udev-rules that aren't
            # wired up unless `boot.initrd.systemd.enable = false` AND
            # extraUdevRulesCommands runs — neither happens in our path. So
            # the post-kexec NixOS kernel sees /dev/vda3+/dev/vdb3 with md
            # superblocks but no userspace mdadm to call `--assemble`.
            #
            # Workaround: explicitly copy mdadm into scripted-stage-1 PATH
            # and call assemble in preLVMCommands. Mirrors what NMBL's own
            # mount-and-kernel.sh.nix already does in NMBL stage-0.
            boot.swraid.enable = lib.mkDefault (
              builtins.any (m: m == "raid1" || m == "raid0" || m == "raid10" || m == "raid456")
                extraInitrdKernelModules
            );

            boot.initrd.extraUtilsCommands = lib.mkIf (
              builtins.any (m: m == "raid1" || m == "raid0" || m == "raid10" || m == "raid456")
                extraInitrdKernelModules
            ) (lib.mkAfter ''
              copy_bin_and_libs ${pkgs.mdadm}/sbin/mdadm
            '');

            boot.initrd.preLVMCommands = lib.mkIf (
              builtins.any (m: m == "raid1" || m == "raid0" || m == "raid10" || m == "raid456")
                extraInitrdKernelModules
            ) (lib.mkBefore ''
              echo "NMBL-test: scripted-stage-1 calling mdadm --assemble --scan"
              mdadm --assemble --scan || true
            '');

            # The installed NixOS's kernel must also be linuxPackages_latest
            # for mdraid configs, otherwise the 6.18 kernel's super_1_load
            # pad3-zero check rejects superblocks written by mdadm 4.4 with
            # EINVAL ("Invalid argument") — mdadm 4.4 writes
            # logical_block_size into pad3 as forward-compat for 6.19's
            # introspection. This rejection happens even with the explicit
            # `mdadm --assemble --scan` running in preLVMCommands above:
            # the kernel module returns EINVAL when the array is RUN_ARRAY'd.
            # NMBL's own initrd already uses linuxPackages_latest.kernel for
            # the same reason. Aligning both ends avoids the version-skew
            # bug entirely until mdadm 4.5+ (the EOL notice in dmesg) lands
            # in nixpkgs.
            boot.kernelPackages = lib.mkIf (
              builtins.any (m: m == "raid1" || m == "raid0" || m == "raid10" || m == "raid456")
                extraInitrdKernelModules
            ) pkgs.linuxPackages_latest;

            boot.loader.grub.enable = false;
            boot.loader.systemd-boot.enable = false;

            networking.hostName = hostName;
            networking.useDHCP = true;
            networking.firewall.allowedTCPPorts = [ 22 ];

            services.openssh = {
              enable = true;
              settings = {
                PermitRootLogin = "prohibit-password";
                PasswordAuthentication = false;
              };
            };

            users.users.root.openssh.authorizedKeys.keys = [
              # Empty by default; the orchestrator drops
              # /root/.ssh/authorized_keys directly via nixos-anywhere --extra-files
            ];

            services.getty.autologinUser = "root";

            environment.systemPackages = with pkgs; [
              vim
              htop
              kexec-tools
            ];

            system.stateVersion = "24.05";
          }
        )
      ];
    };
in
{
  install-gpt-bios = mkInstall {
    hostName = "install-gpt-bios";
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

  install-gpt-uefi-grub = mkInstall {
    hostName = "install-gpt-uefi-grub";
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

  install-gpt-uefi-systemd = mkInstall {
    hostName = "install-gpt-uefi-systemd";
    bootstrapper = {
      partition_table = "gpt";
      bootMode = "uefi";
      loader = "systemd";
      loader_extra_args = {
        timeout = 0;
      };
    };
  };

  # RAID1 variants: same bootstrappers, two-disk mdraid layout. The extra
  # initrd modules are required for NMBL to assemble the array before mount.
  install-gpt-bios-raid1 = mkInstall {
    hostName = "install-gpt-bios-raid1";
    diskoModule = ./disko-raid1.nix;
    extraInitrdKernelModules = [
      "md_mod"
      "raid1"
    ];
    # Use linux_6_18: md superblocks written by the rescue VM's 6.18 kernel must be
    # read by a kernel of the same major.minor to avoid superblock format incompatibilities.
    # Kernels 6.6 and 6.12 both reject the v1.x superblocks with EINVAL in md_import_device.
    # linuxPackages_latest.kernel (currently 7.x) — required so that the
    # md_mod in NMBL's initrd matches what the rescue installer's mdadm
    # wrote into the pad3 region. 6.18's pad3-zero check rejects superblocks
    # whose pad3 carries the logical_block_size field introduced in 6.19.
    nmblKernelPackage = pkgs.linuxPackages_latest.kernel;
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

  install-gpt-uefi-grub-raid1 = mkInstall {
    hostName = "install-gpt-uefi-grub-raid1";
    diskoModule = ./disko-raid1.nix;
    extraInitrdKernelModules = [
      "md_mod"
      "raid1"
    ];
    # linuxPackages_latest.kernel (currently 7.x) — required so that the
    # md_mod in NMBL's initrd matches what the rescue installer's mdadm
    # wrote into the pad3 region. 6.18's pad3-zero check rejects superblocks
    # whose pad3 carries the logical_block_size field introduced in 6.19.
    nmblKernelPackage = pkgs.linuxPackages_latest.kernel;
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

  install-gpt-uefi-systemd-raid1 = mkInstall {
    hostName = "install-gpt-uefi-systemd-raid1";
    diskoModule = ./disko-raid1.nix;
    extraInitrdKernelModules = [
      "md_mod"
      "raid1"
    ];
    # linuxPackages_latest.kernel (currently 7.x) — required so that the
    # md_mod in NMBL's initrd matches what the rescue installer's mdadm
    # wrote into the pad3 region. 6.18's pad3-zero check rejects superblocks
    # whose pad3 carries the logical_block_size field introduced in 6.19.
    nmblKernelPackage = pkgs.linuxPackages_latest.kernel;
    bootstrapper = {
      partition_table = "gpt";
      bootMode = "uefi";
      loader = "systemd";
      loader_extra_args = {
        timeout = 0;
      };
    };
  };

  # btrfs RAID1 variants: bypass the mdadm/kernel-pad3 issue by mirroring at
  # the filesystem layer (btrfs has its own metadata, no mdadm superblock).
  # /boot stays vfat on vda's ESP (firmware can't read btrfs).
  install-gpt-bios-btrfs-raid1 = mkInstall {
    hostName = "install-gpt-bios-btrfs-raid1";
    diskoModule = ./disko-btrfs-raid1.nix;
    extraInitrdKernelModules = [ "btrfs" ];
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

  install-gpt-uefi-grub-btrfs-raid1 = mkInstall {
    hostName = "install-gpt-uefi-grub-btrfs-raid1";
    diskoModule = ./disko-btrfs-raid1.nix;
    extraInitrdKernelModules = [ "btrfs" ];
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

  install-gpt-uefi-systemd-btrfs-raid1 = mkInstall {
    hostName = "install-gpt-uefi-systemd-btrfs-raid1";
    diskoModule = ./disko-btrfs-raid1.nix;
    extraInitrdKernelModules = [ "btrfs" ];
    bootstrapper = {
      partition_table = "gpt";
      bootMode = "uefi";
      loader = "systemd";
      loader_extra_args = {
        timeout = 0;
      };
    };
  };
}
