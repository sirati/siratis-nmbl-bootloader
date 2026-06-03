# Full recovery system rescue image (`fullSystem.enable = true`): a real
# /nix/store + nix-db squashfs with bash, btop, a root nix-daemon (flakes on),
# and sshd. The Rust loader execs `/init` (the bash script baked in by the
# orchestrator). Split out of lib/rescue-sfs.nix per FIX-19; the build body is
# byte-identical to the pre-split `fullSquashfs` binding.
{
  pkgs,
  lib,
  closure,
  nixConf,
  nixRegistry,
  sshdConfig,
  authorizedKeys,
  profileScript,
  cacert,
  initScript,
  bash,
  coreutils,
  utilLinux,
  iproute2,
  procps,
  kmod,
  btrfs,
  cryptsetup,
  btop,
  e2fsprogs,
  gnugrep,
  gnused,
  gawk,
  nix,
  openssh,
  dhcpcd,
  fullSystemPackagePaths,
  moduleClosurePath,
}:

let
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

      # --- rescue kernel modules + firmware ---
      # Stage the module closure (built against NMBL's exact kernel) into
      # the squashfs root so the rescue /init can modprobe its own drivers
      # after switch_root: /lib/modules/<kver> carries the .ko + the
      # depmod'd modules.dep (makeModulesClosure runs depmod), and
      # /lib/firmware carries only the blobs those drivers reference. The
      # running kernel after switch_root is still NMBL's, so $(uname -r)
      # matches <kver> and plain modprobe resolves them. cp -aL so the
      # closure's symlinks are dereferenced into the self-contained blob.
      mc=${lib.escapeShellArg moduleClosurePath}
      if [ -n "$mc" ] && [ -d "$mc/lib/modules" ]; then
        echo "staging rescue kernel modules from $mc/lib/modules"
        mkdir -p root/lib
        cp -aL "$mc/lib/modules" root/lib/modules
        if [ -d "$mc/lib/firmware" ]; then
          echo "staging rescue firmware from $mc/lib/firmware"
          cp -aL "$mc/lib/firmware" root/lib/firmware
        else
          echo "no rescue firmware in closure (no module requested any)"
        fi
        chmod -R u+w root/lib
      else
        echo "no rescue module closure to stage (fullSystem off or all built-in)"
      fi

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

      # nmbl-tui: a client onto NMBL's TUI. NMBL stays PID 1 OUTSIDE this
      # chroot and bind-mounts its own root in at /nmbl-root, so NMBL's own
      # static binary is reachable here at /nmbl-root/init. Symlink it onto
      # PATH as `nmbl-tui`; run from a non-PID-1 process it auto-detects
      # getpid()!=1 → client mode and connects to NMBL_TUI_SOCK (exported in
      # /init, /etc/profile and sshd_config → /nmbl-root/nmbl-run/tui.sock).
      # The link target only resolves once NMBL has set up the /nmbl-root
      # bind mount; that is expected — the squashfs just provides the alias.
      ln -s /nmbl-root/init              root/bin/nmbl-tui
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

      # Generalised shims for any downstream `rescue.fullSystem.packages`
      # (wpa_supplicant, iw, …): link every binary in each package's bin/
      # and sbin/ onto PATH so it resolves from the rescue shell and over
      # ssh. Idempotent ([ -e ] guard) so a package overlapping the
      # hardcoded core set above does not clobber existing links; only the
      # last component name matters for PATH resolution.
      for pkgpath in ${fullSystemPackagePaths}; do
        for tool in "$pkgpath"/bin/*; do
          [ -e "$tool" ] || continue
          name=$(basename "$tool")
          [ -e "root/bin/$name" ] || ln -s "$tool" "root/bin/$name"
        done
        for tool in "$pkgpath"/sbin/*; do
          [ -e "$tool" ] || continue
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
fullSquashfs
