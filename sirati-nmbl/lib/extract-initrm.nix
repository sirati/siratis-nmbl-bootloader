# Shared shell snippet that extracts a (possibly-compressed) NMBL initramfs
# cpio into a directory and reshapes it into a closure root the Rust
# `nmbl-init --validate-initrm` dry-run can probe, WITHOUT a privileged
# chroot/pivot_root. Used by BOTH the build-time gate (lib/config.nix
# `nmblInitrmCheck`) and the install-time gate (lib/install-bootloader.nix)
# so the two validate against an identically-shaped closure.
#
# Imported as a function applied with `{ pkgs }`; returns a string defining a
# shell function `nmbl_extract_initrm <initrd-file> <dest-dir>`. The dest dir
# must already exist. The snippet uses only the tools it interpolates from the
# store, so it works both inside the nix sandbox and on a live installer.
#
# Two reshaping steps make the validator's prefix-graft `ClosureView` resolve
# the same paths it would at boot:
#   1. The initramfs stages /init, /bin/*, /lib/modules as ABSOLUTE symlinks
#      into /nix/store and bundles that store closure inside the cpio. At boot
#      the initrd IS `/`, so the absolute symlinks resolve; under an extracted
#      root they escape it. We rewrite the STAGED boot symlinks (those OUTSIDE
#      the bundled nix/store) to relative form pointing at the bundled copy.
#   2. The module presence walk locates `/lib/modules/<release>/modules.dep`
#      where <release> comes from `uname(2)`. In a build sandbox (or an
#      installer whose running kernel differs from the NMBL target) `uname`
#      reports a release the initramfs does not ship a tree for, so the walk
#      would falsely report the tree absent. We alias the running release to
#      the single modules tree actually shipped, so the walk presence-checks
#      the SAME .ko set under the name the local `uname` reports.

{ pkgs }:

''
  nmbl_extract_initrm() {
    _nmbl_initrd="$1"
    _nmbl_dest="$2"

    # Decompress by magic (gzip / zstd / xz), then unpack the cpio.
    _nmbl_magic=$(${pkgs.coreutils}/bin/od -An -tx1 -N6 "$_nmbl_initrd" | ${pkgs.coreutils}/bin/tr -d ' \n')
    case "$_nmbl_magic" in
      1f8b*)         _nmbl_decomp="${pkgs.gzip}/bin/gzip -dc" ;;
      28b52ffd*)     _nmbl_decomp="${pkgs.zstd}/bin/zstd -dc" ;;
      fd377a585a00*) _nmbl_decomp="${pkgs.xz}/bin/xz -dc" ;;
      *)             _nmbl_decomp="${pkgs.coreutils}/bin/cat" ;;
    esac
    ( cd "$_nmbl_dest" && $_nmbl_decomp < "$_nmbl_initrd" | ${pkgs.cpio}/bin/cpio -idm --quiet )

    # 1. Rewrite staged boot symlinks (outside the bundled store) to relative.
    ${pkgs.coreutils}/bin/chmod -R u+w "$_nmbl_dest"/bin "$_nmbl_dest"/lib "$_nmbl_dest"/etc 2>/dev/null || true
    while IFS= read -r -d "" _nmbl_link; do
      case "$_nmbl_link" in
        "$_nmbl_dest"/nix/store/*) continue ;;
      esac
      _nmbl_target=$(${pkgs.coreutils}/bin/readlink "$_nmbl_link")
      case "$_nmbl_target" in
        /*) _nmbl_rel=$(${pkgs.coreutils}/bin/realpath -m --relative-to="$(${pkgs.coreutils}/bin/dirname "$_nmbl_link")" "$_nmbl_dest$_nmbl_target")
            ${pkgs.coreutils}/bin/ln -sfn "$_nmbl_rel" "$_nmbl_link" ;;
      esac
    done < <(${pkgs.findutils}/bin/find "$_nmbl_dest" -path "$_nmbl_dest/nix/store" -prune -o -type l -print0)

    # 2. Alias the running kernel release to the single shipped modules tree.
    _nmbl_modroot=$(${pkgs.coreutils}/bin/realpath -m "$_nmbl_dest/lib/modules" 2>/dev/null || true)
    if [ -d "$_nmbl_modroot" ]; then
      _nmbl_shipped=$(${pkgs.findutils}/bin/find "$_nmbl_modroot" -mindepth 1 -maxdepth 1 -type d -printf '%f\n' | (read -r f && echo "$f"))
      _nmbl_host=$(${pkgs.coreutils}/bin/uname -r)
      if [ -n "$_nmbl_shipped" ] && [ "$_nmbl_shipped" != "$_nmbl_host" ] && [ ! -e "$_nmbl_modroot/$_nmbl_host" ]; then
        ${pkgs.coreutils}/bin/chmod u+w "$_nmbl_modroot"
        ${pkgs.coreutils}/bin/ln -sfn "$_nmbl_shipped" "$_nmbl_modroot/$_nmbl_host"
      fi
    fi
  }
''
