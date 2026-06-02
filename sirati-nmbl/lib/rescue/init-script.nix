# Rescue /init — PART A: PID-1 entrypoint setup (header/exports, pseudo-filesystems,
# kernel modules + firmware, the ext4 scratch backing, and the writable overlays).
# Continues in ./init-script-net.nix (networking, ssh, nix-daemon, sshd, console).
#
# Split out of lib/rescue-sfs.nix per FIX-19. Returns the shell-string FRAGMENT
# only — the orchestrator concatenates it with the net fragment and feeds the
# result to a single `pkgs.writeShellScript "nmbl-rescue-init"`, so the rendered
# /init is byte-identical to the pre-split body. This fragment carries the
# `${rescueModprobes}` line at column 0, which anchors the indented-string dedent
# to zero so every other line keeps its literal indentation.
{
  bash,
  cacert,
  coreutils,
  e2fsprogs,
  gawk,
  gnugrep,
  gnused,
  nix,
  openssh,
  utilLinux,
  rescueModprobes,
}:
''
    #!${bash}/bin/bash
    # NMBL full recovery system — PID 1 entrypoint.
    export PATH=/run/current-system/sw/bin:/bin:/sbin:/usr/bin:/usr/sbin:${nix}/bin:${openssh}/bin
    export HOME=/root
    export TERM=''${TERM:-linux}
    # TLS trust store so nix-daemon (and the interactive shell) can reach
    # cache.nixos.org over HTTPS. Exported before nix-daemon launches so
    # the daemon inherits it.
    export NIX_SSL_CERT_FILE=${cacert}/etc/ssl/certs/ca-bundle.crt
    export SSL_CERT_FILE=${cacert}/etc/ssl/certs/ca-bundle.crt
    # Resolve <nixpkgs> on the console shell (and for nix-daemon's env) to
    # the pinned nixpkgs fetched on demand — matches nix.conf nix-path and
    # the registry pin. Not baked into the squashfs (ESP size).
    export NIX_PATH=nixpkgs=flake:nixpkgs
    # NMBL stays PID 1 OUTSIDE this chroot and bind-mounts its own root into
    # the chroot at /nmbl-root, so its root-only TUI control socket is visible
    # here at /nmbl-root/nmbl-run/tui.sock. Export the path so the console
    # shell (and anything it spawns) can attach to NMBL's TUI; the matching
    # /bin/nmbl-tui shim (see fullSquashfs) points at NMBL's own static binary
    # (/nmbl-root/init) which auto-detects getpid()!=1 → client mode and honours
    # NMBL_TUI_SOCK as the socket path override.
    export NMBL_TUI_SOCK=/nmbl-root/nmbl-run/tui.sock

    log() { echo "[nmbl-rescue] $*" > /dev/console 2>&1 || true; }

    log "starting recovery system"

    # --- pseudo-filesystems ---
    ${coreutils}/bin/mkdir -p /proc /sys /dev /dev/pts /run /tmp
    ${utilLinux}/bin/mount -t proc     proc     /proc    2>/dev/null || true
    ${utilLinux}/bin/mount -t sysfs    sysfs    /sys     2>/dev/null || true
    ${utilLinux}/bin/mount -t devtmpfs devtmpfs /dev     2>/dev/null || true
    ${coreutils}/bin/mkdir -p /dev/pts
    ${utilLinux}/bin/mount -t devpts   devpts   /dev/pts 2>/dev/null || true
    ${utilLinux}/bin/mount -t tmpfs    tmpfs    /run     2>/dev/null || true
    ${utilLinux}/bin/mount -t tmpfs    tmpfs    /tmp     2>/dev/null || true

    # --- kernel modules (loaded by THE RESCUE, not NMBL) ---
    # NMBL no longer preloads the rescue's drivers; it only loads loop +
    # squashfs (on demand) so it can loop-mount this blob. Everything the
    # recovery system needs — overlay + ext4 (the writable scratch),
    # af_packet (dhcpcd's BPF socket) and the NIC drivers — is shipped in
    # this squashfs at /lib/modules/$(uname -r) and modprobe'd HERE. Done
    # before the ext4 scratch / overlays / networking below, which depend
    # on these modules. /sys is mounted above, so firmware_class exists.
    #
    # Point the kernel's firmware loader at the squashfs /lib/firmware
    # FIRST, so a NIC driver that requests firmware at modprobe time (wifi)
    # finds its blob. Best-effort: the sysfs knob is absent if
    # CONFIG_FW_LOADER_USER_HELPER is off, but the in-kernel loader also
    # searches /lib/firmware by default, so the override is belt-and-braces.
    log "pointing firmware loader at /lib/firmware"
    if [ -w /sys/module/firmware_class/parameters/path ]; then
      ${coreutils}/bin/printf '%s' /lib/firmware \
        > /sys/module/firmware_class/parameters/path 2>/dev/null \
        || log "WARNING: could not set firmware_class search path"
    fi
    log "loading rescue kernel modules (overlay, ext4, af_packet, NIC drivers)"
${rescueModprobes}

    # --- ext4 scratch backing the overlay upper/work dirs ---
    # The squashfs root (/, /nix, /etc, /var) is read-only, so the scratch
    # mountpoint and the overlay upper/work dirs cannot live there. The overlay
    # upper/work dirs all anchor under /run/scratch, which is backed in one of
    # two ways:
    #
    #   1. A DEDICATED SCRATCH DISK (the test VM, RAM-constrained): the
    #      orchestrator attaches a blank NVMe whose serial is exactly
    #      NMBLSCRATCH. We detect it by serial, mkfs.ext4 it, and mount it at
    #      /run/scratch. This decouples scratch size from RAM, so the test VM
    #      can stay at the BIOS-safe 2048 MB yet still unpack the full nixpkgs
    #      source (~1.5GB+) that `nix-shell -p` writes.
    #
    #   2. A RAM-BACKED TMPFS IMAGE (prod / no scratch disk): the historic
    #      path. We format a sparse ext4 image on the /run tmpfs and loop-mount
    #      it. overlayfs rejects a tmpfs upperdir on linux_6_6 (no
    #      trusted.overlay.* xattr support until ~6.11), hence the ext4-on-loop
    #      indirection. Prod servers have ample RAM, so the RAM scratch is fine.
    #
    # SAFETY: only ever mkfs/mount the device whose serial is EXACTLY
    # NMBLSCRATCH. The recovery-target disks (the btrfs RAID nvme0/nvme1) MUST
    # NEVER be formatted. If no NMBLSCRATCH device is found, we do NOT guess —
    # we fall back to path (2). The ext4 + overlay kernel modules were
    # modprobe'd by THIS /init above (shipped in the squashfs, not by NMBL);
    # every step degrades to a WARNING rather than aborting.
    ${coreutils}/bin/mkdir -p /run/scratch \
      || log "WARNING: mkdir /run/scratch failed"

    # Locate a dedicated scratch block device by its serial NMBLSCRATCH.
    #
    # This minimal env has NO udev, so /dev/disk/by-id/* symlinks are NOT
    # populated -- we must NOT rely on them. We scan sysfs directly. The serial
    # of an NVMe NAMESPACE (e.g. nvme2n1) is NOT exposed at
    # /sys/block/nvme2n1/serial; it lives on the CONTROLLER, reachable via the
    # block device's "device" symlink (/sys/block/<dev>/device/serial) and/or
    # /sys/class/nvme/<ctrl>/serial. virtio/scsi expose it at
    # /sys/block/<dev>/device/serial. NVMe serials are space-padded, so we trim
    # whitespace/newlines before comparing.
    #
    # read_block_serial <basename>: echo the trimmed serial of /sys/block/<dev>
    # using the first readable of device/serial, serial, or (for nvme) the
    # controller's /sys/class/nvme/<ctrl>/serial. Empty if none readable.
    read_block_serial() {
      _bn="$1"
      _ser=""
      for _sf in "/sys/block/$_bn/device/serial" "/sys/block/$_bn/serial"; do
        if [ -r "$_sf" ]; then
          _ser=$(${coreutils}/bin/cat "$_sf" 2>/dev/null)
          [ -n "$_ser" ] && break
        fi
      done
      if [ -z "$_ser" ]; then
        # nvme namespace nvme<N>n<M> -> controller nvme<N>: drop the n<M> suffix.
        case "$_bn" in
          nvme*n*)
            _ctrl="''${_bn%n*}"
            if [ -r "/sys/class/nvme/$_ctrl/serial" ]; then
              _ser=$(${coreutils}/bin/cat "/sys/class/nvme/$_ctrl/serial" 2>/dev/null)
            fi
            ;;
        esac
      fi
      # Trim leading/trailing whitespace and strip all newlines.
      ${coreutils}/bin/printf '%s' "$_ser" \
        | ${coreutils}/bin/tr -d '\n' \
        | ${gnused}/bin/sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//'
    }

    scratch_dev=""
    for blk in /sys/block/nvme*n1 /sys/block/vd* /sys/block/sd*; do
      [ -e "$blk" ] || continue
      bn=$(${coreutils}/bin/basename "$blk")
      ser=$(read_block_serial "$bn")
      log "block /dev/$bn serial='$ser'"
      case "$ser" in
        *NMBLSCRATCH*)
          cand="/dev/$bn"
          if [ -b "$cand" ]; then scratch_dev="$cand"; break; fi
          ;;
      esac
    done

    scratch_ready=""
    if [ -n "$scratch_dev" ]; then
      # SAFETY GATE: re-read the resolved device's serial via the same corrected
      # sysfs path and confirm it contains NMBLSCRATCH before touching it. The
      # recovery-target disks (the btrfs RAID nvme0/nvme1) MUST NEVER be
      # formatted, so we never mkfs a device whose serial does not match.
      devname=$(${coreutils}/bin/basename "$scratch_dev")
      devserial=$(read_block_serial "$devname")
      case "$devserial" in
        *NMBLSCRATCH*)
          sz=$(${coreutils}/bin/cat "/sys/block/$devname/size" 2>/dev/null || echo 0)
          sz_mib=$(( sz / 2048 ))
          log "found dedicated scratch disk $scratch_dev (serial NMBLSCRATCH, ~''${sz_mib} MiB); formatting ext4"
          if ${e2fsprogs}/bin/mkfs.ext4 -q -F -O '^has_journal' "$scratch_dev"; then
            if ${utilLinux}/bin/mount "$scratch_dev" /run/scratch; then
              log "scratch backing: DISK $scratch_dev (serial NMBLSCRATCH) mounted at /run/scratch"
              scratch_ready=1
            else
              log "WARNING: mount of scratch disk $scratch_dev failed; falling back to RAM scratch"
            fi
          else
            log "WARNING: mkfs.ext4 on scratch disk $scratch_dev failed; falling back to RAM scratch"
          fi
          ;;
        *)
          log "WARNING: device $scratch_dev serial mismatch ('$devserial' lacks NMBLSCRATCH); NOT formatting; falling back to RAM scratch"
          ;;
      esac
    else
      log "no NMBLSCRATCH disk found; using RAM-backed tmpfs scratch"
    fi

    if [ -z "$scratch_ready" ]; then
      # RAM-backed fallback. Size the sparse ext4 image from available RAM: it
      # lives on the /run tmpfs, so its real ceiling is RAM, not the nominal
      # size. Use ~70% of MemTotal (kB from /proc/meminfo), floored at 1 GiB,
      # leaving headroom for the OS and nix-daemon. ext4 sees the full nominal
      # device; tmpfs only backs pages actually written.
      memtotal_kb=$(${gnugrep}/bin/grep -m1 '^MemTotal:' /proc/meminfo \
        | ${gawk}/bin/awk '{print $2}')
      [ -n "$memtotal_kb" ] || memtotal_kb=0
      scratch_kb=$(( memtotal_kb * 70 / 100 ))
      if [ "$scratch_kb" -lt 1048576 ]; then
        scratch_kb=1048576
      fi
      log "scratch backing: RAM tmpfs; sizing ext4 image MemTotal=''${memtotal_kb}kB scratch=''${scratch_kb}kB (~70% RAM, min 1 GiB)"
      ${coreutils}/bin/truncate -s "''${scratch_kb}K" /run/scratch.img \
        || log "WARNING: truncate /run/scratch.img failed"
      ${e2fsprogs}/bin/mkfs.ext4 -q -F -O '^has_journal' /run/scratch.img \
        || log "WARNING: mkfs.ext4 on /run/scratch.img failed"
      ${utilLinux}/bin/mount -o loop /run/scratch.img /run/scratch \
        || {
          log "WARNING: mount -o loop failed; trying explicit losetup"
          loopdev=$(${utilLinux}/bin/losetup -f --show /run/scratch.img) \
            && ${utilLinux}/bin/mount "$loopdev" /run/scratch \
            || log "WARNING: ext4 scratch unavailable; overlays will fail"
        }
    fi

    # --- writable overlays, upper/work on the ext4 scratch ---
    ${coreutils}/bin/mkdir -p /run/scratch/ovl/store/u /run/scratch/ovl/store/w \
                             /run/scratch/ovl/var/u   /run/scratch/ovl/var/w \
                             /run/scratch/ovl/etc/u   /run/scratch/ovl/etc/w \
                             /run/scratch/ovl/var2/u  /run/scratch/ovl/var2/w \
                             /run/scratch/ovl/root/u  /run/scratch/ovl/root/w

    # nix-shell -p must be able to realise derivations, so layer a writable
    # upper over the ro squashfs store.
    log "mounting writable overlay over /nix/store"
    ${utilLinux}/bin/mount -t overlay overlay \
      -o lowerdir=/nix/store,upperdir=/run/scratch/ovl/store/u,workdir=/run/scratch/ovl/store/w \
      /nix/store \
      || log "WARNING: overlay over /nix/store failed; store stays read-only"

    # The seeded nix DB lives at /nix/var/nix/db on the ro squashfs.
    # Overlay /nix/var keeping the seeded db.sqlite visible while letting
    # nix-daemon register substituted/built paths (copy-up on write).
    log "mounting writable overlay over /nix/var"
    ${utilLinux}/bin/mount -t overlay overlay \
      -o lowerdir=/nix/var,upperdir=/run/scratch/ovl/var/u,workdir=/run/scratch/ovl/var/w \
      /nix/var \
      || log "WARNING: overlay over /nix/var failed; nix db stays read-only"
    ${coreutils}/bin/mkdir -p /nix/var/nix/{daemon-socket,gcroots,profiles,temproots,userpool} /nix/var/log/nix

    # The baked /etc (sshd_config, nix.conf, passwd, ssl symlinks) is on
    # the ro squashfs. Overlay it so dhcpcd can write /etc/resolv.conf and
    # ssh-keygen can write host keys, while every baked file stays visible.
    log "mounting writable overlay over /etc"
    ${utilLinux}/bin/mount -t overlay overlay \
      -o lowerdir=/etc,upperdir=/run/scratch/ovl/etc/u,workdir=/run/scratch/ovl/etc/w \
      /etc \
      || log "WARNING: overlay over /etc failed; /etc stays read-only"

    # The baked /var (notably /var/empty for sshd privsep) is on the ro
    # squashfs. Overlay it so dhcpcd (/var/db) and nix-daemon (/var/log)
    # have writable paths while the baked /var/empty stays visible.
    log "mounting writable overlay over /var"
    ${utilLinux}/bin/mount -t overlay overlay \
      -o lowerdir=/var,upperdir=/run/scratch/ovl/var2/u,workdir=/run/scratch/ovl/var2/w \
      /var \
      || log "WARNING: overlay over /var failed; /var stays read-only"
    ${coreutils}/bin/mkdir -p /var/db /var/log

    # The baked /root (notably /root/.ssh/authorized_keys and /root/.bashrc)
    # is on the ro squashfs, so nix's eval/flake cache (HOME=/root →
    # ~/.cache/nix) cannot be created. Overlay /root so it becomes writable
    # while the baked files stay visible; overlay exposes the lower's mode,
    # so the baked 0700 /root/.ssh and 0600 authorized_keys perms are kept.
    log "mounting writable overlay over /root"
    ${utilLinux}/bin/mount -t overlay overlay \
      -o lowerdir=/root,upperdir=/run/scratch/ovl/root/u,workdir=/run/scratch/ovl/root/w \
      /root \
      || log "WARNING: overlay over /root failed; /root stays read-only"

''
