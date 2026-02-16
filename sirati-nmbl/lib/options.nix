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

    uefiBootloader = lib.mkOption {
      type = lib.types.enum [
        "grub"
        "systemd-boot"
        "efi-stub"
      ];
      default = "grub";
      description = lib.mdDoc ''
        UEFI bootloader type (only used when bootMode is "gpt-uefi").
        - grub: GRUB bootloader for UEFI
        - systemd-boot: systemd-boot (formerly gummiboot)
        - efi-stub: Direct EFI stub - kernel is invoked directly by UEFI firmware
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
