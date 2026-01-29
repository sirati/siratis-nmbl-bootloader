# Kexec Boot Execution Script
# Returns a shell script string for performing the kexec operation

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

  ${pkgs.busybox}/bin/echo ""
  ${pkgs.busybox}/bin/echo "Booting generation ''${GENERATIONS[$SELECTED]}..."

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

  ${pkgs.busybox}/bin/echo "Final kernel parameters: $FINAL_PARAMS"

  # Verify kernel and initrd exist
  if [ ! -f "$KERNEL_PATH" ]; then
    ${pkgs.busybox}/bin/echo "ERROR: Kernel not found at $KERNEL_PATH"
    exit 1
  fi

  if [ ! -f "$INITRD_PATH" ]; then
    ${pkgs.busybox}/bin/echo "ERROR: Initrd not found at $INITRD_PATH"
    exit 1
  fi

  # Load kernel and initrd into RAM
  ${pkgs.busybox}/bin/echo "Loading kernel and initrd into RAM..."
  if ! ${pkgs.kexec-tools}/bin/kexec -l "$KERNEL_PATH" \
    --initrd="$INITRD_PATH" \
    --command-line="$FINAL_PARAMS"; then
    ${pkgs.busybox}/bin/echo "ERROR: Failed to load kernel with kexec"
    exit 1
  fi

  ${pkgs.busybox}/bin/echo "Kernel loaded successfully"

  # Unmount filesystems in reverse order (children before parents)
  ${pkgs.busybox}/bin/echo "Unmounting filesystems..."
  ${lib.concatMapStringsSep "\n" (fs: ''
    ${pkgs.busybox}/bin/echo "  Unmounting ${cfg.mountPrefix}${fs.mountPoint}..."
    ${pkgs.busybox}/bin/umount "${cfg.mountPrefix}${fs.mountPoint}" 2>/dev/null || ${pkgs.busybox}/bin/echo "    (already unmounted or busy)"
  '') reversedFileSystems}

  # Sync to ensure all writes are flushed
  ${pkgs.busybox}/bin/echo "Syncing filesystems..."
  ${pkgs.busybox}/bin/sync

  # Execute kexec
  ${pkgs.busybox}/bin/echo ""
  ${pkgs.busybox}/bin/echo "Executing kexec to boot selected system..."
  ${pkgs.busybox}/bin/echo "====================================="
  exec ${pkgs.kexec-tools}/bin/kexec -e
''
