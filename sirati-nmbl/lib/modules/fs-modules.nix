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
    # Read-only image filesystems, typically loop-mounted from a file
    # (e.g. a /nix-only squashfs serving the target closure).
    "squashfs" = "squashfs";
    "erofs"    = "erofs";
    "iso9660"  = "isofs";
    "tmpfs"    = null;
    "proc"     = null;
    "sysfs"    = null;
    "devtmpfs" = null;
  };

  fsEntries = lib.attrValues config.fileSystems;
  fsTypes = map (fs: fs.fsType) fsEntries;

  # Look up each fsType; an unknown key returns null, so the two
  # filters below collapse both "no mapping" and "explicitly null"
  # entries into the same drop.
  rawModules = map (fst: fsTypeToModule.${fst} or null) fsTypes;

  fsModules = lib.unique (lib.filter (m: m != null) rawModules);

  # A filesystem entry is loop-backed (and so needs the loop driver, since
  # NMBL sets up the loop device itself — no losetup/udev) when its
  # `options` carry `loop`, or its `device` is a regular-file path rather
  # than a block device (anything not under /dev). NixOS's own stage-1
  # uses the same heuristic for `boot.initrd.extraUtilsCommands` losetup.
  isLoopBacked = fs:
    (lib.elem "loop" (fs.options or [ ]))
    || (
      fs.device != null
      && lib.isString fs.device
      && lib.hasPrefix "/" fs.device
      && !(lib.hasPrefix "/dev/" fs.device)
    );

  loopModules = lib.optional (lib.any isLoopBacked fsEntries) "loop";

  baseModules = lib.unique (fsModules ++ loopModules);

  # Crypto helpers that the filesystem driver needs at mount(2) time.
  # Modern NixOS kernels build crypto as separate modules and require
  # them to be loaded explicitly when an fs driver requests an algo
  # through the kernel crypto API. modules.dep does not link these
  # because the relationship is runtime, not symbol-level — so our
  # dep walker won't pull them in unless we list them here.
  cryptoForFs = {
    # ext4 uses crc32c for metadata checksums (default-on since e2fsprogs 1.43).
    # Without it, `mount(2)` returns ENOENT with kernel printk
    # "EXT4-fs: Cannot load crc32c driver".
    "ext4" = [ "crc32c_generic" ];
  };

  cryptoModules = lib.unique (
    lib.concatMap (mod: cryptoForFs.${mod} or [ ]) baseModules
  );
in
baseModules ++ cryptoModules
