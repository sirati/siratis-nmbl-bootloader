# Kexec Boot Execution Script
# Returns a shell script string for performing the kexec operation
# POSIX-compliant - works with busybox sh (no bash arrays)

{
  lib,
  pkgs,
  cfg,
  fileSystems,
  utils,
}:

let
  # Get mount points in reverse order for unmounting
  # (unmount children before parents)
  reversedFileSystems = lib.reverseList fileSystems;
in
''
  # ============================================
  # Part 4: Kexec Boot Execution
  # ============================================

  echo ""
  echo "NMBL: Preparing to kexec into selected generation..."

  # Selected generation info is already in variables from previous step:
  # - SELECTED_GEN
  # - SELECTED_KERNEL
  # - SELECTED_INITRD
  # - SELECTED_PARAMS

  echo "NMBL: Booting generation $SELECTED_GEN"

  # Verify kernel and initrd exist
  if [ ! -f "$SELECTED_KERNEL" ]; then
    echo "ERROR: Kernel not found at $SELECTED_KERNEL"
    exit 1
  fi

  if [ ! -f "$SELECTED_INITRD" ]; then
    echo "ERROR: Initrd not found at $SELECTED_INITRD"
    exit 1
  fi

  # Build final kernel parameters
  # Start with the generation's kernel params
  FINAL_PARAMS="$SELECTED_PARAMS"

  # Add root device parameter if not already present
  if ! echo "$FINAL_PARAMS" | grep -q "root="; then
    # Use the first filesystem's device as root
    echo "NMBL: Adding root device parameter"
    FINAL_PARAMS="root=/dev/sda1 $FINAL_PARAMS"
  fi

  echo "NMBL: Final kernel parameters: $FINAL_PARAMS"
  echo ""

  # Load kernel and initrd into RAM
  echo "NMBL: Loading kernel into memory..."
  echo "  Kernel: $SELECTED_KERNEL"
  echo "  Initrd: $SELECTED_INITRD"

  if ! ${pkgs.kexec-tools}/bin/kexec -l "$SELECTED_KERNEL" \
    --initrd="$SELECTED_INITRD" \
    --command-line="$FINAL_PARAMS"; then
    echo "ERROR: Failed to load kernel with kexec"
    echo "This may be a permission issue or the kernel may not support kexec"
    exit 1
  fi

  echo "NMBL: ✓ Kernel loaded successfully into memory"
  echo ""

  # Unmount filesystems in reverse order (children before parents)
  echo "NMBL: Unmounting filesystems..."
  ${lib.concatMapStringsSep "\n" (fs: ''
    echo "  Unmounting ${cfg.mountPrefix}${fs.mountPoint}..."
    ${pkgs.busybox}/bin/umount "${cfg.mountPrefix}${fs.mountPoint}" 2>/dev/null || echo "    (already unmounted or busy)"
  '') reversedFileSystems}

  # Sync to ensure all writes are flushed
  echo "NMBL: Syncing filesystems..."
  ${pkgs.busybox}/bin/sync

  echo ""
  echo "=========================================="
  echo "NMBL: Executing kexec..."
  echo "=========================================="
  echo "Switching to generation $SELECTED_GEN"
  echo ""

  # Small delay to allow reading the messages
  ${pkgs.busybox}/bin/sleep 1

  # Execute kexec - this replaces the current kernel
  exec ${pkgs.kexec-tools}/bin/kexec -e
''
