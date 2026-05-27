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
      # Additional NixOS modules to merge after the default install layout.
      # Used by the splash-vnc-demo variant to enable boot.nmbl.splash and
      # mkForce-override the default serialConsole.
      extraModules ? [ ],
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
      ] ++ extraModules;
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

  # One-off splash demo variant: same install layout as install-gpt-uefi-grub
  # but with boot.nmbl.splash.enable + serialConsole turned off so the splash
  # path actually renders, and a long menu timeout so an operator looking at
  # the splash in VNC has time to interact.
  splash-vnc-demo = mkInstall {
    hostName = "splash-vnc-demo";
    bootstrapper = {
      partition_table = "gpt";
      bootMode = "uefi";
      loader = "grub";
      loader_extra_args = {
        timeout = 0;
      };
    };
    extraModules = [
      (
        { lib, ... }:
        {
          boot.nmbl.splash.enable = true;
          # Graphics drivers must be loaded BEFORE `open_console` so
          # the splash backend has a DRM card to attach to.
          boot.nmbl.earlyKernelModules = [ "virtio_pci" "virtio_gpu" ];
          # mkForce because mkInstall hard-codes serialConsole = "ttyS0,115200".
          # The splash path is gated by `if config.general.serial_console`, so
          # serial-on would short-circuit straight to line-mode menu and we'd
          # never see the splash.
          boot.nmbl.serialConsole = lib.mkForce null;
          # Bump the menu timeout so the operator has time to look around.
          boot.nmbl.timeoutSeconds = lib.mkForce 600;
          # Mirror kernel + NMBL output to both serial AND the framebuffer
          # so emergency-shell output is visible in VNC, not just on the
          # captured serial log. Last console= wins for stdin/console-read.
          boot.nmbl.kernelParams = lib.mkForce [
            "console=ttyS0,115200"
            "earlyprintk=serial,ttyS0,115200"
            "console=tty1"
            "dyndbg=file super1.c +p"
          ];
          boot.kernelParams = lib.mkForce [
            "console=ttyS0,115200"
            "earlyprintk=serial,ttyS0,115200"
            "console=tty1"
            "loglevel=7"
          ];
        }
      )
    ];
  };

  # Direct-kexec splash harness: NixOS config used purely to produce the
  # NMBL kernel + initrd that we hand straight to qemu's -kernel/-initrd
  # for sub-30-second iteration on the emergency TUI / splash UI. It
  # uses bootMode = "qemu_kernel_invoke" so no /boot or installer payload
  # is required, declares no real filesystems (NMBL will fail to find
  # generations and land on the emergency TUI), and turns the splash on.
  #
  # No disko module is imported and `ignoreMissingDiskModules = true`
  # because the VM is launched with no -drive at all.
  nmbl-direct-splash = nixpkgs.lib.nixosSystem {
    inherit system;
    modules = [
      siratiNmbl.nixosModules.default
      "${nixpkgs}/nixos/modules/profiles/qemu-guest.nix"
      (
        { lib, modulesPath, ... }:
        {
          # NixOS evaluation insists on a root filesystem. Declare a
          # tmpfs root that never gets touched at runtime (NMBL is /init;
          # the kexec into the system never happens because no generations
          # are found).
          #
          # /boot is also declared even though qemu_kernel_invoke bypasses
          # the boot-partition assertion, because lib/config.nix
          # unconditionally sets `fileSystems."/boot".neededForBoot = true`
          # which materialises the entry and then NixOS demands device +
          # fsType. The VM has no /dev/vda so this fs is never mounted
          # at runtime; it only exists to satisfy module evaluation.
          fileSystems."/" = {
            device = "none";
            fsType = "tmpfs";
          };
          fileSystems."/boot" = {
            device = "none";
            fsType = "tmpfs";
            options = [ "nofail" ];
          };

          boot.nmbl = {
            enable = true;
            bootstrapper = {
              partition_table = "gpt";
              bootMode = "qemu_kernel_invoke";
            };
            kernelPackage = pkgs.linuxPackages_latest.kernel;
            # NMBL itself must insmod the virtio_gpu stack in
            # qemu_kernel_invoke mode (no kmod auto-load), otherwise
            # /dev/dri/card0 never materialises and the splash DRM
            # bring-up falls back to "console unavailable". These
            # belong in `earlyKernelModules` so they load in phase 2a,
            # before `open_console` reaches for the DRM card.
            earlyKernelModules = [ "virtio_pci" "virtio_gpu" ];
            kernelModules = [ ];
            mountPrefix = "/mnt";
            kernelParams = [
              "console=ttyS0,115200"
              "earlyprintk=serial,ttyS0,115200"
              "console=tty1"
            ];
            timeoutSeconds = 600;
            # serialConsole stays null so the splash code path is reached
            # (it is gated on serial_console being false).
            serialConsole = null;
            ignoreMissingDiskModules = true;
            splash.enable = true;
          };

          boot.kernelParams = lib.mkForce [
            "console=ttyS0,115200"
            "earlyprintk=serial,ttyS0,115200"
            "console=tty1"
            "loglevel=7"
          ];

          # Minimal kernel-module set: just enough to bring up the
          # framebuffer + DRI for the splash. virtio_blk is harmless
          # filler (the VM has no -drive but matches other tests).
          boot.initrd.availableKernelModules = [ "virtio_gpu" "virtio_pci" "virtio_blk" ];
          boot.initrd.kernelModules = [ ];

          boot.loader.grub.enable = false;
          boot.loader.systemd-boot.enable = false;

          # Disable the entire NixOS toplevel-builder requirement chain
          # we don't need: no users, no services, no networking. The
          # only artifact we consume is system.build.nmblKernel + nmblInitramfs.
          networking.hostName = "nmbl-direct";
          services.openssh.enable = false;

          system.stateVersion = "24.05";
        }
      )
    ];
  };

  # Splash + LUKS demo: same disko layout as the LUKS test (vda3 wrapped
  # in a luks container unlocked with passphrase "test"), but with the
  # splash enabled so the LUKS prompt renders through the graphical UI
  # via /dev/dri/card1 + the cosmic-greeter background.
  splash-luks-vnc-demo = mkInstall {
    hostName = "splash-luks-vnc-demo";
    diskoModule = ./disko-luks-password.nix;
    extraInitrdKernelModules = [
      "dm_mod"
      "dm-crypt"
      "aesni_intel"
    ];
    # Linux 6.6 trips a crypto-API init bug in dm-crypt; use latest.
    nmblKernelPackage = pkgs.linuxPackages_latest.kernel;
    bootstrapper = {
      partition_table = "gpt";
      bootMode = "uefi";
      loader = "grub";
      loader_extra_args = {
        timeout = 0;
      };
    };
    extraModules = [
      (
        { lib, ... }:
        {
          boot.nmbl.splash.enable = true;
          # Graphics drivers must be loaded BEFORE `open_console` so
          # the splash backend has a DRM card to attach to before the
          # LUKS passphrase prompt comes up.
          boot.nmbl.earlyKernelModules = [ "virtio_pci" "virtio_gpu" ];
          boot.nmbl.serialConsole = lib.mkForce null;
          boot.nmbl.timeoutSeconds = lib.mkForce 600;
          boot.nmbl.kernelParams = lib.mkForce [
            "console=ttyS0,115200"
            "earlyprintk=serial,ttyS0,115200"
            "console=tty1"
            "dyndbg=file super1.c +p"
          ];
          boot.kernelParams = lib.mkForce [
            "console=ttyS0,115200"
            "earlyprintk=serial,ttyS0,115200"
            "console=tty1"
            "loglevel=7"
          ];
          # LUKS unlock executed by NMBL stage-0 before mounting /.
          # passToStage1 hand-off isn't on this branch yet, so the
          # post-kexec NixOS stage-1 will re-prompt for the same
          # passphrase. For the demo we only need the splash-side
          # prompt to render.
          boot.nmbl.activation.luks = [
            {
              name = "cryptroot";
              device = "/dev/vda3";
              unlock = "password";
              promptLabel = "Enter LUKS passphrase for cryptroot";
            }
          ];
        }
      )
    ];
  };
}
