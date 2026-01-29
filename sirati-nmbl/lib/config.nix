# NixOS Module Config Implementation for NMBL
# This file contains the actual implementation of the bootloader module

{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.boot.nmbl;

  # Import script components
  mountAndKernelScript = import ../scripts/mount-and-kernel.sh;
  findGenerationsScript = import ../scripts/find-generations.sh;
  selectionUIScript = import ../scripts/selection-ui.sh;
  kexecBootScript = import ../scripts/kexec-boot.sh;

  # Build the complete init script by combining all parts
  buildInitScript = pkgs.writeScript "init" ''
    #!${pkgs.busybox}/bin/sh
    set -e

    # ============================================
    # Part 1: Mount and Kernel Module Loading
    # ============================================

    # Mount essential filesystems
    mount -t proc proc /proc
    mount -t sysfs sys /sys
    mount -t devtmpfs dev /dev
    mkdir -p /dev/pts
    mount -t devpts devpts /dev/pts

    # Load kernel modules
    ${lib.concatMapStringsSep "\n" (mod: "modprobe ${mod} 2>/dev/null || true") cfg.kernelModules}

    # Wait for devices
    sleep 1

    # Mount the configured filesystems
    ${lib.concatStringsSep "\n" (
      lib.mapAttrsToList (mountPoint: fs: ''
        mkdir -p ${mountPoint}
        mount -t ${fs.fsType} -o ${lib.concatStringsSep "," fs.options} ${fs.device} ${mountPoint}
      '') cfg.fileSystems
    )}

    # ============================================
    # Part 2: Find NixOS Generations
    # ============================================

    BOOT_DIR="/mnt-root/boot"
    if [ ! -d "$BOOT_DIR" ]; then
      echo "Error: $BOOT_DIR not found"
      echo "Dropping into shell..."
      exec ${pkgs.bash}/bin/bash
    fi

    # Find all generations
    GENERATIONS=()
    KERNELS=()
    INITRDS=()
    KERNEL_PARAMS=()

    # Parse NixOS system profiles
    for system in $(ls -d /mnt-root/nix/var/nix/profiles/system-*-link 2>/dev/null | sort -V -r); do
      if [ -f "$system/kernel" ] && [ -f "$system/initrd" ]; then
        gen_num=$(basename "$system" | sed 's/system-\(.*\)-link/\1/')
        GENERATIONS+=("$gen_num")
        KERNELS+=("$system/kernel")
        INITRDS+=("$system/initrd")

        # Extract kernel parameters
        if [ -f "$system/kernel-params" ]; then
          KERNEL_PARAMS+=("$(cat $system/kernel-params)")
        else
          KERNEL_PARAMS+=("")
        fi
      fi
    done

    # Also check current system
    if [ -L "/mnt-root/nix/var/nix/profiles/system" ]; then
      system="/mnt-root/nix/var/nix/profiles/system"
      if [ -f "$system/kernel" ] && [ -f "$system/initrd" ]; then
        GENERATIONS=("current" "''${GENERATIONS[@]}")
        KERNELS=("$system/kernel" "''${KERNELS[@]}")
        INITRDS=("$system/initrd" "''${INITRDS[@]}")
        if [ -f "$system/kernel-params" ]; then
          KERNEL_PARAMS=("$(cat $system/kernel-params)" "''${KERNEL_PARAMS[@]}")
        else
          KERNEL_PARAMS=("" "''${KERNEL_PARAMS[@]}")
        fi
      fi
    fi

    if [ ''${#GENERATIONS[@]} -eq 0 ]; then
      echo "No NixOS generations found!"
      echo "Dropping into shell..."
      exec ${pkgs.bash}/bin/bash
    fi

    # ============================================
    # Part 3: Selection UI
    # ============================================

    # Get current kernel parameters
    CURRENT_PARAMS=$(cat /proc/cmdline)

    # Filter out NMBL-specific params for passthrough
    PASSTHROUGH_PARAMS=""
    ${lib.optionalString (cfg.kernelParams != [ ]) ''
      for param in $CURRENT_PARAMS; do
        skip=0
        ${lib.concatMapStringsSep "\n" (p: ''
          if echo "$param" | grep -q "^${lib.escapeShellArg (lib.head (lib.splitString "=" p))}"; then
            skip=1
          fi
        '') cfg.kernelParams}
        if [ $skip -eq 0 ]; then
          PASSTHROUGH_PARAMS="$PASSTHROUGH_PARAMS $param"
        fi
      done
    ''}
    ${lib.optionalString (cfg.kernelParams == [ ]) ''
      PASSTHROUGH_PARAMS="$CURRENT_PARAMS"
    ''}

    # NMBL kernel params
    NMBL_PARAMS="${lib.concatStringsSep " " cfg.kernelParams}"

    # Main menu loop
    PASSTHROUGH_ENABLED=1
    CUSTOM_PARAMS=""
    EDIT_MODE=0

    while true; do
      clear
      echo "=== NixOS Linux Bootloader ==="
      echo ""
      echo "Bootloader Kernel Params: $NMBL_PARAMS"
      echo "Passthrough Params: $PASSTHROUGH_PARAMS"
      echo ""

      if [ $EDIT_MODE -eq 1 ]; then
        echo "[Custom params mode: $CUSTOM_PARAMS]"
        echo ""
      fi

      echo "Available Generations:"
      for i in $(seq 0 $((''${#GENERATIONS[@]} - 1))); do
        echo "  [$i] Generation ''${GENERATIONS[$i]}"
      done
      echo ""

      if [ $PASSTHROUGH_ENABLED -eq 1 ]; then
        echo "[X] Passthrough kernel params (enabled)"
      else
        echo "[ ] Passthrough kernel params (disabled)"
      fi
      echo ""
      echo "Commands:"
      echo "  0-9: Select generation to boot"
      echo "  p: Toggle passthrough kernel params"
      echo "  e: Edit kernel params"
      echo "  s: Drop to shell"
      echo ""
      echo -n "Select option (auto-boot 0 in ${toString cfg.timeoutSeconds}s): "

      # Read with timeout
      INPUT=""
      if read -t ${toString cfg.timeoutSeconds} INPUT; then
        # Process input
        case "$INPUT" in
          p|P)
            if [ $PASSTHROUGH_ENABLED -eq 1 ]; then
              PASSTHROUGH_ENABLED=0
            else
              PASSTHROUGH_ENABLED=1
            fi
            continue
            ;;
          e|E)
            echo ""
            echo "Enter custom kernel parameters:"
            read -e -i "$CUSTOM_PARAMS" CUSTOM_PARAMS
            EDIT_MODE=1
            continue
            ;;
          s|S)
            echo ""
            echo "Dropping into shell..."
            exec ${pkgs.bash}/bin/bash
            ;;
          [0-9])
            if [ "$INPUT" -ge 0 ] && [ "$INPUT" -lt "''${#GENERATIONS[@]}" ]; then
              SELECTED=$INPUT
              break
            else
              echo "Invalid selection!"
              sleep 1
              continue
            fi
            ;;
          *)
            echo "Invalid input!"
            sleep 1
            continue
            ;;
        esac
      else
        # Timeout, boot default
        SELECTED=0
        break
      fi
    done

    # ============================================
    # Part 4: Kexec Boot Execution
    # ============================================

    echo ""
    echo "Booting generation ''${GENERATIONS[$SELECTED]}..."

    # Prepare for kexec
    KERNEL_PATH="''${KERNELS[$SELECTED]}"
    INITRD_PATH="''${INITRDS[$SELECTED]}"
    ENTRY_PARAMS="''${KERNEL_PARAMS[$SELECTED]}"

    # Build final params
    FINAL_PARAMS=""

    # Add passthrough params if enabled
    if [ $PASSTHROUGH_ENABLED -eq 1 ]; then
      FINAL_PARAMS="$PASSTHROUGH_PARAMS"
    fi

    # Add entry params
    FINAL_PARAMS="$FINAL_PARAMS $ENTRY_PARAMS"

    # Add custom params if in edit mode
    if [ $EDIT_MODE -eq 1 ]; then
      FINAL_PARAMS="$FINAL_PARAMS $CUSTOM_PARAMS"
    fi

    echo "Final kernel parameters: $FINAL_PARAMS"

    # Load kernel and initrd into RAM
    echo "Loading kernel and initrd..."
    ${pkgs.kexec-tools}/bin/kexec -l "$KERNEL_PATH" \
      --initrd="$INITRD_PATH" \
      --command-line="$FINAL_PARAMS"

    # Unmount filesystems
    echo "Unmounting filesystems..."
    ${lib.concatStringsSep "\n" (
      lib.mapAttrsToList (mountPoint: fs: ''
        umount ${mountPoint} || true
      '') cfg.fileSystems
    )}

    sync

    # Execute kexec
    echo "Executing kexec..."
    exec ${pkgs.kexec-tools}/bin/kexec -e
  '';

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
            # Include necessary kernel modules
            {
              object = "${kernel}/lib/modules";
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
        Boot Mode: ${cfg.bootMode}
        Kernel: ${kernel}/bzImage
        Initrd: ${initrd}/initrd
        Kernel Parameters: ${kernelParams}
      '';

    # Boot loader installation
    boot.loader.grub.enable = lib.mkDefault false;
    boot.loader.systemd-boot.enable = lib.mkDefault false;

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
        # Install systemd-boot or GRUB for UEFI
        mkdir -p /boot/EFI/BOOT

        ${pkgs.grub2_efi}/bin/grub-install --target=x86_64-efi --efi-directory=/boot --bootloader-id=NMBL

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

      echo "NMBL bootloader installed successfully!"
    '';

    # Add kexec-tools to system packages
    environment.systemPackages = [
      pkgs.kexec-tools
      config.system.build.installNmbl
    ];
  };
}
