# The rescue tools image: `nmblctl` and its runtime closure as a small EROFS.
#
# `nmblctl` is built with this host's signing public keys baked in, so it is
# kept out of the host-independent stage-2 image. NMBL's config pins this
# image by SHA-512 (`[rescue.tools]`); NMBL verifies it over one descriptor,
# mounts it at /nmbl-tools, and the rescue /init links its store paths and
# /nmbl-tools/bin into the rescue (see ./init-tools.nix). Built only from
# `nmblctl`'s closure, so it is small and rebuilds without stage 2.
{
  pkgs,
  nmblCtl,
  # mkfs.erofs compressor; the same one as the stage-2 image, which the NMBL
  # kernel already has to decompress.
  compression ? "lz4hc",
}:

let
  closure = pkgs.closureInfo { rootPaths = [ nmblCtl ]; };
  compressionFlags = import ./erofs-compression.nix compression;
in
pkgs.runCommand "nmbl-rescue-tools.erofs" { nativeBuildInputs = [ pkgs.erofs-utils ]; } ''
  mkdir -p root/nix/store root/bin
  while read -r p; do
    cp -a "$p" root/nix/store/
  done < ${closure}/store-paths
  # Relative links, so /nmbl-tools/bin resolves inside the image itself.
  for tool in ${nmblCtl}/bin/*; do
    name=$(basename "$tool")
    ln -s "../nix/store/$(basename ${nmblCtl})/bin/$name" "root/bin/$name"
  done
  # Reproducible like stage 2: root-owned, fixed timestamps and UUID,
  # single-threaded compression.
  mkfs.erofs \
    --quiet \
    --workers=1 \
    --force-uid=0 \
    --force-gid=0 \
    -T 0 \
    -U 6e6d626c-746f-6f6c-7300-000000000001 \
    -L nmbl-tools \
    ${pkgs.lib.escapeShellArgs compressionFlags} \
    "$out" root
''
