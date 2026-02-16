# NMBL Bootloader Installation Script
# This script is called by system.build.installBootLoader during VM builds and system installations
#
# Boot Partition Requirements:
# - Must be FAT32 (vfat) filesystem
# - Must be marked as neededForBoot=true (done automatically by config.nix)
# - This ensures:
#   * vfat, nls_cp437, nls_iso8859-1 modules are in system initrd
#   * x-initrd.mount option is automatically added
#   * Boot partition is treated as boot-critical by the system
#
# For UEFI systems: boot partition can be /boot or /efi (ESP)
# For BIOS systems: boot partition should be /boot

{
  lib,
  pkgs,
  config,
  cfg,
  bootstrapper,
  legacyBootMode,
}:

let
  # Use the same logic as config.nix to get actual loader values
  actualLoader =
    if bootstrapper.loader != null then
      bootstrapper.loader
    else if bootstrapper.bootMode == "qemu_kernel_invoke" then
      null
    else
      "grub";

  actualLoaderExtraArgs =
    if bootstrapper.loader_extra_args != null then
      bootstrapper.loader_extra_args
    else if bootstrapper.bootMode == "qemu_kernel_invoke" then
      null
    else
      { };
in

pkgs.writeScript "install-nmbl-bootloader" ''
  #!${pkgs.runtimeShell}
  set -e

  echo "Installing NMBL bootloader..."
  echo "  Partition Table: ${bootstrapper.partition_table}"
  echo "  Boot Mode: ${bootstrapper.bootMode}"
  echo "  Loader: ${if actualLoader == null then "none (qemu_kernel_invoke)" else actualLoader}"

  # Verify boot partition is mounted and writable
  if [ ! -d /boot ]; then
    echo "ERROR: /boot directory not found"
    exit 1
  fi

  # Test write access to boot partition
  if ! touch /boot/.nmbl-test-write 2>/dev/null; then
    echo "ERROR: /boot is not writable. Boot partition must be mounted read-write."
    echo "Check that the boot partition is properly mounted."
    exit 1
  fi
  rm -f /boot/.nmbl-test-write

  # Check filesystem type
  BOOT_FS_TYPE=$(stat -f -c %T /boot 2>/dev/null || echo "unknown")
  echo "Boot filesystem type: $BOOT_FS_TYPE"
  if [ "$BOOT_FS_TYPE" != "msdos" ] && [ "$BOOT_FS_TYPE" != "vfat" ]; then
    echo "WARNING: Boot partition filesystem is $BOOT_FS_TYPE, expected vfat/msdos"
  fi

  KERNEL="${config.system.build.nmblKernel}/bzImage"
  INITRD="${config.system.build.nmblInitramfs}/initrd"
  KERNEL_PARAMS="${lib.concatStringsSep " " cfg.kernelParams}"

  # Copy NMBL kernel and initrd to boot partition
  echo "Copying NMBL bootloader files to /boot..."
  mkdir -p /boot
  cp -f "$KERNEL" /boot/nmbl-kernel
  cp -f "$INITRD" /boot/nmbl-initrd
  echo "✓ Bootloader files installed: /boot/nmbl-kernel, /boot/nmbl-initrd"

  ${lib.optionalString (bootstrapper.bootMode == "bios" && actualLoader == "grub") ''
        echo "Configuring GPT+BIOS bootloader with GRUB..."
        mkdir -p /boot/grub

        # Create GRUB config
        cat > /boot/grub/grub.cfg << 'EOF'
    set timeout=${toString actualLoaderExtraArgs.timeout}
    set default=${actualLoaderExtraArgs.default}
    ${actualLoaderExtraArgs.extraConfig}

    menuentry "NMBL Bootloader" {
      linux /nmbl-kernel $KERNEL_PARAMS
      initrd /nmbl-initrd
    }
    ${actualLoaderExtraArgs.extraEntries}
    EOF

        # Install GRUB if device exists
        if [ -b /dev/vda ]; then
          echo "Installing GRUB (GPT+BIOS mode) to /dev/vda..."
          ${pkgs.grub2}/bin/grub-install --target=i386-pc /dev/vda || true
          echo "✓ GRUB bootloader installed"
        fi
  ''}

  ${lib.optionalString (bootstrapper.bootMode == "uefi" && actualLoader == "grub") ''
        echo "Configuring GPT+UEFI bootloader with GRUB..."
        mkdir -p /boot/EFI/BOOT /boot/grub

        # Create GRUB config
        cat > /boot/grub/grub.cfg << 'EOF'
    set timeout=${toString actualLoaderExtraArgs.timeout}
    set default=${actualLoaderExtraArgs.default}
    ${actualLoaderExtraArgs.extraConfig}

    menuentry "NMBL Bootloader" {
      linux /nmbl-kernel $KERNEL_PARAMS
      initrd /nmbl-initrd
    }
    ${actualLoaderExtraArgs.extraEntries}
    EOF

        # Install GRUB EFI if device exists
        if [ -b /dev/vda ]; then
          echo "Installing GRUB (UEFI mode) to /boot ESP..."
          GRUB_INSTALL_ARGS="--target=x86_64-efi --efi-directory=/boot --bootloader-id=NMBL"

          ${lib.optionalString (!actualLoaderExtraArgs.canTouchEfiVariables) ''
            GRUB_INSTALL_ARGS="$GRUB_INSTALL_ARGS --no-nvram"
          ''}

          ${lib.optionalString actualLoaderExtraArgs.efiInstallAsRemovable ''
            GRUB_INSTALL_ARGS="$GRUB_INSTALL_ARGS --removable"
          ''}

          ${pkgs.grub2_efi}/bin/grub-install $GRUB_INSTALL_ARGS || true

          # Copy GRUB EFI to fallback location for UEFI firmware boot
          # UEFI looks for /EFI/BOOT/BOOTX64.EFI when no NVRAM entries exist
          ${lib.optionalString (!actualLoaderExtraArgs.efiInstallAsRemovable) ''
            if [ -f /boot/EFI/NMBL/grubx64.efi ]; then
              echo "Copying GRUB EFI to fallback location /EFI/BOOT/BOOTX64.EFI..."
              cp /boot/EFI/NMBL/grubx64.efi /boot/EFI/BOOT/BOOTX64.EFI
              echo "✓ GRUB EFI fallback bootloader installed"
            else
              echo "WARNING: GRUB EFI binary not found at /boot/EFI/NMBL/grubx64.efi"
            fi
          ''}

          echo "✓ GRUB EFI bootloader installed"
        fi
  ''}

  ${lib.optionalString (bootstrapper.bootMode == "uefi" && actualLoader == "systemd") ''
        echo "Configuring GPT+UEFI bootloader with systemd-boot..."
        mkdir -p /boot/EFI/BOOT /boot/loader/entries

        # Create systemd-boot loader config
        cat > /boot/loader/loader.conf << 'EOF'
    default nmbl.conf
    timeout ${toString actualLoaderExtraArgs.timeout}
    console-mode max
    editor no
    ${actualLoaderExtraArgs.extraConfig}
    EOF

        # Create boot entry
        cat > /boot/loader/entries/nmbl.conf << 'EOF'
    title NMBL Bootloader
    linux /nmbl-kernel
    initrd /nmbl-initrd
    options $KERNEL_PARAMS
    EOF

        # Install systemd-boot if device exists
        if [ -b /dev/vda ]; then
          echo "Installing systemd-boot to /boot ESP..."
          BOOTCTL_ARGS="install --esp-path=/boot"

          ${lib.optionalString (!actualLoaderExtraArgs.canTouchEfiVariables) ''
            BOOTCTL_ARGS="$BOOTCTL_ARGS --no-variables"
          ''}

          ${pkgs.systemd}/bin/bootctl $BOOTCTL_ARGS || true

          # Copy systemd-boot to fallback location for UEFI firmware boot
          if [ -f /boot/EFI/systemd/systemd-bootx64.efi ]; then
            echo "Copying systemd-boot to fallback location /EFI/BOOT/BOOTX64.EFI..."
            cp /boot/EFI/systemd/systemd-bootx64.efi /boot/EFI/BOOT/BOOTX64.EFI
            echo "✓ systemd-boot fallback bootloader installed"
          else
            echo "WARNING: systemd-boot EFI binary not found"
          fi

          echo "✓ systemd-boot bootloader installed"
        fi
  ''}

  # Create /init symlink for NixOS stage-1
  # After kexec, the NixOS kernel's stage-1 will look for /init (or /sbin/init)
  # We need to symlink it to the system's init script
  echo "Creating /init symlink for stage-2 boot..."
  if [ -e /nix/var/nix/profiles/system/init ]; then
    ln -sf /nix/var/nix/profiles/system/init /init
    echo "✓ Created /init -> /nix/var/nix/profiles/system/init"
  else
    echo "WARNING: System init not found at /nix/var/nix/profiles/system/init"
  fi

  echo "NMBL bootloader installation complete!"
''
