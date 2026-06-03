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
  nmblUki,
  # Install-time driver-image staging + `nmbl-sign` signing shell (#25a).
  # Empty string when no driver images are enabled (default keeps older
  # callers evaluable).
  driverImageInstallShell ? "",
  # The host-platform `nmbl-sign` ML-DSA signer (flake `_module.args.nmblSign`).
  # Threaded into install-signing.nix for per-generation signing; `null` on an
  # older host flake (only dereferenced when signing is enabled).
  nmblSign ? null,
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

  # Resolve the cryptsetup the activation plan uses (prefer the static
  # build, same as lib/modules/activation.nix's `tryStatic`). Handed to
  # `--validate-hardware` so the read-only LUKS-header probe uses the
  # exact tool the toml implies, with a self-magic fallback if absent.
  tryStatic = attr:
    let
      s = pkgs.pkgsStatic.${attr} or null;
      d = pkgs.${attr} or null;
    in if s != null then s else d;
  cryptsetupPkg = tryStatic "cryptsetup";
  cryptsetupToolArg =
    lib.optionalString (cryptsetupPkg != null)
      "--tool=cryptsetup:${cryptsetupPkg}/bin/cryptsetup";
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

  # Read-only hardware validation of the SAME config.toml the bootloader
  # ships, BEFORE any bootloader files are written. Probes each declared
  # device against the real machine (LUKS headers, device existence);
  # zero side effects. `refuseInvalidHardwareOnInstall` decides whether a
  # failure aborts the install or is only a severe warning.
  echo "Validating NMBL config against target hardware..."
  ${
    if cfg.refuseInvalidHardwareOnInstall then
      # set -e is active: a non-zero exit aborts the install here.
      ''${config.system.build.nmblInit}/bin/nmbl-init --validate-hardware=${nmblConfigToml} ${cryptsetupToolArg}''
    else
      ''${config.system.build.nmblInit}/bin/nmbl-init --validate-hardware=${nmblConfigToml} ${cryptsetupToolArg} || echo "SEVERE WARNING: NMBL hardware validation failed; installing anyway because refuseInvalidHardwareOnInstall=false"''
  }

  ${lib.optionalString (actualLoader != "efi-stub") ''
    KERNEL="${config.system.build.nmblKernel}/bzImage"
    INITRD="${config.system.build.nmblInitramfs}/initrd"

    # Copy NMBL kernel and initrd to boot partition. SKIPPED in efi-stub
    # mode: there the kernel + initrd live inside the UKI PE installed at
    # EFI/BOOT/BOOTX64.EFI, so no separate files belong on the ESP.
    echo "Copying NMBL bootloader files to /boot..."
    mkdir -p /boot
    cp -f "$KERNEL" /boot/nmbl-kernel
    cp -f "$INITRD" /boot/nmbl-initrd
    echo "✓ Bootloader files installed: /boot/nmbl-kernel, /boot/nmbl-initrd"
  ''}

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

  # Optional signed driver-image squashfs blobs (#25a). Each is staged onto
  # the ESP and signed in place with `nmbl-sign --domain driver-image`
  # (impure ML-DSA key read at install time). Empty string when no driver
  # images are enabled.
  ${driverImageInstallShell}

  ${lib.optionalString (cfg.splash.enable && cfg.splash.backgroundLocation == "boot-partition") (
    let
      # Splash background sidecar mode: the PNG is NOT embedded in the
      # initramfs (see lib/config.nix `splashBackgroundContents`).
      # Instead it lives on the boot partition next to the initrd at a
      # FIXED basename (`nmblsplash.png`, mirrored by the Rust constant
      # `SIDECAR_SPLASH_BG_BASENAME`), which the Rust /init reads at
      # runtime from the Phase-0.5 boot mountpoint. The name is not
      # configurable, so this destination is constant.
      escapedDest = lib.escapeShellArg "/boot/nmblsplash.png";
    in ''
      # Boot-partition splash mode: stage the background PNG on /boot at
      # the fixed basename the Rust splash loader reads from. The
      # initramfs carries only the font in this mode; the background is
      # read on demand once Phase 0.5 has mounted the boot partition. A
      # missing file degrades to a solid background at runtime — it never
      # blocks boot — but we still install it here so the image renders.
      echo "Staging NMBL splash background to ${escapedDest}..."
      install -D -m 0644 ${cfg.splash.backgroundImage} ${escapedDest}
      echo "✓ Splash background installed: ${escapedDest}"
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
            # `sgdisk -p` puts the type code in column 5 *only* if the Size
            # cell is unit-less; "1024.0 KiB" splits over two tokens and shifts
            # EF02 to column 6. grep -w on the token is column-agnostic and
            # EF02 only appears in the Code column of sgdisk output.
            if ${pkgs.gptfdisk}/bin/sgdisk -p "$dev" 2>/dev/null \
                | grep -qw "EF02"; then
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

  ${import ./install-signing.nix {
    inherit
      lib
      pkgs
      config
      bootstrapper
      actualLoader
      actualLoaderExtraArgs
      nmblUki
      nmblSign
      ;
    # Install-time UKI Secure-Boot signing policy. `cfg.signing` may be the
    # bare skeleton on builds without the security slice; read with `or`
    # defaults so non-secure-boot configs keep installing the unsigned PE.
    ukiSigning =
      let u = cfg.signing.uki or { };
      in {
        enable = u.enable or false;
        keyFile = u.keyFile or null;
        certFile = u.certFile or null;
        refuseInstallIfNotEnforcing = u.refuseInstallIfNotEnforcing or false;
      };
    # Install-time per-generation ML-DSA signing policy. Same `or`-default
    # posture so non-secure-boot configs keep installing without signing.
    genSigning =
      let s = cfg.signing or { };
      in {
        enable = s.enable or false;
        keyFile = s.generationKeyFile or null;
        sigPathSuffix = s.sigPathSuffix or ".sig";
      };
    # Build-time-only: skip in-installer signing (unsigned UKI, no sidecars) so
    # a sealed image builder that cannot read the impure keys still completes.
    # Runtime enforcement is unchanged; the boot partition is signed out of band.
    deferInstallSigning = cfg.signing.deferInstallSigning or false;
  }}

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
