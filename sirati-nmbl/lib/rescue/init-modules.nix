# Rescue /init fragment: load kernel modules named by NMBL at runtime.
#
# The stage-2 image is host-independent, so it bakes no module list and no
# decision about a networking stage. NMBL writes both into the rescue
# overlay before /init starts (see nmbl-init-rs src/rescue/host.rs):
# /etc/nmbl-rescue/modules (one name per line) and, when a signed
# networking stage is configured, /etc/nmbl-rescue/network-stage. Modules
# and firmware then come from /nmbl-network, otherwise from this image.
{ coreutils, kmod }:
''
    host_data=/etc/nmbl-rescue
    if [ -e "$host_data/network-stage" ]; then
      module_root=/nmbl-network
    else
      module_root=/
    fi
    firmware_path="''${module_root%/}/lib/firmware"
    log "pointing firmware loader at $firmware_path"
    if [ -w /sys/module/firmware_class/parameters/path ]; then
      ${coreutils}/bin/printf '%s' "$firmware_path" \
        > /sys/module/firmware_class/parameters/path 2>/dev/null \
        || log "WARNING: could not set firmware_class search path"
    fi
    if [ -r "$host_data/modules" ]; then
      log "loading rescue kernel modules named by NMBL"
      while read -r module; do
        [ -n "$module" ] || continue
        ${kmod}/bin/modprobe -d "$module_root" "$module" > /dev/console 2>&1 \
          || log "WARNING: modprobe $module failed"
      done < "$host_data/modules"
    else
      log "WARNING: NMBL handed over no module list"
    fi
''
