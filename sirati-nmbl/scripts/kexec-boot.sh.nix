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

  # Resolve symlinks to actual kernel and initrd files
  # The paths from the system profile are symlinks with absolute targets
  # We need to prepend the mount prefix to make them accessible
  resolve_file_path() {
    local link_path="$1"
    if [ -L "$link_path" ]; then
      # Read the symlink target
      local target=$(${pkgs.busybox}/bin/readlink "$link_path")
      # If target is absolute, prepend mount prefix
      case "$target" in
        /*)
          echo "${cfg.mountPrefix}$target"
          ;;
        *)
          # Relative path - resolve relative to link directory
          echo "$(${pkgs.busybox}/bin/dirname "$link_path")/$target"
          ;;
      esac
    else
      echo "$link_path"
    fi
  }

  # Resolve the symlinks
  SELECTED_KERNEL=$(resolve_file_path "$SELECTED_KERNEL")
  SELECTED_INITRD=$(resolve_file_path "$SELECTED_INITRD")

  echo "NMBL: Resolved kernel: $SELECTED_KERNEL"
  echo "NMBL: Resolved initrd: $SELECTED_INITRD"

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
  # NixOS stage-1 reads filesystem configuration from the initramfs,
  # so we don't need to add root= parameter - it's already known
  FINAL_PARAMS="$SELECTED_PARAMS"

  echo "NMBL: Final kernel parameters: $FINAL_PARAMS"
  echo ""

  # Load kernel and initrd into RAM
  echo "NMBL: Loading kernel into memory..."
  echo "  Kernel: $SELECTED_KERNEL"
  echo "  Initrd: $SELECTED_INITRD"

  if ! ${pkgs.kexec-tools}/bin/kexec -s -l "$SELECTED_KERNEL" \
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

  # Drop caches for cleaner kexec transition
  echo "NMBL: Dropping caches..."
  echo 3 > /proc/sys/vm/drop_caches

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
