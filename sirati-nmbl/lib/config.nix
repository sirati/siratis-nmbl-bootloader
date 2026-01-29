# NixOS Module Config Implementation for NMBL
# This file contains the actual implementation of the bootloader module

{
  config,
  lib,
  pkgs,
  utils,
  ...
}:

let
  cfg = config.boot.nmbl;

  # Get filesystems needed for boot (same logic as stage-1)
  fileSystems = builtins.filter utils.fsNeededForBoot (builtins.attrValues config.fileSystems);

  # Automatically inherit kernel modules from system's initrd configuration
  # These are the modules the system already knows it needs for boot
  autoKernelModules = config.boot.initrd.availableKernelModules ++ config.boot.initrd.kernelModules;

  # Combine system modules with user-specified modules for the bootloader
  allKernelModules = lib.unique (autoKernelModules ++ cfg.kernelModules);

  # Create modules tree for our bootloader kernel (same as system.modulesTree in kernel.nix)
  # This gets the "modules" output from the kernel package
  bootloaderModulesTree = pkgs.aggregateModules [
    (lib.getOutput "modules" cfg.kernelPackage)
  ];

  # Create modules closure with kernel modules for our bootloader kernel
  # Note: If modules aren't found, they may be built-in to the kernel
  modulesClosure = pkgs.makeModulesClosure {
    rootModules = allKernelModules;
    kernel = bootloaderModulesTree;
    # Inherit firmware from system configuration (same as stage-1)
    firmware = config.hardware.firmware;
    allowMissing = true;
  };

  # Build the complete init script by importing script.nix
  buildInitScript = import ../scripts/script.nix {
    inherit
      lib
      pkgs
      cfg
      fileSystems
      utils
      ;
    kernelModules = allKernelModules;
  };

in
{
  config = lib.mkIf cfg.enable {
    # Build the minimal initramfs
    system.build.nmblInitramfs =
      let
        kernel = cfg.kernelPackage;
        initScript = buildInitScript;

        # Build minimal initramfs with only essential tools
        initramfs = pkgs.makeInitrd {
          contents = [
            {
              object = initScript;
              symlink = "/init";
            }
            {
              object = pkgs.busybox;
              symlink = "/bin/busybox";
            }
            {
              object = pkgs.bash;
              symlink = "/bin/bash";
            }
            {
              object = pkgs.kexec-tools;
              symlink = "/bin/kexec";
            }
            {
              object = pkgs.kmod;
              symlink = "/bin/kmod";
            }
            {
              object = "${modulesClosure}/lib/modules";
              symlink = "/lib";
            }
          ];

          compressor = "gzip -9";
        };
      in
      initramfs;

    # Build the bootloader kernel
    system.build.nmblKernel = cfg.kernelPackage;

    # Generate bootloader configuration based on boot mode
    system.build.nmblBootConfig =
      let
        kernel = config.system.build.nmblKernel;
        initrd = config.system.build.nmblInitramfs;
        kernelParams = lib.concatStringsSep " " (
          cfg.kernelParams ++ lib.optional (cfg.serialConsole != null) "console=${cfg.serialConsole}"
        );
      in
      pkgs.writeText "nmbl-boot-config" ''
        Boot Mode: ${cfg.bootMode}
        Kernel: ${kernel}/bzImage
        Initrd: ${initrd}/initrd
        Kernel Parameters: ${kernelParams}
      '';

    # Boot loader installation - disable standard bootloaders
    boot.loader.grub.enable = lib.mkDefault false;
    boot.loader.systemd-boot.enable = lib.mkDefault false;

    # Register NMBL as the active bootloader (required by NixOS)
    system.boot.loader.id = "nmbl";

    # NMBL supports initrd secrets since it has an initramfs
    boot.loader.supportsInitrdSecrets = true;

    # Hook for NixOS to install NMBL bootloader during VM builds and system installations
    system.build.installBootLoader = import ./install-bootloader.nix {
      inherit
        lib
        pkgs
        config
        cfg
        ;
    };

    # Custom installation script
    system.build.installNmbl = pkgs.writeShellScriptBin "install-nmbl" ''
      set -e

      DEVICE=$1
      if [ -z "$DEVICE" ]; then
        echo "Usage: install-nmbl <device>"
        echo "Example: install-nmbl /dev/sda"
        exit 1
      fi

      KERNEL="${config.system.build.nmblKernel}/bzImage"
      INITRD="${config.system.build.nmblInitramfs}/initrd"
      KERNEL_PARAMS="${lib.concatStringsSep " " cfg.kernelParams}"

      ${lib.optionalString (cfg.bootMode == "mbr") ''
        echo "Installing MBR bootloader..."
        # Install syslinux for MBR
        ${pkgs.syslinux}/bin/syslinux --install $DEVICE

        # Create syslinux config
        cat > /boot/syslinux/syslinux.cfg << EOF
        DEFAULT linux
        LABEL linux
          KERNEL /nmbl-kernel
          INITRD /nmbl-initrd
          APPEND $KERNEL_PARAMS
        EOF

        cp $KERNEL /boot/nmbl-kernel
        cp $INITRD /boot/nmbl-initrd
      ''}

      ${lib.optionalString (cfg.bootMode == "gpt-bios") ''
        echo "Installing GPT+BIOS bootloader..."
        # Install GRUB for GPT+BIOS
        ${pkgs.grub2}/bin/grub-install --target=i386-pc $DEVICE

        # Create GRUB config
        cat > /boot/grub/grub.cfg << EOF
        set timeout=0
        menuentry "NMBL" {
          linux /nmbl-kernel $KERNEL_PARAMS
          initrd /nmbl-initrd
        }
        EOF

        cp $KERNEL /boot/nmbl-kernel
        cp $INITRD /boot/nmbl-initrd
      ''}

      ${lib.optionalString (cfg.bootMode == "gpt-uefi") ''
        echo "Installing GPT+UEFI bootloader..."
        # Install systemd-boot or GRUB for UESo Im hoping this gets extended in rustFI
        mkdir -p /boot/EFI/BOOT

        ${pkgs.grub2_efi}/bin/grub-install --target=x86_64-efi --efi-directory=/boot --bootloader-id=NMBL

        # Create GRUB config
        cat > /boot/grub/grub.cfg << EOF
        set timeout=0
        menuentry "NMBL" {
          linux /nmbl-kernel $KERNEL_PARAMS
          initrd /nmbl-initrd
        }
        EOFSo Im hoping this gets extended in rust

        cp $KERNEL /boot/nmbl-kernel
        cp $INITRD /boot/nmbl-initrd
      ''}

      echo "NMBL bootloader installed successfully!"
    '';

    # Add kexec-tools to system packages
    environment.systemPackages = [
      pkgs.kexec-tools
      config.system.build.installNmbl
    ];
  };
}
