{
  pkgs,
  lib,
  rootPaths,
  label ? "NMBL-NIX",
}:

let
  toplevel = lib.throwIf (rootPaths == [ ])
    "generation-image requires at least one NixOS system toplevel"
    (builtins.head rootPaths);
  closure = pkgs.closureInfo { inherit rootPaths; };
in
pkgs.runCommand "nmbl-generation.erofs" {
  nativeBuildInputs = [ pkgs.erofs-utils pkgs.gnutar ];
  __structuredAttrs = true;
  unsafeDiscardReferences.out = true;
} ''
  # The placeholder avoids the /nix/ store-path transform rewriting metadata.
  mkdir -p profile/var/NMBLPROFILES
  ln -s ${toplevel} profile/var/NMBLPROFILES/system-1-link
  ln -s system-1-link profile/var/NMBLPROFILES/system
  tar --create \
    --absolute-names \
    --verbatim-files-from \
    --transform 'flags=rSh;s|/nix/|/|' \
    --transform 'flags=rSh;s|~nix~case~hack~[[:digit:]]\+||g' \
    --transform 'flags=rSh;s|NMBLPROFILES|nix/profiles|' \
    --files-from ${closure}/store-paths \
    --directory profile . \
  | mkfs.erofs \
      --quiet \
      --force-uid=0 \
      --force-gid=0 \
      -L ${lib.escapeShellArg label} \
      -U eb176051-bd15-49b7-9e6b-462e0b467019 \
      -T 0 \
      --hard-dereference \
      --tar=f \
      "$out"
''
