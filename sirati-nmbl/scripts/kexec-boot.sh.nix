# Kexec Boot Execution Script
# Returns a shell script string for performing the kexec operation

{
  lib,
  pkgs,
  cfg,
}:

''
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
''
