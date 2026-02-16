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
  bootstrapper = cfg.bootstrapper;

  # Set default loader based on bootMode if not explicitly set
  actualLoader =
    if bootstrapper.loader != null then
      bootstrapper.loader
    else if bootstrapper.bootMode == "qemu_kernel_invoke" then
      null
    else
      "grub"; # Default for bios/uefi

  # Set default loader_extra_args if not explicitly set
  actualLoaderExtraArgs =
    if bootstrapper.loader_extra_args != null then
      bootstrapper.loader_extra_args
    else if bootstrapper.bootMode == "qemu_kernel_invoke" then
      null
    else
      { }; # Default empty set for bios/uefi

  # Get filesystems needed for boot
  # This includes filesystems marked with neededForBoot = true and those in critical paths
  fileSystems = builtins.filter utils.fsNeededForBoot (builtins.attrValues config.fileSystems);

  # All filesystems that NMBL needs to mount
  nmblFileSystems = fileSystems;

  # Automatically inherit kernel modules from system's initrd configuration
  # These are the modules the system already knows it needs for boot
  autoKernelModules = config.boot.initrd.availableKernelModules ++ config.boot.initrd.kernelModules;

  # Combine system modules with user-specified modules for the bootloader
  # availableKernelModules (crc32c by default) + extraKernelModules + auto-detected modules
  allKernelModules = lib.unique (
    cfg.availableKernelModules ++ cfg.extraKernelModules ++ autoKernelModules
  );

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
      utils
      ;
    fileSystems = nmblFileSystems;
    kernelModules = allKernelModules;
  };

  # Determine legacy boot mode string for compatibility
  legacyBootMode =
    if bootstrapper.partition_table == "gpt" && bootstrapper.bootMode == "bios" then
      "gpt-bios"
    else if bootstrapper.partition_table == "gpt" && bootstrapper.bootMode == "uefi" then
      "gpt-uefi"
    else
      "gpt-${bootstrapper.bootMode}";

in
{
  config = lib.mkIf cfg.enable {
    # Mark boot partition as neededForBoot to ensure:
    # 1. Proper kernel modules are included (vfat, nls_cp437, nls_iso8859-1)
    # 2. Boot partition is treated as boot-critical by the system
    # 3. x-initrd.mount option is automatically added
    # Use mkOverride with priority 1000 (higher than default 1500) to ensure boot partition is marked as needed
    # This ensures vfat kernel modules are automatically included in the system initrd
    fileSystems."/boot".neededForBoot = lib.mkOverride 1000 true;

    # Assertions to verify boot configuration
    assertions = [
      {
        assertion = bootstrapper.partition_table == "gpt";
        message = "boot.nmbl.bootstrapper.partition_table must be 'gpt' (only GPT is supported)";
      }
      {
        assertion =
          bootstrapper.bootMode == "bios"
          || bootstrapper.bootMode == "uefi"
          || bootstrapper.bootMode == "qemu_kernel_invoke";
        message = "boot.nmbl.bootstrapper.bootMode must be 'bios', 'uefi', or 'qemu_kernel_invoke'";
      }
      {
        assertion = actualLoader == null || actualLoader == "grub" || actualLoader == "systemd";
        message = "boot.nmbl.bootstrapper.loader must be 'grub', 'systemd', or null";
      }
      {
        assertion = bootstrapper.bootMode == "qemu_kernel_invoke" || actualLoader != null;
        message = "loader must be set for bios/uefi boot modes (should default to 'grub')";
      }
      {
        assertion = bootstrapper.bootMode != "qemu_kernel_invoke" || bootstrapper.loader == null;
        message = "loader must not be set when bootMode is 'qemu_kernel_invoke' (QEMU directly invokes kernel, no bootloader needed)";
      }
      {
        assertion = bootstrapper.bootMode != "qemu_kernel_invoke" || bootstrapper.loader_extra_args == null;
        message = "loader_extra_args must not be set when bootMode is 'qemu_kernel_invoke' (no bootloader is used)";
      }
      {
        assertion = actualLoader != "systemd" || bootstrapper.bootMode == "uefi";
        message = "systemd-boot (loader='systemd') requires UEFI boot mode. Use loader='grub' for BIOS boot.";
      }
      {
        assertion =
          bootstrapper.bootMode == "qemu_kernel_invoke"
          || config.fileSystems ? "/boot"
          || (bootstrapper.bootMode == "uefi" && config.fileSystems ? "/efi");
        message = ''
          NMBL requires a separate boot partition (except for qemu_kernel_invoke mode).
          For UEFI boot, declare fileSystems."/boot" or fileSystems."/efi" with fsType = "vfat".
          For BIOS boot, declare fileSystems."/boot" with fsType = "vfat".

          Example:
            fileSystems."/boot" = {
              device = "/dev/sda1";  # or /dev/vda1 for VirtIO
              fsType = "vfat";
            };
        '';
      }
      {
        assertion =
          let
            bootFS =
              if config.fileSystems ? "/boot" then
                config.fileSystems."/boot"
              else if config.fileSystems ? "/efi" then
                config.fileSystems."/efi"
              else
                null;
          in
          (bootstrapper.bootMode == "qemu_kernel_invoke") || (bootFS != null -> bootFS.fsType == "vfat");
        message = "NMBL boot partition must be FAT32 (fsType = \"vfat\")";
      }
      {
        assertion =
          bootstrapper.bootMode == "qemu_kernel_invoke"
          || actualLoaderExtraArgs == null
          || !actualLoaderExtraArgs.efiInstallAsRemovable
          || !actualLoaderExtraArgs.canTouchEfiVariables;
        message = "Cannot use both efiInstallAsRemovable and canTouchEfiVariables. Choose one.";
      }
    ];

    # Build the minimal initramfs
    system.build.nmblInitramfs =
      let
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
              symlink = "/lib/modules";
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
        Partition Table: ${bootstrapper.partition_table}
        Boot Mode: ${bootstrapper.bootMode}
        Loader: ${if actualLoader == null then "none (qemu_kernel_invoke)" else actualLoader}
        Kernel: ${kernel}/bzImage
        Initrd: ${initrd}/initrd
        Kernel Parameters: ${kernelParams}
        Loader Timeout: ${
          if actualLoaderExtraArgs == null then "N/A" else toString actualLoaderExtraArgs.timeout
        }
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
        bootstrapper
        legacyBootMode
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

      ${lib.optionalString (bootstrapper.bootMode == "bios" && actualLoader == "grub") ''
        echo "Installing GPT+BIOS bootloader..."
        # Install GRUB for GPT+BIOS
        ${pkgs.grub2}/bin/grub-install --target=i386-pc $DEVICE

        # Create GRUB config
        cat > /boot/grub/grub.cfg << EOF
        set timeout=${toString actualLoaderExtraArgs.timeout}
        ${actualLoaderExtraArgs.extraConfig}
        menuentry "NMBL" {
          linux /nmbl-kernel $KERNEL_PARAMS
          initrd /nmbl-initrd
        }
        ${actualLoaderExtraArgs.extraEntries}
        EOF

        cp $KERNEL /boot/nmbl-kernel
        cp $INITRD /boot/nmbl-initrd
      ''}

      ${lib.optionalString (bootstrapper.bootMode == "uefi" && actualLoader == "grub") ''
        echo "Installing GPT+UEFI bootloader with GRUB..."
        mkdir -p /boot/EFI/BOOT /boot/grub

        # Create GRUB config
        cat > /boot/grub/grub.cfg << EOF
        set timeout=${toString actualLoaderExtraArgs.timeout}
        ${actualLoaderExtraArgs.extraConfig}
        menuentry "NMBL" {
          linux /nmbl-kernel $KERNEL_PARAMS
          initrd /nmbl-initrd
        }
        ${actualLoaderExtraArgs.extraEntries}
        EOF

        # Install GRUB EFI
        ${pkgs.grub2_efi}/bin/grub-install --target=x86_64-efi --efi-directory=/boot --bootloader-id=NMBL \
          ${lib.optionalString (!actualLoaderExtraArgs.canTouchEfiVariables) "--no-nvram"} \
          ${lib.optionalString actualLoaderExtraArgs.efiInstallAsRemovable "--removable"}

        # Copy to fallback location if needed
        if [ -f /boot/EFI/NMBL/grubx64.efi ] && [ "${toString actualLoaderExtraArgs.efiInstallAsRemovable}" != "true" ]; then
          cp /boot/EFI/NMBL/grubx64.efi /boot/EFI/BOOT/BOOTX64.EFI
        fi

        cp $KERNEL /boot/nmbl-kernel
        cp $INITRD /boot/nmbl-initrd
      ''}

      ${lib.optionalString (bootstrapper.bootMode == "uefi" && actualLoader == "systemd") ''
        echo "Installing GPT+UEFI bootloader with systemd-boot..."
        mkdir -p /boot/EFI/BOOT /boot/loader/entries

        # Create systemd-boot loader config
        cat > /boot/loader/loader.conf << EOF
        default nmbl.conf
        timeout ${toString actualLoaderExtraArgs.timeout}
        console-mode max
        editor no
        ${actualLoaderExtraArgs.extraConfig}
        EOF

        # Create boot entry
        cat > /boot/loader/entries/nmbl.conf << EOF
        title NMBL Bootloader
        linux /nmbl-kernel
        initrd /nmbl-initrd
        options $KERNEL_PARAMS
        EOF

        # Install systemd-boot
        ${pkgs.systemd}/bin/bootctl install --esp-path=/boot \
          ${lib.optionalString (!actualLoaderExtraArgs.canTouchEfiVariables) "--no-variables"}

        # Copy to fallback location
        if [ -f /boot/EFI/systemd/systemd-bootx64.efi ]; then
          cp /boot/EFI/systemd/systemd-bootx64.efi /boot/EFI/BOOT/BOOTX64.EFI
        fi

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
