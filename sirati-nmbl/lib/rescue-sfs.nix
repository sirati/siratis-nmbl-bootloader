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
    coreModules = [ ];
    moduleClosure = null;
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

  # All kernel modules the rescue /init loads ITSELF after switch_root.
  # Core fs/packet modules first (overlay + ext4 back the writable
  # scratch, af_packet backs dhcpcd's BPF socket), then the NIC drivers.
  # NMBL no longer preloads any of these — their .ko (+ firmware) ship in
  # the squashfs at /lib/modules/<uname -r> and /lib/firmware, staged from
  # `fullSystem.moduleClosure`. The running kernel after switch_root is
  # still NMBL's, so `uname -r` matches the staged module-tree version and
  # plain `modprobe` (absolute kmod path — PATH is not set up yet) resolves
  # them via the modules.dep we depmod at build time. Firmware-dependent
  # NIC drivers (wifi: iwlwifi, ath*, brcmfmac, …) work here BECAUSE
  # /lib/firmware is present in the squashfs root and firmware_class's
  # search path is pointed at it just before these modprobes run — unlike
  # loading the driver into NMBL's firmware-less initramfs.
  rescueModprobeList = fullSystem.coreModules ++ fullSystem.nicDrivers;
  rescueModprobes = lib.concatMapStringsSep "\n"
    (m: "    ${kmod}/bin/modprobe ${m} > /dev/console 2>&1 || log \"WARNING: modprobe ${m} failed\"")
    rescueModprobeList;

  # The rescue /init: PID 1 after switch_root. A bash script, baked into
  # the image at /init. References tools by absolute store path so it
  # never depends on a pre-existing PATH. Defensive: every step logs to
  # the console and tolerates failure so the operator still lands in a
  # shell even if (say) DHCP times out.
  initScript = pkgs.writeShellScript "nmbl-rescue-init" (
    import ./rescue/init-script.nix {
      inherit
        bash cacert coreutils e2fsprogs gawk gnugrep gnused nix openssh
        utilLinux rescueModprobes;
    }
    + import ./rescue/init-script-net.nix {
      inherit bash coreutils dhcpcd iproute2 nix openssh fullSystem;
    }
  );

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
    # NMBL_TUI_SOCK lets a non-interactive `ssh host nmbl-tui` find NMBL's
    # TUI socket, bind-mounted in via NMBL's root at /nmbl-root.
    SetEnv PATH=/bin:/sbin:/usr/bin:/usr/sbin NIX_PATH=nixpkgs=flake:nixpkgs NMBL_TUI_SOCK=/nmbl-root/nmbl-run/tui.sock
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
    # NMBL's TUI control socket, visible here because NMBL (still PID 1 outside
    # the chroot) bind-mounts its own root at /nmbl-root. `nmbl-tui` (a /bin
    # shim onto NMBL's own static binary) honours this as its socket override.
    export NMBL_TUI_SOCK=/nmbl-root/nmbl-run/tui.sock
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

  # Downstream-supplied recovery packages (e.g. wpa_supplicant, iw added
  # by a laptop config). Their bin/sbin dirs are shimmed onto PATH below,
  # alongside the hardcoded core tools, so any binary the operator added
  # via `rescue.fullSystem.packages` is usable from the rescue shell and
  # over ssh without the caller having to also touch this file. The store
  # paths are emitted as a space-separated list the build loop iterates.
  fullSystemPackagePaths =
    lib.concatMapStringsSep " " (p: "${p}") fullSystem.packages;

  # The rescue module closure built against NMBL's exact kernel (its
  # /lib/modules/<kver> already has a depmod'd modules.dep, and its
  # /lib/firmware holds only the blobs those modules reference). Staged
  # into the squashfs root so the rescue /init can modprobe them after
  # switch_root. May be null (fullSystem disabled / no modules), in which
  # case the staging block is a no-op. makeModulesClosure with
  # allowMissing and zero resolved modules emits an EMPTY out (no lib/),
  # so the build-time `cp` is guarded by an existence test regardless.
  moduleClosurePath =
    if fullSystem.moduleClosure != null then "${fullSystem.moduleClosure}" else "";


  flatSquashfs = import ./rescue/flat.nix {
    inherit pkgs contents;
  };

  fullSquashfs = import ./rescue/full-system.nix {
    inherit
      pkgs lib closure nixConf nixRegistry sshdConfig authorizedKeys
      profileScript cacert initScript bash coreutils utilLinux iproute2
      procps kmod btrfs cryptsetup btop e2fsprogs gnugrep gnused gawk nix
      openssh dhcpcd fullSystemPackagePaths moduleClosurePath;
  };
in
if fullSystem.enable then fullSquashfs else flatSquashfs
