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
  configLocation,
  nmblConfigToml,
  nmblRescueSquashfs,
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

  # Copy NMBL kernel and initrd to boot partition
  echo "Copying NMBL bootloader files to /boot..."
  mkdir -p /boot
  cp -f "$KERNEL" /boot/nmbl-kernel
  cp -f "$INITRD" /boot/nmbl-initrd
  echo "✓ Bootloader files installed: /boot/nmbl-kernel, /boot/nmbl-initrd"

  ${lib.optionalString cfg.stateful.enable ''
    # Stateful mode: initialise (or upgrade) the persistent state.bin
    # under cfg.stateful.stateDir on /boot so the Rust /init has a
    # known-good slot to track boot attempts against on the next boot.
    echo "Initializing NMBL state at ${cfg.stateful.stateDir}/state.bin..."
    ${config.system.build.nmblInit}/bin/nmbl-init --init-state ${cfg.stateful.stateDir}
    echo "✓ State file initialised"
  ''}

  ${lib.optionalString (configLocation == "external") (
    let
      # In external-config mode, copy the full config.toml onto the boot
      # partition at the path the embedded bootstrap.toml will look for it.
      # The `or "/nmbl/config.toml"` fallback matches
      # `default_bootstrap_config_path` in `nmbl-init-rs/src/config.rs` so
      # the runtime contract holds even if `boot.nmbl.bootstrap.configPath`
      # is unset. Computed inside the optionalString body so embedded mode
      # never evaluates it.
      externalConfigPath =
        let p = cfg.bootstrap.configPath or "/nmbl/config.toml";
        in if lib.hasPrefix "/" p then lib.removePrefix "/" p else p;
      # `lib.escapeShellArg` protects the heredoc-generated shell script
      # against operator-supplied paths containing whitespace or quotes.
      escapedDest = lib.escapeShellArg "/boot/${externalConfigPath}";
    in ''
      # External-config mode: stage the full config.toml on /boot at the
      # path the embedded bootstrap.toml advertises. The initramfs itself
      # carries only the bootstrap, so this file is what nmbl-init reads
      # for filesystems / activations / TUI settings at boot time.
      echo "Staging external NMBL config to ${escapedDest}..."
      install -D -m 0644 ${nmblConfigToml} ${escapedDest}
      echo "✓ External config installed: ${escapedDest}"
    ''
  )}

  ${lib.optionalString (cfg.rescue.mode == "external") (
    let
      # `cfg.rescue.sfsPath` is interpreted relative to the boot mount
      # by the Rust /init; strip a leading slash so the host-side
      # install path joins cleanly under `/boot/`.
      rescuePath =
        if lib.hasPrefix "/" cfg.rescue.sfsPath
        then lib.removePrefix "/" cfg.rescue.sfsPath
        else cfg.rescue.sfsPath;
      escapedDest = lib.escapeShellArg "/boot/${rescuePath}";
    in ''
      # External-rescue mode: stage the squashfs blob on /boot at the
      # path the Rust disk-rescue path reads from. The initramfs itself
      # carries no busybox / activation tools in this mode — they all
      # live in the squashfs and are loop-mounted on the emergency
      # path.
      echo "Staging NMBL rescue squashfs to ${escapedDest}..."
      install -D -m 0644 ${nmblRescueSquashfs} ${escapedDest}
      echo "✓ Rescue squashfs installed: ${escapedDest}"
    ''
  )}

  ${lib.optionalString (bootstrapper.bootMode == "bios" && actualLoader == "grub") ''
        echo "Configuring GPT+BIOS bootloader with GRUB..."
        mkdir -p /boot/grub

        # Create GRUB config
        cat > /boot/grub/grub.cfg << EOF
    set timeout=${toString actualLoaderExtraArgs.timeout}
    set default=${actualLoaderExtraArgs.default}
    ${actualLoaderExtraArgs.extraConfig}

    menuentry "NMBL Bootloader" {
      linux /nmbl-kernel ${lib.concatStringsSep " " cfg.kernelParams}
      initrd /nmbl-initrd
    }
    ${actualLoaderExtraArgs.extraEntries}
    EOF

        # Discover boot disks (whole disks with an EF02 partition) unless
        # the caller pinned bootstrapper.bootDisks. grub-install runs per
        # disk so each member of e.g. a RAID1 mirror boots independently.
        boot_disks=( ${lib.escapeShellArgs bootstrapper.bootDisks} )
        if [ "''${#boot_disks[@]}" -eq 0 ]; then
          for blk in /sys/class/block/*; do
            [ -e "$blk/device" ] || continue      # skip loop/ram/dm/md
            [ -e "$blk/partition" ] && continue   # skip partitions
            dev="/dev/$(basename "$blk")"
            [ -b "$dev" ] || continue
            if ${pkgs.gptfdisk}/bin/sgdisk -p "$dev" 2>/dev/null \
                | awk 'NR>5 && $5=="EF02" {found=1} END {exit !found}'; then
              boot_disks+=("$dev")
            fi
          done
        fi

        if [ "''${#boot_disks[@]}" -eq 0 ]; then
          echo "ERROR: no boot disks with an EF02 partition found" >&2
          echo "       set boot.nmbl.bootstrapper.bootDisks explicitly" >&2
          exit 1
        fi

        for disk in "''${boot_disks[@]}"; do
          echo "Installing GRUB (GPT+BIOS mode) to $disk..."
          ${pkgs.grub2}/bin/grub-install --target=i386-pc "$disk" || true
          echo "✓ GRUB bootloader installed to $disk"
        done
  ''}

  ${lib.optionalString (bootstrapper.bootMode == "uefi" && actualLoader == "grub") ''
        echo "Configuring GPT+UEFI bootloader with GRUB..."
        mkdir -p /boot/EFI/BOOT /boot/grub

        # Create GRUB config
        cat > /boot/grub/grub.cfg << EOF
    set timeout=${toString actualLoaderExtraArgs.timeout}
    set default=${actualLoaderExtraArgs.default}
    ${actualLoaderExtraArgs.extraConfig}

    menuentry "NMBL Bootloader" {
      linux /nmbl-kernel ${lib.concatStringsSep " " cfg.kernelParams}
      initrd /nmbl-initrd
    }
    ${actualLoaderExtraArgs.extraEntries}
    EOF

        # Install GRUB to the mounted /boot ESP. UEFI doesn't need a per-disk
        # `grub-install` like BIOS does — grub-install writes the EFI binary
        # into the ESP, and firmware finds it via the fallback path or NVRAM.
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
        cat > /boot/loader/entries/nmbl.conf << EOF
    title NMBL Bootloader
    linux /nmbl-kernel
    initrd /nmbl-initrd
    options ${lib.concatStringsSep " " cfg.kernelParams}
    EOF

        echo "Installing systemd-boot to /boot ESP..."
        BOOTCTL_ARGS="install --esp-path=/boot"

        ${lib.optionalString (!actualLoaderExtraArgs.canTouchEfiVariables) ''
          BOOTCTL_ARGS="$BOOTCTL_ARGS --no-variables"
        ''}

        ${pkgs.systemd}/bin/bootctl $BOOTCTL_ARGS || true

        # Copy systemd-boot EFI binary directly from Nix store.
        # bootctl install may fail silently (--graceful) when /boot is on an
        # MD RAID device (not a raw GPT partition), so we always copy the EFI
        # binary ourselves as a fallback.  UEFI firmware finds it at the
        # well-known removable media path /EFI/BOOT/BOOTX64.EFI.
        mkdir -p /boot/EFI/systemd /boot/EFI/BOOT
        SDBOOT_EFI="${pkgs.systemd}/lib/systemd/boot/efi/systemd-bootx64.efi"
        if [ -f "$SDBOOT_EFI" ]; then
          cp "$SDBOOT_EFI" /boot/EFI/systemd/systemd-bootx64.efi
          cp "$SDBOOT_EFI" /boot/EFI/BOOT/BOOTX64.EFI
          echo "✓ systemd-boot EFI installed to /EFI/systemd/ and /EFI/BOOT/BOOTX64.EFI"
        else
          echo "WARNING: systemd-boot EFI binary not found at $SDBOOT_EFI"
        fi

        echo "✓ systemd-boot bootloader installed"
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
