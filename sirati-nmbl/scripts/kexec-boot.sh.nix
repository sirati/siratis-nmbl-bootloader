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

  info ""
  info "NMBL: Preparing to kexec into selected generation..."

  # Selected generation info is already in variables from previous step:
  # - SELECTED_GEN
  # - SELECTED_KERNEL
  # - SELECTED_INITRD
  # - SELECTED_PARAMS

  info "NMBL: Booting generation $SELECTED_GEN"

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

  # Compute the NixOS system dir BEFORE resolving symlinks.
  # SELECTED_KERNEL at this point is e.g.
  #   /mnt/nix/store/52x7zj3...-nixos-system-.../kernel
  # which is a symlink in the nixos-system directory.  dirname gives the
  # nixos-system dir which contains /init (the NixOS stage-2 init script).
  # After resolve_file_path, SELECTED_KERNEL becomes the kernel binary
  # inside the kernel package (linux-*) which is NOT the right parent.
  SELECTED_SYSTEM_DIR=$(${pkgs.busybox}/bin/dirname "$SELECTED_KERNEL")
  # Strip the NMBL mount prefix to get a store-absolute path that will be
  # valid on the root fs after NixOS stage-1 mounts /sysroot.
  # ${cfg.mountPrefix} is Nix-interpolated to /mnt at build time.
  SELECTED_INIT_ABS=$(echo "$SELECTED_SYSTEM_DIR" | ${pkgs.busybox}/bin/sed 's|^${cfg.mountPrefix}||')/init

  info "NMBL: Injecting init= for NixOS stage-1: $SELECTED_INIT_ABS"

  # Resolve the symlinks
  SELECTED_KERNEL=$(resolve_file_path "$SELECTED_KERNEL")
  SELECTED_INITRD=$(resolve_file_path "$SELECTED_INITRD")

  info "NMBL: Resolved kernel: $SELECTED_KERNEL"
  info "NMBL: Resolved initrd: $SELECTED_INITRD"

  # Verify kernel and initrd exist
  if [ ! -f "$SELECTED_KERNEL" ]; then
    echo "ERROR: Kernel not found at $SELECTED_KERNEL"
    exit 1
  fi

  if [ ! -f "$SELECTED_INITRD" ]; then
    echo "ERROR: Initrd not found at $SELECTED_INITRD"
    exit 1
  fi

  # Build final kernel parameters — always append init= so NixOS stage-1
  # can locate the closure.
  FINAL_PARAMS="$SELECTED_PARAMS init=$SELECTED_INIT_ABS"

  info "NMBL: Final kernel parameters: $FINAL_PARAMS"
  info ""

  # Load kernel and initrd into RAM
  info "NMBL: Loading kernel into memory..."
  info "  Kernel: $SELECTED_KERNEL"
  info "  Initrd: $SELECTED_INITRD"

  if ! ${pkgs.kexec-tools}/bin/kexec -s -l "$SELECTED_KERNEL" \
    --initrd="$SELECTED_INITRD" \
    --command-line="$FINAL_PARAMS"; then
    echo "ERROR: Failed to load kernel with kexec"
    echo "This may be a permission issue or the kernel may not support kexec"
    exit 1
  fi

  info "NMBL: ✓ Kernel loaded successfully into memory"
  info ""

  # Unmount filesystems in reverse order (children before parents)
  info "NMBL: Unmounting filesystems..."
  ${lib.concatMapStringsSep "\n" (fs: ''
    info "  Unmounting ${cfg.mountPrefix}${fs.mountPoint}..."
    ${pkgs.busybox}/bin/umount "${cfg.mountPrefix}${fs.mountPoint}" 2>/dev/null || info "    (already unmounted or busy)"
  '') reversedFileSystems}

  # Sync to ensure all writes are flushed
  info "NMBL: Syncing filesystems..."
  ${pkgs.busybox}/bin/sync

  # Drop caches for cleaner kexec transition
  info "NMBL: Dropping caches..."
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
