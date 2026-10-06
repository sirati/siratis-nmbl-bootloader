# Rescue /init fragment: put the separately pinned tools image on PATH.
#
# NMBL verifies the tools image (`nmblctl` and its closure; see
# ./tools-image.nix) and mounts it read-only at /nmbl-tools before /init
# starts, or leaves /etc/nmbl-tools-disabled saying why it refused it. The
# image's store paths are linked into this rescue's /nix/store (its binaries
# reference them by absolute path) and its bin/ into /bin, which every
# rescue shell, console or SSH, has on PATH. Paths the stage-2 store already
# holds are left alone: a store path names its content.
{ coreutils }:
''
    # --- NMBL tools (nmblctl) from the pinned tools image ---
    if [ -e /etc/nmbl-tools-disabled ]; then
      log "WARNING: NMBL refused the rescue tools image; nmblctl is unavailable: $(${coreutils}/bin/cat /etc/nmbl-tools-disabled)"
    elif [ -d /nmbl-tools/nix/store ]; then
      for path in /nmbl-tools/nix/store/*; do
        name=''${path##*/}
        [ -e "/nix/store/$name" ] || [ -L "/nix/store/$name" ] \
          || ${coreutils}/bin/ln -s "$path" "/nix/store/$name" \
          || log "WARNING: could not link $name from the tools image"
      done
      for tool in /nmbl-tools/bin/*; do
        [ -e "$tool" ] || continue
        name=''${tool##*/}
        [ -e "/bin/$name" ] || [ -L "/bin/$name" ] \
          || ${coreutils}/bin/ln -s "$tool" "/bin/$name" \
          || log "WARNING: could not put $name on PATH"
      done
      log "rescue tools available: $(${coreutils}/bin/ls /nmbl-tools/bin | ${coreutils}/bin/tr '\n' ' ')"
    fi

''
