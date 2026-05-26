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
    nmblKernelPackage = pkgs.linux_6_18;
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
    nmblKernelPackage = pkgs.linux_6_18;
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
    nmblKernelPackage = pkgs.linux_6_18;
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
