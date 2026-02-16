# Mount and Kernel Module Loading Script
# Returns a shell script string for mounting filesystems and loading kernel modules
# This follows the same logic as NixOS stage-1 init

{
  lib,
  pkgs,
  cfg,
  fileSystems,
  utils,
  kernelModules,
}:

let
  # Helper to escape mount options
  escape = s: builtins.replaceStrings [ " " "\t" ] [ "\\040" "\\011" ] s;

  # Filter out systemd-specific mount options that shouldn't be passed to mount(8)
  # These are options like "x-initrd.mount", "x-systemd.*", "nofail", etc.
  filterMountOptions =
    opts: lib.filter (opt: !(lib.hasPrefix "x-" opt) && opt != "nofail" && opt != "_netdev") opts;

  # Format filesystem info for mounting
  formatFS = fs: {
    mountPoint = fs.mountPoint;
    device = if fs.device != null then fs.device else "/dev/disk/by-label/${escape fs.label}";
    fsType = fs.fsType;
    options = builtins.concatStringsSep "," (
      # Mount /boot read-write so NMBL can write bootloader files
      # Mount other filesystems read-only for safety
      lib.unique (
        filterMountOptions fs.options
        ++ (if fs.mountPoint == "/boot" || fs.mountPoint == "/efi" then [ "rw" ] else [ "ro" ])
      )
    );
  };

  # Get formatted filesystem list
  fsList = map formatFS fileSystems;

in
''
  # ============================================
  # Part 1: Mount Essential Filesystems
  # ============================================

  info "NMBL: Starting filesystem initialization..."

  # Mount essential filesystems first
  info "Mounting proc at /proc"
  ${pkgs.busybox}/bin/mount -t proc proc /proc

  info "Mounting sysfs at /sys"
  ${pkgs.busybox}/bin/mount -t sysfs sys /sys

  info "Mounting devtmpfs at /dev"
  ${pkgs.busybox}/bin/mount -t devtmpfs dev /dev

  ${pkgs.busybox}/bin/mkdir -p /dev/pts
  info "Mounting devpts at /dev/pts"
  ${pkgs.busybox}/bin/mount -t devpts devpts /dev/pts

  ${pkgs.busybox}/bin/mkdir -p /tmp
  info "Mounting tmpfs at /tmp"
  ${pkgs.busybox}/bin/mount -t tmpfs tmpfs /tmp

  # ============================================
  # Part 2: Load Kernel Modules
  # ============================================

  # Set modprobe path so kernel can find modules
  info "NMBL: Configuring module loading..."
  echo "${pkgs.kmod}/bin/modprobe" > /proc/sys/kernel/modprobe

  # Load kernel modules needed for storage and filesystems
  # Use module names (not paths) - modprobe handles decompression
  # Only report failures - suppress "Invalid ELF header magic" errors
  info "NMBL: Loading required kernel modules..."
  ${lib.concatMapStringsSep "\n" (mod: ''
    # Try to load the module and capture stderr
    MOD_ERR=$(modprobe ${mod} 2>&1) || true

    # Check if module is now loaded (either we just loaded it, or it was already loaded/built-in)
    if ! ( ${pkgs.busybox}/bin/grep -q "^${mod} " /proc/modules 2>/dev/null || \
           ${pkgs.busybox}/bin/test -e "/sys/module/${mod}" 2>/dev/null ); then
      # Module failed to load and is not loaded - report error
      # Filter out "Invalid ELF header magic" noise
      FILTERED_ERR=$(echo "$MOD_ERR" | ${pkgs.busybox}/bin/grep -v "Invalid ELF header magic" || true)
      if [ -n "$FILTERED_ERR" ]; then
        echo "NMBL: Failed to load module ${mod}: $FILTERED_ERR"
      else
        echo "NMBL: Failed to load module ${mod} (module may not be needed)"
      fi
    fi
  '') kernelModules}

  # Wait for required devices to become available
  info "NMBL: Waiting for block devices to become available..."

  # Build list of required devices
  REQUIRED_DEVICES="${
    lib.concatStringsSep " " (
      map (fs: if fs.device != null && lib.hasPrefix "/dev/" fs.device then fs.device else "") (
        lib.filter (fs: fs.device != null) fileSystems
      )
    )
  }"

  # Wait for each device with timeout based on actual elapsed time
  MAX_WAIT=10
  START_TIME=$(${pkgs.busybox}/bin/cat /proc/uptime | ${pkgs.busybox}/bin/cut -d' ' -f1)
  ALL_READY=0
  LAST_PROGRESS=0

  while [ $ALL_READY -eq 0 ]; do
    # Check current time and calculate elapsed
    CURRENT_TIME=$(${pkgs.busybox}/bin/cat /proc/uptime | ${pkgs.busybox}/bin/cut -d' ' -f1)
    ELAPSED=$(${pkgs.busybox}/bin/awk "BEGIN {printf \"%.0f\", $CURRENT_TIME - $START_TIME}")

    if [ $ELAPSED -ge $MAX_WAIT ]; then
      break
    fi

    # Check if all devices are ready and collect missing ones
    ALL_READY=1
    MISSING_DEVICES=""
    for dev in $REQUIRED_DEVICES; do
      if [ -n "$dev" ] && [ ! -e "$dev" ]; then
        ALL_READY=0
        # Strip /dev/ prefix and add to comma-separated list
        dev_name=$(echo "$dev" | ${pkgs.busybox}/bin/sed 's|^/dev/||')
        if [ -z "$MISSING_DEVICES" ]; then
          MISSING_DEVICES="$dev_name"
        else
          MISSING_DEVICES="$MISSING_DEVICES, $dev_name"
        fi
      fi
    done

    # Print progress message every second
    if [ $ALL_READY -eq 0 ] && [ $ELAPSED -gt $LAST_PROGRESS ]; then
      LAST_PROGRESS=$ELAPSED
      echo "NMBL: Still waiting for devices: $MISSING_DEVICES (''${ELAPSED}s elapsed)"
    fi

    if [ $ALL_READY -eq 0 ]; then
      ${pkgs.busybox}/bin/sleep 0.025
    fi
  done

  if [ $ALL_READY -eq 1 ]; then
    FINAL_TIME=$(${pkgs.busybox}/bin/cat /proc/uptime | ${pkgs.busybox}/bin/cut -d' ' -f1)
    WAIT_MS=$(${pkgs.busybox}/bin/awk "BEGIN {printf \"%.0f\", ($FINAL_TIME - $START_TIME) * 1000}")
    info "NMBL: ✓ All required devices available (waited $WAIT_MS ms)"
  else
    echo "NMBL: ⚠ Timeout waiting for devices after $MAX_WAIT s, proceeding anyway..."
  fi

  # ============================================
  # Part 3: Mount System Filesystems
  # ============================================

  info "Mounting system filesystems..."
  info "Available block devices:"
  ${pkgs.busybox}/bin/ls -la /dev/sd* /dev/vd* /dev/hd* /dev/nvme* 2>/dev/null || info "  (no block devices found yet)"

  # Mount each filesystem needed for boot
  ${lib.concatMapStringsSep "\n" (fs: ''
    info ""
    info "mounting ${fs.device} on ${fs.mountPoint}..."
    info "  Type: ${fs.fsType}"
    info "  Options: ${fs.options}"

    # Create mount point
    ${pkgs.busybox}/bin/mkdir -p "${cfg.mountPrefix}${fs.mountPoint}"

    # Attempt to mount
    if ${pkgs.busybox}/bin/mount -t ${fs.fsType} -o ${fs.options} ${fs.device} "${cfg.mountPrefix}${fs.mountPoint}"; then
      info "  ✓ Successfully mounted ${fs.mountPoint}"
    else
      echo "  ✗ ERROR: Failed to mount ${fs.device} at ${cfg.mountPrefix}${fs.mountPoint}"
      echo "  This filesystem is required for boot!"
      exit 1
    fi
  '') fsList}

  info ""
  info "All filesystems mounted successfully!"
  info "Mount summary:"
  ${pkgs.busybox}/bin/mount | ${pkgs.busybox}/bin/grep "^/" || true
''
