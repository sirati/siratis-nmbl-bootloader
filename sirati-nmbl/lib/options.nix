# NixOS Module Options for NMBL (NixOS Minimal BootLoader)
# This file defines all configuration options available for the bootloader

{ lib, pkgs, ... }:

{
  options.boot.nmbl = {
    enable = lib.mkEnableOption "Linux-as-bootloader (NMBL)";

    bootMode = lib.mkOption {
      type = lib.types.enum [
        "mbr"
        "gpt-bios"
        "gpt-uefi"
      ];
      default = "gpt-uefi";
      description = lib.mdDoc ''
        Boot mode configuration for the bootloader.
        - mbr: Master Boot Record (legacy BIOS)
        - gpt-bios: GPT partition table with BIOS boot
        - gpt-uefi: GPT partition table with UEFI boot
      '';
    };

    kernelPackage = lib.mkOption {
      type = lib.types.package;
      default = pkgs.linux_6_6;
      description = lib.mdDoc ''
        Pinned kernel version for the bootloader.
        This kernel is separate from your system kernel and should be
        kept minimal and stable.
      '';
    };

    kernelModules = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [
        "ext4"
        "virtio_blk"
        "ahci"
        "sd_mod"
      ];
      description = lib.mdDoc ''
        Kernel modules to load in the bootloader initramfs.
        These are NOT inherited from your system configuration.
        Include modules needed for:
        - Your filesystem (ext4, btrfs, xfs, etc.)
        - Your storage controller (ahci, nvme, virtio_blk, etc.)
      '';
    };

    fileSystems = lib.mkOption {
      type = lib.types.attrsOf (
        lib.types.submodule {
          options = {
            device = lib.mkOption {
              type = lib.types.str;
              description = lib.mdDoc "Device to mount (e.g., /dev/sda1)";
            };
            fsType = lib.mkOption {
              type = lib.types.str;
              default = "ext4";
              description = lib.mdDoc "Filesystem type (ext4, btrfs, xfs, etc.)";
            };
            options = lib.mkOption {
              type = lib.types.listOf lib.types.str;
              default = [ "ro" ];
              description = lib.mdDoc ''
                Mount options. Default is read-only (ro).
                The bootloader should mount filesystems read-only for safety.
              '';
            };
          };
        }
      );
      default = { };
      example = {
        "/mnt-root" = {
          device = "/dev/sda1";
          fsType = "ext4";
          options = [ "ro" ];
        };
      };
      description = lib.mdDoc ''
        Filesystems to mount in the bootloader environment.
        These should be mounted read-only.
        The main filesystem is typically mounted at /mnt-root.
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
  };
}
