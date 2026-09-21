# Rescue /init — SSH identity, daemon, diagnostics, and console shell.
{
  lib,
  bash,
  coreutils,
  iproute2,
  nix,
  openssh,
  fullSystem,
  startNixDaemon ? true,
}:
''
    # --- ssh host keys ---
    log "ensuring ssh host keys"
    ${coreutils}/bin/mkdir -p /etc/ssh
    sshd_ready=1
    ${
      if fullSystem.hostKeyPath != null then ''
        persistent_host_key=${lib.escapeShellArg "/nmbl-root${fullSystem.hostKeyPath}"}
        if [ -L "$persistent_host_key" ]; then
          log "ERROR: persistent rescue SSH host key must not be a symlink"
          sshd_ready=0
        elif [ ! -f "$persistent_host_key" ]; then
          log "ERROR: persistent rescue SSH host key is missing: $persistent_host_key"
          sshd_ready=0
        else
          host_key_meta=$(${coreutils}/bin/stat -c '%u:%g:%a' "$persistent_host_key" 2>/dev/null || echo invalid)
          if [ "$host_key_meta" != "0:0:600" ]; then
            log "ERROR: rescue SSH host key must be root:root mode 0600, got $host_key_meta"
            sshd_ready=0
          else
            ${coreutils}/bin/install -m 0600 "$persistent_host_key" /etc/ssh/ssh_host_ed25519_key
            if [ -f "$persistent_host_key.pub" ]; then
              ${coreutils}/bin/install -m 0644 "$persistent_host_key.pub" /etc/ssh/ssh_host_ed25519_key.pub
            fi
          fi
        fi
      '' else ''
        if [ ! -f /etc/ssh/ssh_host_ed25519_key ]; then
          ${openssh}/bin/ssh-keygen -t ed25519 -f /etc/ssh/ssh_host_ed25519_key -N "" 2>/dev/null \
            || sshd_ready=0
        fi
      ''
    }

    ${lib.optionalString startNixDaemon ''
      # --- nix daemon ---
      log "starting nix-daemon"
      ${nix}/bin/nix-daemon > /var/log/nix-daemon.log 2>&1 &
    ''}

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
    if [ "$sshd_ready" = 1 ] \
      && ${openssh}/bin/sshd -t -f /etc/ssh/sshd_config > /dev/console 2>&1; then
      ${openssh}/bin/sshd -f /etc/ssh/sshd_config -E /dev/console > /dev/console 2>&1 \
        || log "WARNING: sshd failed to start"
    else
      log "ERROR: sshd not started because its host identity/config is invalid"
    fi
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
    selftest ::1
    # Probe each IPv4 address actually assigned to a NIC (captures the real
    # leased address instead of hardcoding the slirp default 10.0.2.15).
    guest_ips=$(${iproute2}/bin/ip -o -4 addr show scope global 2>/dev/null \
      | ${coreutils}/bin/cut -d' ' -f7 | ${coreutils}/bin/cut -d/ -f1)
    if [ -z "$guest_ips" ] && [ "$network_family" != "ipv6-only" ]; then
      log "SELFTEST no global IPv4 address found; falling back to 10.0.2.15"
      guest_ips="10.0.2.15"
    fi
    for gip in $guest_ips; do
      selftest "$gip"
    done
    guest_ipv6=$(${iproute2}/bin/ip -o -6 addr show scope global 2>/dev/null \
      | ${coreutils}/bin/cut -d' ' -f7 | ${coreutils}/bin/cut -d/ -f1)
    for gip in $guest_ipv6; do
      selftest "$gip"
    done

    log "recovery system ready — dropping to console shell"
    # Local operator shell on the console. exec so bash becomes PID 1's
    # foreground; when it exits PID 1 (this script) is gone and the
    # kernel panics — acceptable for a manual recovery session.
    exec ${bash}/bin/bash -i < /dev/console > /dev/console 2>&1
''
