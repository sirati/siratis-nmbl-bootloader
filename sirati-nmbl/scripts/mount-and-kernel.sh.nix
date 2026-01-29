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

  # Format filesystem info for mounting
  formatFS = fs: {
    mountPoint = fs.mountPoint;
    device = if fs.device != null then fs.device else "/dev/disk/by-label/${escape fs.label}";
    fsType = fs.fsType;
    options = builtins.concatStringsSep "," (
      # Force read-only for bootloader safety
      lib.unique (fs.options ++ [ "ro" ])
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

  # ============================================
  # Part 2: Load Kernel Modules
  # ============================================

  # Debug: Check module directory structure
  echo "DEBUG: Checking for kernel modules..."
  if [ -d /lib/modules ]; then
    echo "DEBUG: /lib/modules exists"
    ${pkgs.busybox}/bin/ls -la /lib/modules/ || true
    for kver in /lib/modules/*; do
      if [ -d "$kver" ]; then
        echo "DEBUG: Found kernel version: $(${pkgs.busybox}/bin/basename $kver)"
        ${pkgs.busybox}/bin/ls -la "$kver/" | ${pkgs.busybox}/bin/head -20 || true
      fi
    done
  else
    echo "DEBUG: /lib/modules does NOT exist!"
  fi

  # Generate module dependencies if modules exist
  if [ -d /lib/modules ]; then
    echo "Generating module dependencies..."
    ${pkgs.kmod}/bin/depmod -a 2>&1 || echo "Warning: depmod failed (modules may not be available)"
  fi

  # Load kernel modules needed for storage and filesystems
  ${lib.concatMapStringsSep "\n" (mod: ''
    echo "Loading kernel module: ${mod}"
    ${pkgs.kmod}/bin/modprobe ${mod} 2>/dev/null || echo "Warning: Failed to load ${mod}"
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
