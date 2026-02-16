# Installation Script Module
# Generates the install-nmbl command for manual bootloader installation

{
  lib,
  pkgs,
  config,
  cfg,
  bootstrapper,
  actualLoader,
  actualLoaderExtraArgs,
}:

{
  # Manual installation script for NMBL bootloader
  installNmbl = pkgs.writeShellScriptBin "install-nmbl" ''
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
}

