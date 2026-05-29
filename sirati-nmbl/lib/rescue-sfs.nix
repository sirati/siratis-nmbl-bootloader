# Builds the external NMBL rescue squashfs blob.
#
# When `boot.nmbl.rescue.mode = "external"`, the initramfs no longer
# carries busybox + storage activation binaries; instead, those tools
# are bundled into a single read-only squashfs (`nmbl-rescue.sfs`)
# staged on the boot partition by install-bootloader.nix. The Rust
# /init loop-mounts the blob and switch_roots into it (MS_MOVE + chroot)
# when the emergency shell is requested.
#
# Two image shapes are produced from this one function:
#
#   * Flat busybox tree (the default, `fullSystem.enable = false`):
#     a `buildEnv` + `cp -aL` FHS tree with NO /nix/store. The Rust
#     loader execs `/bin/sh` (busybox). This is the historic behaviour.
#
#   * Full recovery system (`fullSystem.enable = true`): a real
#     /nix/store + nix-db image with bash, btop, a root nix-daemon
#     (flakes on), and sshd. The Rust loader execs `/init` (a bash
#     script baked into the image) which brings up pseudo-filesystems,
#     an overlay'd writable store, networking, ssh host keys, the
#     nix-daemon and sshd, then drops to an interactive bash on the
#     console.
#
# Used as a pure function: callers `import` this file and apply it
# with `{ pkgs, lib, contents, fullSystem }` to get a derivation
# containing the rendered squashfs.

{
  pkgs,
  lib,
  contents,
  # Full-recovery-system parameters. `enable = false` keeps the flat
  # busybox path. The remaining fields are only consumed when enabled.
  fullSystem ? {
    enable = false;
    packages = [ ];
    sshdPort = 22222;
    rootAuthorizedKeys = [ ];
    nicDrivers = [ ];
  },
}:

let
  # The exact nixpkgs revision this flake is locked to. Read from the
  # adjacent flake.lock at eval time so it tracks the lock (reproducible,
  # no "latest unstable" drift) instead of being hand-copied. `<nixpkgs>`
  # in the rescue is pinned to this rev and fetched ON DEMAND from GitHub
  # — the source is never baked into the squashfs (it would overflow the
  # 256M ESP), but the rescue has working DHCP/internet so it resolves at
  # runtime.
  nixpkgsRev =
    let
      lock = builtins.fromJSON (builtins.readFile (../. + "/flake.lock"));
      topNixpkgs = lock.nodes.${lock.root}.inputs.nixpkgs;
    in
    lock.nodes.${topNixpkgs}.locked.rev;

  # ---- Flat busybox tree (legacy default) ---------------------------
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

  flatSquashfs = pkgs.runCommand "nmbl-rescue.sfs"
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
    '';

  # ---- Full recovery system (closure-store image) -------------------

  # Tools whose store paths we resolve to absolute /bin paths for the
  # /init script and the /bin shims. Pull them out of the package set so
  # the script does not depend on PATH being set up before it has set up
  # PATH (chicken/egg at PID 1).
  bash = pkgs.bashInteractive;
  coreutils = pkgs.coreutils-full;
  utilLinux = pkgs.util-linux;
  e2fsprogs = pkgs.e2fsprogs;
  iproute2 = pkgs.iproute2;
  dhcpcd = pkgs.dhcpcd;
  openssh = pkgs.openssh;
  kmod = pkgs.kmod;
  nix = pkgs.nixVersions.stable;
  procps = pkgs.procps;
  gnugrep = pkgs.gnugrep;
  gnused = pkgs.gnused;
  gawk = pkgs.gawk;
  btop = pkgs.btop;
  btrfs = pkgs.btrfs-progs;
  cryptsetup = pkgs.cryptsetup;
  cacert = pkgs.cacert;

  # NIC drivers the /init modprobes (a no-op if NMBL already loaded them
  # into its kernel before switch_root, which it does — see options.nix).
  nicModprobes = lib.concatMapStringsSep "\n"
    (m: "  modprobe ${m} 2>/dev/null || true")
    fullSystem.nicDrivers;

  # The rescue /init: PID 1 after switch_root. A bash script, baked into
  # the image at /init. References tools by absolute store path so it
  # never depends on a pre-existing PATH. Defensive: every step logs to
  # the console and tolerates failure so the operator still lands in a
  # shell even if (say) DHCP times out.
  initScript = pkgs.writeShellScript "nmbl-rescue-init" ''
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
    # we fall back to path (2). The ext4 + overlay kernel modules are loaded
    # into NMBL's kernel before switch_root (see options.nix / config.nix);
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

    # --- kernel modules (no-op if NMBL preloaded them) ---
    log "loading NIC drivers"
${nicModprobes}

    # --- networking ---
    # nixpkgs builds dhcpcd with --disable-privsep --dbdir=/var/lib/dhcpcd,
    # so there is no privsep user to provision; the failure mode is instead
    # a missing writable dbdir / run dir, or simply no link brought up. We
    # create both state dirs at the exact paths the binary was compiled with
    # and run dhcpcd with stdout+stderr on /dev/console so its real error is
    # captured in the serial log (the previous `2>/dev/null` swallowed it).
    log "bringing up loopback + DHCP"
    ${iproute2}/bin/ip link set lo up > /dev/console 2>&1 || true

    log "interfaces before bring-up:"
    ${iproute2}/bin/ip link > /dev/console 2>&1 || true

    # Enumerate /sys/class/net and bring every non-loopback link up. Log the
    # chosen interface name(s) so the next run shows exactly what was wired.
    ifaces=""
    for iface in $(${coreutils}/bin/ls /sys/class/net 2>/dev/null); do
      [ "$iface" = "lo" ] && continue
      log "bringing up NIC: $iface"
      ${iproute2}/bin/ip link set dev "$iface" up > /dev/console 2>&1 || true
      ifaces="$ifaces $iface"
    done
    if [ -z "$ifaces" ]; then
      log "WARNING: no non-loopback NIC found in /sys/class/net (virtio_net not loaded?)"
    fi

    # Wait for the kernel to report a REAL carrier before running dhcpcd.
    # /sys/class/net/<iface>/carrier reads 1 only once the link is up; reading
    # it too early (or while operstate is still "down") returns 0 or EINVAL,
    # so a naive "started => carrier" assumption is a false positive. Poll up
    # to ~20s (40 * 0.5s), logging the real carrier/operstate each second, and
    # only declare carrier when carrier==1. If it never comes up, WARN and
    # continue (degrade) — but only after the full wait.
    carrier_iface=""
    for i in $(${coreutils}/bin/seq 1 40); do
      for iface in $ifaces; do
        c=$(${coreutils}/bin/cat /sys/class/net/$iface/carrier 2>/dev/null || echo 0)
        if [ "$c" = "1" ]; then
          carrier_iface="$iface"
          log "carrier detected on $iface (carrier=1)"
          break 2
        fi
      done
      # Log the real state roughly once a second (every other 0.5s tick).
      if [ $(( i % 2 )) -eq 1 ]; then
        for iface in $ifaces; do
          c=$(${coreutils}/bin/cat /sys/class/net/$iface/carrier 2>/dev/null || echo "?")
          o=$(${coreutils}/bin/cat /sys/class/net/$iface/operstate 2>/dev/null || echo "?")
          log "waiting for carrier: $iface carrier=$c operstate=$o"
        done
      fi
      ${coreutils}/bin/sleep 0.5 2>/dev/null || true
    done
    if [ -z "$carrier_iface" ]; then
      log "WARNING: no carrier after ~20s; running dhcpcd anyway (may not bind)"
    fi

    log "interfaces after bring-up:"
    ${iproute2}/bin/ip link > /dev/console 2>&1 || true

    # dhcpcd's compiled-in dbdir (--dbdir=/var/lib/dhcpcd) and run dir
    # must be writable, or it bails before touching the wire. dhcpcd 10.3
    # mkdir()s /var/run/dhcpcd (not /run/dhcpcd) and reads /etc/dhcpcd.conf,
    # so create /var/run/dhcpcd (which also makes /var/run) plus the legacy
    # /run/dhcpcd, and touch an empty config so read_config does not error.
    # /var, /run and /etc are writable overlays/tmpfs here.
    ${coreutils}/bin/mkdir -p /var/lib/dhcpcd /var/run/dhcpcd /run/dhcpcd
    ${coreutils}/bin/touch /etc/dhcpcd.conf 2>/dev/null || true
    # Run dhcpcd in the FOREGROUND, bounded, IPv4-only, waiting for the lease,
    # with all output on /dev/console so the DHCP negotiation is visible.
    # Per dhcpcd(8) 10.3.1:
    #   -4 / --ipv4only : configure IPv4 only (skip SLAAC/DHCPv6).
    #   --waitip=4      : do not fork to the background until an IPv4 address
    #                     has actually been ASSIGNED — so the foreground call
    #                     only returns after the lease is bound (the inet addr
    #                     is on the NIC), then dhcpcd forks to keep the lease.
    #   -t 20           : give up after 20s. WITHOUT -1, on timeout dhcpcd
    #                     forks to the background and keeps trying instead of
    #                     exiting, so the foreground returns and /init proceeds
    #                     (degrade, never block forever).
    # We deliberately avoid -b (daemonizes immediately, hiding the negotiation
    # logs and not waiting for a bind) and -1/--oneshot (exits before the
    # address is committed). On a bound lease the foreground returns 0; on
    # timeout it returns and /init continues.
    log "running dhcpcd (foreground, IPv4, waiting for lease, output to console)"
    ${dhcpcd}/bin/dhcpcd -4 -t 20 --waitip=4 $ifaces > /dev/console 2>&1 \
      || log "WARNING: dhcpcd did not bind an IPv4 lease in time (see output above)"

    log "addresses after DHCP:"
    ${iproute2}/bin/ip addr > /dev/console 2>&1 || true

    # --- ssh host keys ---
    log "ensuring ssh host keys"
    ${coreutils}/bin/mkdir -p /etc/ssh
    if [ ! -f /etc/ssh/ssh_host_ed25519_key ]; then
      ${openssh}/bin/ssh-keygen -t ed25519 -f /etc/ssh/ssh_host_ed25519_key -N "" 2>/dev/null \
        || log "WARNING: ssh host key generation failed"
    fi

    # --- nix daemon ---
    log "starting nix-daemon"
    ${nix}/bin/nix-daemon > /var/log/nix-daemon.log 2>&1 &

    # --- sshd ---
    log "starting sshd on port ${toString fullSystem.sshdPort}"
    # Privilege-separation prerequisites. sshd's pre-auth child chroots into
    # /var/empty and re-execs through /run/sshd; both must exist and (for
    # StrictModes) be owned root:root and NOT group/world-writable, or the
    # child dies right after accept() — the client gets no banner and the
    # connection hangs until it times out. /var and /run are writable here
    # (overlay + tmpfs), so re-assert the dirs and their perms at runtime
    # rather than trusting only the baked squashfs entries.
    ${coreutils}/bin/mkdir -p /var/empty /run/sshd /var/run/sshd
    ${coreutils}/bin/chown root:root /var/empty /run/sshd /var/run/sshd 2>/dev/null || true
    ${coreutils}/bin/chmod 0711 /var/empty
    ${coreutils}/bin/chmod 0755 /run/sshd /var/run/sshd
    # Validate the config, then start sshd. -E /dev/console sends sshd's own
    # log (connections, auth, privsep errors) to the serial console — the
    # only place we can see it in this syslog-less env. With LogLevel VERBOSE
    # in sshd_config, the next run's stage log shows exactly what happens on
    # each inbound connection (or NOTHING if no connection arrives at all,
    # which would point at slirp forwarding rather than sshd).
    ${openssh}/bin/sshd -t -f /etc/ssh/sshd_config > /dev/console 2>&1 \
      || log "WARNING: sshd config test (-t) failed"
    ${openssh}/bin/sshd -f /etc/ssh/sshd_config -E /dev/console > /dev/console 2>&1 \
      || log "WARNING: sshd failed to start"
    # Confirm sshd actually bound the port so the next run definitively
    # shows whether 0.0.0.0:${toString fullSystem.sshdPort} is listening.
    log "listening sockets:"
    ${iproute2}/bin/ss -tlnp > /dev/console 2>&1 || true

    # --- guest-side reachability self-probe (decisive) ---
    # Probe sshd from inside the guest, on loopback and on the leased eth0
    # IP, using bash's /dev/tcp. This isolates the failure: if loopback OK
    # but the orchestrator still can't reach :${toString fullSystem.sshdPort}, the problem is slirp
    # forwarding, not sshd; if loopback FAILs too, sshd never bound/serves.
    log "running sshd reachability self-probe"
    selftest() {
      ( exec 3<>"/dev/tcp/$1/${toString fullSystem.sshdPort}" ) 2>/dev/null \
        && log "SELFTEST $1:${toString fullSystem.sshdPort} TCP OK" \
        || log "SELFTEST $1:${toString fullSystem.sshdPort} TCP FAIL"
    }
    selftest 127.0.0.1
    # Probe each IPv4 address actually assigned to a NIC (captures the real
    # leased address instead of hardcoding the slirp default 10.0.2.15).
    guest_ips=$(${iproute2}/bin/ip -o -4 addr show scope global 2>/dev/null \
      | ${coreutils}/bin/cut -d' ' -f7 | ${coreutils}/bin/cut -d/ -f1)
    if [ -z "$guest_ips" ]; then
      log "SELFTEST no global IPv4 address found; falling back to 10.0.2.15"
      guest_ips="10.0.2.15"
    fi
    for gip in $guest_ips; do
      selftest "$gip"
    done

    log "recovery system ready — dropping to console shell"
    # Local operator shell on the console. exec so bash becomes PID 1's
    # foreground; when it exits PID 1 (this script) is gone and the
    # kernel panics — acceptable for a manual recovery session.
    exec ${bash}/bin/bash -i < /dev/console > /dev/console 2>&1
  '';

  # closureInfo gives us the transitive store-path set + a `registration`
  # file in `nix-store --load-db` format. Mirrors the pattern used by
  # pkgs.dockerTools and nixos/lib/make-disk-image.nix to build a
  # self-contained /nix/store with a valid DB.
  closure = pkgs.closureInfo {
    rootPaths = fullSystem.packages ++ [ initScript bash cacert ];
  };

  sshdConfig = pkgs.writeText "sshd_config" ''
    Port ${toString fullSystem.sshdPort}
    ListenAddress 0.0.0.0
    PermitRootLogin prohibit-password
    PubkeyAuthentication yes
    PasswordAuthentication no
    UsePAM no
    # OpenSSH 10.x enables PerSourcePenalties by default: a single rejected
    # connection makes sshd refuse all subsequent connections from that source
    # pre-banner for a penalty window, which poisons the orchestrator's retry
    # loop. Disable it so a transient rejection does not block later retries.
    PerSourcePenalties no
    HostKey /etc/ssh/ssh_host_ed25519_key
    AuthorizedKeysFile /root/.ssh/authorized_keys
    # A non-interactive `ssh host 'cmd'` session inherits sshd's own minimal
    # PATH, which omits the rescue tool dirs, so plain `grep`/`ss`/`nix-shell`
    # are not found. Inject a usable PATH into every session (interactive and
    # non-interactive) so recovery commands resolve without a login profile.
    # NIX_PATH makes <nixpkgs> resolvable for non-interactive
    # `ssh host 'nix-shell -p hello --run hello'` (no /etc/profile sourced);
    # it points at the pinned nixpkgs fetched on demand (not baked in).
    SetEnv PATH=/bin:/sbin:/usr/bin:/usr/sbin NIX_PATH=nixpkgs=flake:nixpkgs
    Subsystem sftp ${openssh}/libexec/sftp-server
    # VERBOSE so every connection/auth attempt is logged to the console
    # (-E /dev/console in /init); the minimal env has no syslog.
    LogLevel VERBOSE
  '';

  # Login-shell PATH for a human SSHing in for real recovery. SetEnv in
  # sshd_config covers non-interactive `bash -c` sessions; this covers
  # interactive login shells (sourced via /etc/profile and /root/.bashrc).
  profileScript = pkgs.writeText "profile" ''
    export PATH=/bin:/sbin:/usr/bin:/usr/sbin
    export HOME=/root
    # Make <nixpkgs> resolvable for classic `nix-shell -p` in interactive
    # login shells. Matches nix-path/registry in nix.conf — points at the
    # pinned nixpkgs fetched on demand (not baked into the squashfs).
    export NIX_PATH=nixpkgs=flake:nixpkgs
  '';

  nixConf = pkgs.writeText "nix.conf" ''
    experimental-features = nix-command flakes
    build-users-group =
    substituters = https://cache.nixos.org/
    trusted-public-keys = cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=
    trusted-users = root
    # Resolve <nixpkgs> to the flake's pinned nixpkgs, fetched on demand
    # from GitHub (the source is NOT baked into the squashfs — that would
    # overflow the 256M ESP). flake:nixpkgs goes through the flake
    # registry below, which is pinned to the locked rev, so both
    # `nix-shell -p` (classic, via <nixpkgs>) and `nix shell nixpkgs#...`
    # (flake) resolve to the same reproducible nixpkgs.
    nix-path = nixpkgs=flake:nixpkgs
    flake-registry = /etc/nix/registry.json
  '';

  # Pin the `nixpkgs` flake-registry entry to the exact locked rev so
  # `flake:nixpkgs` (and therefore <nixpkgs> via nix-path above, plus
  # `nix shell nixpkgs#hello`) fetches the same nixpkgs the build used.
  nixRegistry = pkgs.writeText "registry.json" (builtins.toJSON {
    version = 2;
    flakes = [
      {
        from = { type = "indirect"; id = "nixpkgs"; };
        to = {
          type = "github";
          owner = "NixOS";
          repo = "nixpkgs";
          rev = nixpkgsRev;
        };
      }
    ];
  });

  authorizedKeys = pkgs.writeText "authorized_keys"
    (lib.concatStringsSep "\n" fullSystem.rootAuthorizedKeys + "\n");

  fullSquashfs = pkgs.runCommand "nmbl-rescue.sfs"
    {
      nativeBuildInputs = [ pkgs.squashfsTools pkgs.nix ];
    }
    ''
      mkdir -p root/nix/store root/nix/var/nix/db
      mkdir -p root/bin root/sbin root/usr/bin root/etc/nix root/etc/ssh
      mkdir -p root/root/.ssh root/var/empty root/proc root/sys root/dev root/run root/tmp

      # Copy every path of the combined closure into the image store.
      echo "copying closure into image /nix/store"
      while read -r p; do
        cp -a "$p" root/nix/store/
      done < ${closure}/store-paths

      # Register the closure in the image's nix DB. NIX_STATE_DIR points
      # the db into the staging tree; the registration manifest comes
      # from closureInfo. load-db reads paths relative to the real store
      # but writes the DB rooted at NIX_STATE_DIR.
      echo "registering nix database"
      export NIX_STATE_DIR=$PWD/root/nix/var/nix
      export NIX_STORE_DIR=/nix/store
      nix-store --load-db < ${closure}/registration

      # --- baked config files ---
      cp ${nixConf}        root/etc/nix/nix.conf
      # Pinned flake registry so flake:nixpkgs (and <nixpkgs> via nix-path)
      # resolves to the locked rev, fetched on demand from GitHub.
      cp ${nixRegistry}    root/etc/nix/registry.json
      cp ${sshdConfig}     root/etc/ssh/sshd_config
      cp ${authorizedKeys} root/root/.ssh/authorized_keys

      # Login-shell PATH for interactive recovery sessions (SetEnv in
      # sshd_config handles the non-interactive case). /etc/profile is sourced
      # by login shells; /root/.bashrc by interactive non-login bash.
      cp ${profileScript} root/etc/profile
      cp ${profileScript} root/root/.bashrc

      # --- CA trust store ---
      # Symlink the conventional bundle paths at the cacert store path so
      # tools that consult /etc/ssl/certs (rather than NIX_SSL_CERT_FILE)
      # still find a trust store. The closure carries cacert's store path.
      mkdir -p root/etc/ssl/certs
      ln -s ${cacert}/etc/ssl/certs/ca-bundle.crt root/etc/ssl/certs/ca-bundle.crt
      ln -s ${cacert}/etc/ssl/certs/ca-bundle.crt root/etc/ssl/certs/ca-certificates.crt

      # /init entrypoint (the bash script, resolved to a real file).
      cp ${initScript} root/init
      chmod 0755 root/init

      # --- /bin, /sbin, /usr/bin shims onto the closure ---
      ln -s ${bash}/bin/bash             root/bin/bash
      ln -s ${bash}/bin/bash             root/bin/sh
      ln -s ${bash}/bin/bash             root/usr/bin/bash
      for tool in ${coreutils}/bin/* ${utilLinux}/bin/* ${iproute2}/bin/* \
                  ${procps}/bin/* ${kmod}/bin/* ${btrfs}/bin/* \
                  ${cryptsetup}/bin/* ${btop}/bin/* ${e2fsprogs}/bin/* \
                  ${gnugrep}/bin/* ${gnused}/bin/* ${gawk}/bin/*; do
        name=$(basename "$tool")
        [ -e "root/bin/$name" ] || ln -s "$tool" "root/bin/$name"
      done
      for tool in ${nix}/bin/* ${openssh}/bin/* ${dhcpcd}/bin/* ${dhcpcd}/sbin/*; do
        [ -e "$tool" ] || continue
        name=$(basename "$tool")
        [ -e "root/bin/$name" ] || ln -s "$tool" "root/bin/$name"
      done
      # sbin tools (sshd lives in sbin in some builds; modprobe too).
      for d in ${openssh}/sbin ${kmod}/sbin ${utilLinux}/sbin ${e2fsprogs}/sbin; do
        [ -d "$d" ] || continue
        for tool in "$d"/*; do
          name=$(basename "$tool")
          [ -e "root/sbin/$name" ] || ln -s "$tool" "root/sbin/$name"
        done
      done

      # --- /etc account databases ---
      # root: uid 0, bash login shell. sshd: privsep user (sshd drops to
      # it for the unprivileged listener child); /var/empty is its home.
      # No nixbld users — nix.conf sets `build-users-group =` so the
      # daemon builds as root (single-user style).
      cat > root/etc/passwd <<EOF
      root:x:0:0:root:/root:${bash}/bin/bash
      sshd:x:498:498:sshd privsep:/var/empty:${coreutils}/bin/false
      EOF
      cat > root/etc/group <<EOF
      root:x:0:
      sshd:x:498:
      EOF
      # root's password field is EMPTY (not `!`/`*`), so the account is NOT
      # locked. OpenSSH 10.x refuses pubkey auth (PermitRootLogin
      # prohibit-password) for a LOCKED account ("account is locked"), so an
      # empty field is required for key-only ssh login to succeed. ssh stays
      # key-only because sshd_config sets PasswordAuthentication no; the empty
      # field only permits passwordless CONSOLE login, normal for an emergency
      # recovery system. The sshd privsep user stays locked.
      cat > root/etc/shadow <<EOF
      root::1::::::
      sshd:!:1::::::
      EOF
      chmod 0644 root/etc/passwd root/etc/group
      chmod 0600 root/etc/shadow

      # --- permissions on the ssh dir/key file ---
      chmod 0700 root/root
      chmod 0700 root/root/.ssh
      chmod 0600 root/root/.ssh/authorized_keys
      # sshd privsep dir: root:root, not group/world-writable (StrictModes).
      chmod 0711 root/var/empty

      # zstd image; -all-root makes every entry uid=0 gid=0 (the kernel
      # gives us no choice in a sandbox anyway). -no-progress keeps the
      # build log clean.
      mksquashfs root "$out" \
        -comp zstd -Xcompression-level 19 \
        -noappend \
        -all-root \
        -no-progress
    '';
in
if fullSystem.enable then fullSquashfs else flatSquashfs
