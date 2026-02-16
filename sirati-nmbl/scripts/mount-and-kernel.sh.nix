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

  echo "NMBL: Starting filesystem initialization..."

  # Mount essential filesystems first
  echo "Mounting proc at /proc"
  ${pkgs.busybox}/bin/mount -t proc proc /proc

  echo "Mounting sysfs at /sys"
  ${pkgs.busybox}/bin/mount -t sysfs sys /sys

  echo "Mounting devtmpfs at /dev"
  ${pkgs.busybox}/bin/mount -t devtmpfs dev /dev

  ${pkgs.busybox}/bin/mkdir -p /dev/pts
  echo "Mounting devpts at /dev/pts"
  ${pkgs.busybox}/bin/mount -t devpts devpts /dev/pts

  ${pkgs.busybox}/bin/mkdir -p /tmp
  echo "Mounting tmpfs at /tmp"
  ${pkgs.busybox}/bin/mount -t tmpfs tmpfs /tmp

  # ============================================
  # Part 2: Load Kernel Modules
  # ============================================

  # Debug: Check module directory structure
  echo "NMBL: Checking for kernel modules..."
  KERNEL_VERSION=$(${pkgs.busybox}/bin/uname -r)
  echo "NMBL: Running kernel version: $KERNEL_VERSION"

  if [ -d /lib/modules ]; then
    echo "NMBL: /lib/modules exists"
    echo "NMBL: Available kernel module directories:"
    ${pkgs.busybox}/bin/ls -la /lib/modules/ || true

    # Check if we have modules for the running kernel
    if [ -d "/lib/modules/$KERNEL_VERSION" ]; then
      echo "NMBL: ✓ Found modules for running kernel: $KERNEL_VERSION"
      echo "NMBL: Module directory contents:"
      ${pkgs.busybox}/bin/ls -la "/lib/modules/$KERNEL_VERSION/" | ${pkgs.busybox}/bin/head -20 || true
    else
      echo "NMBL: ✗ WARNING: No modules found for running kernel $KERNEL_VERSION"
      echo "NMBL: Available versions:"
      for kver in /lib/modules/*; do
        if [ -d "$kver" ]; then
          echo "NMBL:   - $(${pkgs.busybox}/bin/basename $kver)"
        fi
      done
    fi
  else
    echo "NMBL: ✗ WARNING: /lib/modules does NOT exist!"
  fi

  # Generate module dependencies if modules exist
  if [ -d "/lib/modules/$KERNEL_VERSION" ]; then
    echo "NMBL: Generating module dependencies with depmod..."
    if depmod -a 2>&1; then
      echo "NMBL: ✓ Module dependencies generated successfully"
    else
      echo "NMBL: ✗ WARNING: depmod failed (modules may not load correctly)"
    fi
  else
    echo "NMBL: Skipping depmod (no modules for running kernel)"
  fi

  # Load kernel modules needed for storage and filesystems
  echo "NMBL: Loading required kernel modules..."
  ${lib.concatMapStringsSep "\n" (mod: ''
    echo -n "NMBL: Loading ${mod}... "
    if modprobe ${mod} 2>&1; then
      echo "✓"
    else
      # Module might already be built-in or loaded
      if ${pkgs.busybox}/bin/grep -q "^${mod} " /proc/modules 2>/dev/null; then
        echo "✓ (already loaded)"
      elif ${pkgs.busybox}/bin/test -e "/sys/module/${mod}" 2>/dev/null; then
        echo "✓ (built-in)"
      else
        echo "✗ FAILED"
      fi
    fi
  '') kernelModules}

  # Wait for devices to settle after loading modules
  echo "Waiting for devices to settle..."
  ${pkgs.busybox}/bin/sleep 2

  # ============================================
  # Part 3: Mount System Filesystems
  # ============================================

  echo "Mounting system filesystems..."
  echo "Available block devices:"
  ${pkgs.busybox}/bin/ls -la /dev/sd* /dev/vd* /dev/hd* /dev/nvme* 2>/dev/null || echo "  (no block devices found yet)"

  # Mount each filesystem needed for boot
  ${lib.concatMapStringsSep "\n" (fs: ''
    echo ""
    echo "Mounting: ${fs.mountPoint} -> ${cfg.mountPrefix}${fs.mountPoint}"
    echo "  Device: ${fs.device}"
    echo "  Type: ${fs.fsType}"
    echo "  Options: ${fs.options}"

    # Create mount point
    ${pkgs.busybox}/bin/mkdir -p "${cfg.mountPrefix}${fs.mountPoint}"

    # Attempt to mount
    if ${pkgs.busybox}/bin/mount -t ${fs.fsType} -o ${fs.options} ${fs.device} "${cfg.mountPrefix}${fs.mountPoint}"; then
      echo "  ✓ Successfully mounted ${fs.mountPoint}"
    else
      echo "  ✗ ERROR: Failed to mount ${fs.device} at ${cfg.mountPrefix}${fs.mountPoint}"
      echo "  This filesystem is required for boot!"
      exit 1
    fi
  '') fsList}

  echo ""
  echo "All filesystems mounted successfully!"
  echo "Mount summary:"
  ${pkgs.busybox}/bin/mount | ${pkgs.busybox}/bin/grep "^/" || true
''
