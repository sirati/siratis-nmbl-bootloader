# Derive kernel-module names from filesystem types.
#
# NixOS's normal stage-1 has udev, which auto-loads filesystem driver
# modules on `mount(2)` via the `fs-<fsType>` modalias. NMBL has no
# udev, so the driver `.ko` for each mounted filesystem must appear in
# the explicit-load list, otherwise `mount(2)` fails with `ENODEV`.
#
# Returns a unique list of module names derived from
# `config.fileSystems.*.fsType`, filtering out built-in / pseudo
# filesystems (mapping value `null`) and unknown fsTypes (silently
# ignored — surfaced via `boot.initrd.supportedFilesystems` validation
# in upstream NixOS modules).
#
# Used as a pure function: callers `import` this file and apply it
# with `{ lib, config }` to get the derived list, which is then unioned
# into `boot.nmbl.explicitKernelModules`.

{ lib, config }:

let
  # Map filesystem types to their kernel module names.
  # Names must match the `.ko` filename exactly (no extension), since
  # the runtime calls `init_module(2)` with these strings.
  #
  # Conventions match upstream NixOS stage-1:
  #   - ext2/ext3 share the `ext4.ko` driver in modern kernels.
  #   - `fat` is an alias for `vfat`.
  #   - `ntfs` here means the in-tree `ntfs3.ko` (read/write), not the
  #     legacy read-only `ntfs.ko` nor the FUSE `ntfs-3g` userspace
  #     driver. Configurations needing the old driver can set
  #     `boot.nmbl.kernelModules` explicitly.
  #   - Pseudo filesystems (`tmpfs`, `proc`, `sysfs`, `devtmpfs`) are
  #     compiled in and need no module load.
  fsTypeToModule = {
    "ext2"     = "ext4";
    "ext3"     = "ext4";
    "ext4"     = "ext4";
    "btrfs"    = "btrfs";
    "xfs"      = "xfs";
    "f2fs"     = "f2fs";
    "vfat"     = "vfat";
    "fat"      = "vfat";
    "msdos"    = "msdos";
    "ntfs"     = "ntfs3";
    "exfat"    = "exfat";
    "zfs"      = "zfs";
    "tmpfs"    = null;
    "proc"     = null;
    "sysfs"    = null;
    "devtmpfs" = null;
  };

  fsTypes = map (fs: fs.fsType) (lib.attrValues config.fileSystems);

  # Look up each fsType; an unknown key returns null, so the two
  # filters below collapse both "no mapping" and "explicitly null"
  # entries into the same drop.
  rawModules = map (fst: fsTypeToModule.${fst} or null) fsTypes;
in
lib.unique (lib.filter (m: m != null) rawModules)
