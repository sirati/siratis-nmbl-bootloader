# Rescue /init — PART B: networking (loopback, NIC bring-up, carrier wait, dhcpcd),
# ssh host keys, the nix-daemon, sshd (privsep prereqs + reachability self-probe),
# and the final exec into an interactive console shell.
#
# Continuation of ./init-script.nix — split out of lib/rescue-sfs.nix per FIX-19.
# Returns the shell-string FRAGMENT only; the orchestrator concatenates it onto
# the part-A fragment and hands the whole to one `pkgs.writeShellScript`, so the
# rendered /init is byte-identical to the pre-split body. The leading `${""}`
# is a zero-width column-0 token: it emits nothing but anchors the indented-string
# dedent to zero so every line keeps its literal indentation across the split.
{
  bash,
  coreutils,
  dhcpcd,
  iproute2,
  nix,
  openssh,
  fullSystem,
}:
''
${""}    # --- networking ---
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
''
