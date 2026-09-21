{
  bash,
  coreutils,
  kmod,
  utilLinux,
  rescueModprobes,
  networkStageEnabled,
}:
''
  #!${bash}/bin/bash
  export PATH=/bin:/sbin:/usr/bin:/usr/sbin
  export HOME=/root TERM=''${TERM:-linux}
  export NMBL_TUI_SOCK=/nmbl-root/nmbl-run/tui.sock
  log() { echo "[nmbl-rescue] $*" > /dev/console 2>&1 || true; }
  log "starting minimal recovery system"

  ${coreutils}/bin/mkdir -p /proc /sys /dev /dev/pts /run /tmp
  ${utilLinux}/bin/mount -t proc proc /proc 2>/dev/null || true
  ${utilLinux}/bin/mount -t sysfs sysfs /sys 2>/dev/null || true
  ${utilLinux}/bin/mount -t devtmpfs devtmpfs /dev 2>/dev/null || true
  ${coreutils}/bin/mkdir -p /dev/pts
  ${utilLinux}/bin/mount -t devpts devpts /dev/pts 2>/dev/null || true
  ${utilLinux}/bin/mount -t tmpfs tmpfs /run 2>/dev/null || true
  ${utilLinux}/bin/mount -t tmpfs tmpfs /tmp 2>/dev/null || true

  log "pointing firmware loader at ${if networkStageEnabled then "/nmbl-network/lib/firmware" else "/lib/firmware"}"
  if [ -w /sys/module/firmware_class/parameters/path ]; then
    ${coreutils}/bin/printf '%s' ${if networkStageEnabled then "/nmbl-network/lib/firmware" else "/lib/firmware"} \
      > /sys/module/firmware_class/parameters/path 2>/dev/null || true
  fi
${rescueModprobes}

  # Only configuration and daemon state need writes. Keep the store and all
  # recovery binaries read-only; tmpfs-backed overlays disappear on reboot.
  ${coreutils}/bin/mkdir -p /run/overlay
  for tree in etc var root; do
    ${coreutils}/bin/mkdir -p "/run/overlay/$tree/upper" "/run/overlay/$tree/work"
    ${utilLinux}/bin/mount -t overlay overlay \
      -o "lowerdir=/$tree,upperdir=/run/overlay/$tree/upper,workdir=/run/overlay/$tree/work" \
      "/$tree" || log "ERROR: writable /$tree overlay failed"
  done
  ${coreutils}/bin/mkdir -p /var/db /var/log /root/.ssh
''
