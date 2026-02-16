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
# For legacy systems: boot partition should be /boot

{
  lib,
  pkgs,
  config,
  cfg,
}:

pkgs.writeScript "install-nmbl-bootloader" ''
  #!${pkgs.runtimeShell}
  set -e

  echo "Installing NMBL bootloader..."

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

  ${lib.optionalString (cfg.bootMode == "mbr") ''
        echo "Configuring MBR bootloader with syslinux..."
        mkdir -p /boot/syslinux

        # Create syslinux config
        cat > /boot/syslinux/syslinux.cfg << EOF
    DEFAULT nmbl
    PROMPT 0
    TIMEOUT 0
    SERIAL 0 115200

    LABEL nmbl
      KERNEL /nmbl-kernel
      INITRD /nmbl-initrd
      APPEND $KERNEL_PARAMS
    EOF

        # Install syslinux MBR if device exists
        if [ -b /dev/vda ]; then
          echo "Installing syslinux to /dev/vda1 (boot partition)..."
          ${pkgs.syslinux}/bin/syslinux --install /dev/vda1 || true
          echo "Installing MBR boot code to /dev/vda..."
          ${pkgs.util-linux}/bin/dd bs=440 count=1 conv=notrunc if=${pkgs.syslinux}/share/syslinux/mbr.bin of=/dev/vda || true
          echo "✓ Syslinux MBR bootloader installed"
        fi
  ''}

  ${lib.optionalString (cfg.bootMode == "gpt-bios") ''
        echo "Configuring GPT+BIOS bootloader with GRUB..."
        mkdir -p /boot/grub

        # Create GRUB config
        cat > /boot/grub/grub.cfg << EOF
    set timeout=0
    serial --unit=0 --speed=115200
    terminal_input serial
    terminal_output serial

    menuentry "NMBL Bootloader" {
      linux /nmbl-kernel $KERNEL_PARAMS
      initrd /nmbl-initrd
    }
    EOF

        # Install GRUB if device exists
        if [ -b /dev/vda ]; then
          echo "Installing GRUB (GPT+BIOS mode) to /dev/vda..."
          ${pkgs.grub2}/bin/grub-install --target=i386-pc /dev/vda || true
          echo "✓ GRUB bootloader installed"
        fi
  ''}

  ${lib.optionalString (cfg.bootMode == "gpt-uefi") ''
        echo "Configuring GPT+UEFI bootloader with GRUB..."
        mkdir -p /boot/EFI/BOOT /boot/grub

        # Create GRUB config
        cat > /boot/grub/grub.cfg << EOF
    set timeout=0
    serial --unit=0 --speed=115200
    terminal_input serial
    terminal_output serial

    menuentry "NMBL Bootloader" {
      linux /nmbl-kernel $KERNEL_PARAMS
      initrd /nmbl-initrd
    }
    EOF

        # Install GRUB EFI if device exists
        if [ -b /dev/vda ]; then
          echo "Installing GRUB (UEFI mode) to /boot ESP..."
          ${pkgs.grub2_efi}/bin/grub-install --target=x86_64-efi --efi-directory=/boot --bootloader-id=NMBL --no-nvram || true

          # Copy GRUB EFI to fallback location for UEFI firmware boot
          # UEFI looks for /EFI/BOOT/BOOTX64.EFI when no NVRAM entries exist
          if [ -f /boot/EFI/NMBL/grubx64.efi ]; then
            echo "Copying GRUB EFI to fallback location /EFI/BOOT/BOOTX64.EFI..."
            cp /boot/EFI/NMBL/grubx64.efi /boot/EFI/BOOT/BOOTX64.EFI
            echo "✓ GRUB EFI fallback bootloader installed"
          else
            echo "WARNING: GRUB EFI binary not found at /boot/EFI/NMBL/grubx64.efi"
          fi

          echo "✓ GRUB EFI bootloader installed"
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
