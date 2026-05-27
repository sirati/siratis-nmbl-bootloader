# Builds the external NMBL rescue squashfs blob.
#
# When `boot.nmbl.rescue.mode = "external"`, the initramfs no longer
# carries busybox + storage activation binaries; instead, those tools
# are bundled into a single read-only squashfs (`nmbl-rescue.sfs`)
# staged on the boot partition by install-bootloader.nix. The Rust
# /init loop-mounts the blob and switch_roots into it (MS_MOVE + chroot)
# when the emergency shell is requested.
#
# Used as a pure function over `cfg.rescue.squashfsContents`: callers
# `import` this file and apply it with `{ pkgs, lib, contents }` to
# get a derivation containing the rendered squashfs.

{
  pkgs,
  lib,
  contents,
}:

let
  # `buildEnv` aggregates `bin/`, `sbin/`, `lib/`, `share/`,
  # resolves symlink conflicts, and follows the standard FHS layout
  # — exactly what the rescue squashfs needs. The resulting tree's
  # leaves are symlinks pointing into the build-host nix store, which
  # we resolve at squashfs-build time so the blob is self-contained
  # (the boot environment has no /nix/store).
  rescueRoot = pkgs.buildEnv {
    name = "nmbl-rescue-root";
    paths = contents;
    pathsToLink = [ "/bin" "/sbin" "/lib" "/libexec" "/share" "/etc" ];
  };
in
pkgs.runCommand "nmbl-rescue.sfs"
  {
    nativeBuildInputs = [ pkgs.squashfsTools ];
  }
  ''
    # Stage the buildEnv tree into a writable copy with `cp -aL` so
    # symlinks pointing into the build-host /nix/store are dereferenced
    # into real files. Busybox-style applet aliases (sh, ls, …) all
    # resolve to the single busybox binary, producing a tree that
    # *looks* repetitive but compresses cheaply: mksquashfs hashes
    # file contents and stores each unique inode once, so the on-disk
    # blob stays small even though the staging tree is large.
    mkdir -p root
    cp -aL ${rescueRoot}/. root/
    # `cp -aL` preserves source permissions which are read-only in a Nix
    # buildEnv. Relax them so subsequent operations (mksquashfs) can
    # read the staging tree without permission errors.
    chmod -R u+w root/

    # `-comp zstd -Xcompression-level 19` matches the plan: best ratio
    # at build time, page-by-page decompression at runtime so the boot
    # cost is amortised. `-noappend` makes the build deterministic by
    # refusing to extend an existing image, and `-all-root` makes every
    # entry uid=0 gid=0 (mksquashfs has no choice in a sandbox anyway,
    # but stating it explicitly avoids host-uid leakage).
    mksquashfs root "$out" \
      -comp zstd -Xcompression-level 19 \
      -noappend \
      -all-root \
      -no-progress
  ''
