# NixOS Module Options for NMBL (NixOS Minimal BootLoader)
# This file defines all configuration options available for the bootloader

{ lib, pkgs, ... }:

{
  imports = [
    ./modules/activation.nix
  ];

  options.boot.nmbl = {
    enable = lib.mkEnableOption "Linux-as-bootloader (NMBL)";

    bootstrapper = lib.mkOption {
      type = lib.types.submodule {
        options = {
          partition_table = lib.mkOption {
            type = lib.types.enum [ "gpt" ];
            default = "gpt";
            description = lib.mdDoc ''
              Partition table type for the bootloader.
              Currently only GPT is supported.
            '';
          };

          bootMode = lib.mkOption {
            type = lib.types.enum [
              "bios"
              "uefi"
              "qemu_kernel_invoke"
            ];
            default = "uefi";
            description = lib.mdDoc ''
              Boot mode for the system:
              - bios: Legacy BIOS boot with GPT partition table (requires BIOS boot partition)
              - uefi: UEFI boot with GPT partition table (requires ESP)
              - qemu_kernel_invoke: Direct kernel invocation by QEMU (bypasses bootloader installation)
            '';
          };

          loader = lib.mkOption {
            type = lib.types.nullOr (
              lib.types.enum [
                "grub"
                "systemd"
              ]
            );
            default = null;
            description = lib.mdDoc ''
              Bootloader to use:
              - grub: GRUB bootloader (supports both BIOS and UEFI)
              - systemd: systemd-boot (UEFI only, formerly gummiboot)
              - null: No loader (used for qemu_kernel_invoke mode)

              Defaults to "grub" for bios/uefi modes, null for qemu_kernel_invoke.
            '';
          };

          loader_extra_args = lib.mkOption {
            type = lib.types.nullOr (
              lib.types.submodule {
                options = {
                  timeout = lib.mkOption {
                    type = lib.types.int;
                    default = 0;
                    description = lib.mdDoc ''
                      Timeout in seconds before auto-selecting the default boot entry.
                      Set to 0 for immediate boot with no menu delay.
                    '';
                  };

                  canTouchEfiVariables = lib.mkOption {
                    type = lib.types.bool;
                    default = false;
                    description = lib.mdDoc ''
                      Whether the installation process is allowed to modify EFI boot variables.
                      Only applies to UEFI boot mode.
                    '';
                  };

                  efiInstallAsRemovable = lib.mkOption {
                    type = lib.types.bool;
                    default = false;
                    description = lib.mdDoc ''
                      Whether to install the bootloader as a removable device.
                      This installs to the fallback path (EFI/BOOT/BOOTX64.EFI) which
                      firmware looks for when no NVRAM entries exist.
                      Only applies to UEFI boot mode with GRUB.
                    '';
                  };

                  default = lib.mkOption {
                    type = lib.types.either lib.types.int lib.types.str;
                    default = "0";
                    apply = toString;
                    description = lib.mdDoc ''
                      Index of the default menu item to be booted.
                      Can also be set to "saved" for GRUB to remember the last selection.
                    '';
                  };

                  configurationLimit = lib.mkOption {
                    type = lib.types.int;
                    default = 100;
                    description = lib.mdDoc ''
                      Maximum number of configurations in boot menu.
                    '';
                  };

                  extraConfig = lib.mkOption {
                    type = lib.types.lines;
                    default = "";
                    example = ''
                      # GRUB example
                      set theme=$prefix/themes/starfield/theme.txt
                    '';
                    description = lib.mdDoc ''
                      Additional bootloader-specific configuration.
                      For GRUB: inserted before menu entries.
                      For systemd-boot: additional loader.conf settings.
                    '';
                  };

                  extraEntries = lib.mkOption {
                    type = lib.types.lines;
                    default = "";
                    example = ''
                      menuentry "Windows" {
                        chainloader (hd0,2)+1
                      }
                    '';
                    description = lib.mdDoc ''
                      Additional boot entries (GRUB-specific).
                    '';
                  };

                  theme = lib.mkOption {
                    type = lib.types.nullOr lib.types.path;
                    default = null;
                    example = lib.literalExpression ''"''${pkgs.kdePackages.breeze-grub}/grub/themes/breeze"'';
                    description = lib.mdDoc ''
                      Path to the bootloader theme (GRUB-specific).
                    '';
                  };
                };
              }
            );
            default = null;
            description = lib.mdDoc ''
              Extra arguments to pass to the bootloader configuration.
              These settings are merged with NMBL's bootloader configuration.
              The timeout is set to 0 by default for immediate boot.

              Set to null for qemu_kernel_invoke mode (no bootloader).
            '';
          };
        };
      };
      default = { };
      description = lib.mdDoc ''
        Bootstrapper configuration for NMBL.
        Defines partition table, boot mode, loader type, and loader-specific settings.
      '';
    };

    kernelPackage = lib.mkOption {
      type = lib.types.package;
      default = pkgs.linux_6_6;
      defaultText = lib.literalExpression "pkgs.linux_6_6";
      description = lib.mdDoc ''
        Kernel package for the bootloader.
        It's recommended to use a pinned, stable kernel version (like linux_6_6)
        for the bootloader to ensure stability and predictability.
        The bootloader will automatically inherit the necessary kernel modules
        from your system's initrd configuration.
      '';
    };

    availableKernelModules = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ "crc32c" ];
      example = [
        "crc32c"
        "ext4"
        "virtio_blk"
      ];
      description = lib.mdDoc ''
        Base kernel modules available in the bootloader initramfs.
        These modules are always included and available for loading.
        The default includes crc32c which is required by ext4.
      '';
    };

    kernelModules = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [
        "nvme"
        "ahci"
        "sd_mod"
      ];
      description = lib.mdDoc ''
        Kernel modules to load explicitly in the bootloader initramfs.
        These are added to boot.initrd.kernelModules from your system configuration.
        The bootloader will also include all modules from boot.initrd.availableKernelModules
        in the initramfs (available but not loaded explicitly).
        Include modules needed for:
        - Your filesystem (ext4, btrfs, xfs, etc.)
        - Your storage controller (ahci, nvme, virtio_blk, etc.)
      '';
    };

    blacklistedKernelModules = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [
        "nouveau"
        "i915"
      ];
      description = lib.mdDoc ''
        List of kernel modules to blacklist in the bootloader initramfs.
        These modules will not be loaded even if requested.
        Useful for preventing problematic drivers from loading during boot.
      '';
    };

    mountPrefix = lib.mkOption {
      type = lib.types.str;
      default = "/mnt";
      example = "/mnt";
      description = lib.mdDoc ''
        Prefix path where filesystems will be mounted in the bootloader environment.
        For example, if set to "/mnt", the root filesystem (/) will be mounted at /mnt,
        /boot will be mounted at /mnt/boot, etc.

        This allows the bootloader to access all system filesystems read-only
        to find available NixOS generations for kexec.
      '';
    };

    kernelParams = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [
        "console=ttyS0,115200"
        "quiet"
      ];
      description = lib.mdDoc ''
        Extra kernel parameters for the NMBL bootloader kernel only.
        These parameters are used when booting the bootloader itself,
        not the target NixOS system.
      '';
    };

    timeoutSeconds = lib.mkOption {
      type = lib.types.int;
      default = 3;
      description = lib.mdDoc ''
        Timeout in seconds before auto-selecting the default boot entry.
        Set to 0 for no timeout (manual selection required).
        Note: This is for NMBL's own menu, not the underlying bootloader.
      '';
    };

    serialConsole = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "ttyS0,115200";
      description = lib.mdDoc ''
        Serial console configuration for input/output.
        Useful for headless systems or virtual machines.
        Format: device,baudrate (e.g., ttyS0,115200)
      '';
    };

    verbose = lib.mkOption {
      type = lib.types.nullOr lib.types.bool;
      default = null;
      description = lib.mdDoc ''
        Whether to show verbose messages during NMBL boot.
        When null (default), inherits the value from boot.initrd.verbose.
        Set to true for verbose output, false for silent boot (only critical messages will be shown).
      '';
    };

    ignoreMissingDiskModules = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = lib.mdDoc ''
        Whether to skip validation for missing storage driver kernel modules.

        NMBL validates that required storage drivers (like virtio_blk for /dev/vda*,
        nvme for NVMe drives, etc.) are available in boot.initrd.kernelModules or
        boot.initrd.availableKernelModules. This prevents boot failures where devices
        don't appear because drivers weren't loaded.

        Set to true to disable this validation if you know what you're doing or are
        using a custom kernel with built-in drivers.

        Default: false (validation enabled for safety)
      '';
    };
  };
}
