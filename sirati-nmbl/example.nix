# Example NixOS Configuration using NMBL
# This demonstrates how to use the NixOS Minimal BootLoader module

{ config, pkgs, ... }:

{
  imports = [
    # Import the NMBL module
    ./lib/options.nix
    ./lib/config.nix
  ];

  # Enable and configure NMBL
  boot.nmbl = {
    enable = true;

    # Boot mode: "mbr", "gpt-bios", or "gpt-uefi"
    bootMode = "gpt-uefi";

    # Pin a specific kernel version for the bootloader
    # This should be a stable, well-tested kernel
    kernelPackage = pkgs.linux_6_6;

    # Kernel modules needed to mount your root filesystem
    # These are NOT inherited from your system configuration
    kernelModules = [
      # Filesystem support
      "ext4"
      "btrfs"
      "xfs"

      # Storage controller drivers
      "ahci" # SATA controllers
      "sd_mod" # SCSI disk support
      "nvme" # NVMe drives

      # Virtual machine drivers (if applicable)
      "virtio_blk"
      "virtio_pci"
      "virtio_scsi"
    ];

    # Filesystems to mount in the bootloader
    # The bootloader needs to mount your root filesystem to find NixOS generations
    fileSystems = {
      "/mnt-root" = {
        device = "/dev/sda1"; # Change this to your root partition
        fsType = "ext4"; # Change this to your filesystem type
        options = [ "ro" ]; # Read-only is recommended for safety
      };

      # If you have a separate boot partition, you may need to mount it too
      # "/mnt-root/boot" = {
      #   device = "/dev/sda2";
      #   fsType = "vfat";
      #   options = [ "ro" ];
      # };
    };

    # Kernel parameters for the NMBL bootloader kernel
    # These are used when booting the bootloader itself, not the target system
    kernelParams = [
      "console=ttyS0,115200" # Serial console (useful for headless systems)
      "console=tty1" # VGA console
      "quiet" # Reduce boot messages
      "loglevel=3" # Kernel log level
    ];

    # Auto-boot timeout in seconds
    # Set to 0 to require manual selection
    timeoutSeconds = 3;

    # Serial console for the bootloader UI
    # Useful for headless servers or virtual machines
    serialConsole = "ttyS0,115200";
  };

  # Rest of your NixOS configuration
  system.stateVersion = "24.05";

  # Example: Basic system configuration
  networking.hostName = "nixos-nmbl";

  # You can use your normal system kernel here
  # The bootloader kernel is separate
  boot.kernelPackages = pkgs.linuxPackages_latest;

  # Your regular filesystem configuration
  fileSystems."/" = {
    device = "/dev/sda1";
    fsType = "ext4";
  };

  # Other system configuration...
}
