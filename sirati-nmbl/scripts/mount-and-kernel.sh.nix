# Mount and Kernel Module Loading Script
# Returns a shell script string for mounting filesystems and loading kernel modules

{
  lib,
  pkgs,
  cfg,
}:

''
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

  # Wait for devices to settle
  sleep 1

  # Mount the configured filesystems
  ${lib.concatStringsSep "\n" (
    lib.mapAttrsToList (mountPoint: fs: ''
      mkdir -p ${mountPoint}
      mount -t ${fs.fsType} -o ${lib.concatStringsSep "," fs.options} ${fs.device} ${mountPoint}
    '') cfg.fileSystems
  )}
''
