# Builds the external NMBL rescue image (flat squashfs, or the full-system
# EROFS stage 2; see docs/rescue-stages.md).
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
#   * Full recovery system (`fullSystem.enable = true`): an EROFS image
#     (stage 2) with a real /nix/store + nix-db, bash, a root nix-daemon
#     (flakes on), sshd, its own kernel modules and network profile. The Rust loader execs `/init` (a bash
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
  #
  # The full-system image (stage 2) is HOST-INDEPENDENT: its only inputs are
  # the rescue package set, the kernel module closure for NMBL's kernel and
  # fixed scripts. Ports, keys, host identity, module choices and network
  # profiles are host data; NMBL hands them over at runtime from its own
  # config (`[rescue.system]`, see docs/rescue-stages.md), so two hosts with
  # the same kernel and packages share one image store path.
  fullSystem ? {
    enable = false;
    minimal = false;
    packages = [ ];
    moduleClosure = null;
    compression = "lz4hc";
    console = null;
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
  # The rescue console launcher. `fullSystem.console` replaces it only for
  # the test-only console fixture (an internal option guarded to
  # forceOnBoot test builds); production always uses the real launcher.
  rescueConsole =
    if (fullSystem.console or null) != null then fullSystem.console
    else import ./rescue/console.nix { inherit pkgs; };
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
  cacert = pkgs.cacert;

  # Kernel modules: the image carries the module closure (built for NMBL's
  # kernel, depmod'd) and firmware; WHICH modules to load is host data that
  # NMBL hands over (/etc/nmbl-rescue/modules). See ./rescue/init-modules.nix.
  modulesFragment = import ./rescue/init-modules.nix { inherit coreutils kmod; };

  # The rescue /init: PID 1 after switch_root. A bash script, baked into
  # the image at /init. References tools by absolute store path so it
  # never depends on a pre-existing PATH. Defensive: every step logs to
  # the console and tolerates failure so the operator still lands in a
  # shell even if (say) DHCP times out.
  initScriptPrefix = if fullSystem.minimal then
    import ./rescue/init-script-minimal.nix {
      inherit bash coreutils kmod utilLinux modulesFragment;
    }
  else
    import ./rescue/init-script.nix {
      inherit
        bash cacert coreutils e2fsprogs gawk gnugrep gnused nix openssh
        utilLinux modulesFragment;
    };
  initScript = pkgs.writeShellScript "nmbl-rescue-init" (
    initScriptPrefix
    + import ./rescue/init-tools.nix { inherit coreutils; }
    + import ./rescue/init-script-network.nix {
      inherit bash coreutils dhcpcd gawk iproute2 rescueConsole utilLinux;
    }
    + import ./rescue/init-script-net.nix {
      inherit lib bash coreutils iproute2 nix openssh rescueConsole utilLinux;
      startNixDaemon = !fullSystem.minimal;
    }
  );

  # closureInfo gives us the transitive store-path set + a `registration`
  # file in `nix-store --load-db` format. Mirrors the pattern used by
  # pkgs.dockerTools and nixos/lib/make-disk-image.nix to build a
  # self-contained /nix/store with a valid DB.
  closure = pkgs.closureInfo {
    rootPaths = fullSystem.packages ++ [ initScript bash ]
      ++ lib.optional (!fullSystem.minimal) cacert;
  };

  sshdConfig = import ./rescue/sshd-config.nix {
    inherit pkgs openssh;
  };

  # Login-shell PATH for a human SSHing in for real recovery. SetEnv in
  # sshd_config covers non-interactive `bash -c` sessions; this covers
  # interactive login shells (sourced via /etc/profile and /root/.bashrc).
  profileScript = pkgs.writeText "profile" ''
    export PATH=/bin:/sbin:/usr/bin:/usr/sbin
    export HOME=/root
    ${lib.optionalString (!fullSystem.minimal) ''
      export NIX_PATH=nixpkgs=flake:nixpkgs
    ''}
    # NMBL's TUI control socket, visible here because NMBL (still PID 1 outside
    # the chroot) bind-mounts its own root at /nmbl-root. `nmbl-tui` (a /bin
    # shim onto NMBL's own static binary) honours this as its socket override.
    export NMBL_TUI_SOCK=/nmbl-root/nmbl-run/tui.sock
  '';

  # Shown by sshd after an interactive login (PrintMotd), so an operator who
  # reached the rescue over SSH knows where they are and how to get back to
  # the bootloader. Never shown before authentication.
  motd = pkgs.writeText "nmbl-rescue-motd" ''

    NMBL rescue - run `nmbl` to enter the bootloader.

    This is NMBL's recovery system. NMBL is still running as PID 1 outside
    it: `nmbl` attaches to its menu (boot a generation, retry, reboot).
    NMBL's own root is mounted at /nmbl-root; the host disks are under /dev.

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
      pkgs lib closure nixConf nixRegistry sshdConfig motd
      profileScript cacert initScript bash coreutils utilLinux iproute2
      procps kmod e2fsprogs gnugrep gnused gawk nix
      openssh dhcpcd fullSystemPackagePaths moduleClosurePath;
    minimal = fullSystem.minimal;
    compression = fullSystem.compression or "lz4hc";
  };
in
if fullSystem.enable then fullSquashfs else flatSquashfs
