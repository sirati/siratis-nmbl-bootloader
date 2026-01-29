# NMBL Bootloader Installation Script
# This script is called by system.build.installBootLoader during VM builds and system installations

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

  KERNEL="${config.system.build.nmblKernel}/bzImage"
  INITRD="${config.system.build.nmblInitramfs}/initrd"
  KERNEL_PARAMS="${lib.concatStringsSep " " cfg.kernelParams}"

  # Copy NMBL kernel and initrd to boot partition
  echo "Copying NMBL bootloader files to /boot..."
  mkdir -p /boot
  cp -f "$KERNEL" /boot/nmbl-kernel
  cp -f "$INITRD" /boot/nmbl-initrd

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
          echo "Installing syslinux to /dev/vda..."
          ${pkgs.syslinux}/bin/syslinux --install /dev/vda1 || true
          ${pkgs.util-linux}/bin/dd bs=440 count=1 conv=notrunc if=${pkgs.syslinux}/share/syslinux/mbr.bin of=/dev/vda || true
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
          echo "Installing GRUB to /dev/vda..."
          ${pkgs.grub2}/bin/grub-install --target=i386-pc /dev/vda || true
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
          echo "Installing GRUB EFI to /dev/vda..."
          ${pkgs.grub2_efi}/bin/grub-install --target=x86_64-efi --efi-directory=/boot --bootloader-id=NMBL --no-nvram || true
        fi
  ''}

  echo "NMBL bootloader installation complete!"
''
