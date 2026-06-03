# Runtime install of the secure-boot test disk (#57 F6b signing-model fix).
#
# HARD PROJECT PRINCIPLE: a signing PRIVATE key must NEVER be an input to a Nix
# derivation (a derivation's inputs land in the world-readable /nix/store). The
# old model `sbsign`/`nmbl-sign`-ed the test disk inside a `runCommand` that
# store-imported `insecure-test-{ml-dsa-87,sb-db}.key` — forbidden.
#
# This module instead INSTALLS the disk at RUNTIME via nixos-anywhere, exactly
# like production: a rescue VM boots, nixos-anywhere installs the NMBL config,
# and NMBL's normal install-time signing (lib/install-{signing,gen-signing}.nix)
# `sbsign`s the UKI and writes the per-generation ML-DSA sidecars FROM THE KEY'S
# PATH staged in the installer — never from a derivation. The booted disk is
# therefore signed by the real install-time path-based code, and the private
# keys appear in NO derivation closure.
#
# The orchestrator reads the test keys from a RUNTIME directory (`--keys-dir`,
# default `$NMBL_TEST_KEYS_DIR`, else `$PWD`'s `testing/keys`). It stages them
# into the kexec-installer at the on-disk paths the config declares
# (`/run/nmbl-test-keys/insecure-test-{gen.key,sb-db.key,sb-db.crt}`) between
# nixos-anywhere's disko and install phases, so `installBootLoader` signs in
# place. The committed test private key is PUBLICLY KNOWN (testing/keys/
# README.md) and only ever signs TEST artifacts; passing it as a runtime PATH
# (not a Nix path literal) keeps it out of the store, mirroring a real operator
# whose `db` key lives at `/run/secrets/...`.

{
  nixpkgs,
  system ? "x86_64-linux",
  nixos-anywhere,
  rescueArtifacts,
}:

let
  pkgs = nixpkgs.legacyPackages.${system};

  # Build one install orchestrator for an install-variant NMBL config (one whose
  # `boot.nmbl.signing.deferInstallSigning = false`, so the install-time signing
  # actually runs). Produces the SIGNED qcow2 at "$WORK_DIR/disk1.qcow2".
  mkSbInstaller =
    {
      name,
      # An evaluated NixOS system whose config.system.build.{diskoScript,toplevel}
      # are the nixos-anywhere --store-paths. MUST have deferInstallSigning=false.
      config,
      port,
    }:
    pkgs.writeShellApplication {
      name = "sb-install-${name}";
      runtimeInputs = [
        pkgs.qemu_kvm
        pkgs.dosfstools
        pkgs.mtools
        pkgs.coreutils
        pkgs.openssh
        pkgs.netcat-openbsd
        pkgs.gawk
        pkgs.gnused
        nixos-anywhere.packages.${system}.default
      ];
      text = ''
        set -euo pipefail

        NAME="${name}"
        PORT="${port}"
        # Pre-built store paths so nixos-anywhere uses --store-paths (sidesteps
        # the pure-eval sibling-import problem). These two are the ONLY signing
        # inputs that come from the store; neither contains a private key.
        DISKO_SCRIPT="${config.config.system.build.diskoScript}"
        NIXOS_SYSTEM="${config.config.system.build.toplevel}"
        SYSTEMRESCUE_ISO="${rescueArtifacts.systemRescueIso}"
        SYSTEMRESCUE_KERNEL="${rescueArtifacts.systemRescueBoot}/vmlinuz"
        SYSTEMRESCUE_INITRD="${rescueArtifacts.systemRescueBoot}/initrd"

        WORK_DIR="$PWD/.sb-install-$NAME"
        MEMORY="2560"
        CORES="4"
        SSH_KEY_FILE="''${NMBL_SSH_KEY:-}"
        # Where the INSTALL-TIME signing keys are read FROM at runtime. They are
        # staged into the rescue installer's /run/nmbl-test-keys/ so NMBL's
        # install-time signing reads them by PATH (never a derivation input).
        KEYS_DIR="''${NMBL_TEST_KEYS_DIR:-$PWD/testing/keys}"
        # The on-disk paths the test-secure-boot config declares (its
        # generationKeyFile / uki.{keyFile,certFile} defaults).
        STAGE_DIR="/run/nmbl-test-keys"

        usage() {
          cat <<EOF
        Usage: sb-install-$NAME [options]

        Install the secure-boot test disk at RUNTIME via nixos-anywhere so the
        UKI + generation sidecars are signed by NMBL's install-time path-based
        code (no signing key ever enters a derivation). Leaves the SIGNED disk
        at \$WORK_DIR/disk1.qcow2.

          --ssh-key PATH       Passphrase-less SSH PRIVATE key (or set NMBL_SSH_KEY).
          --keys-dir PATH      Dir holding the INSTALL-TIME signing keys
                               (insecure-test-gen.key / insecure-test-sb-db.{key,crt});
                               default \$NMBL_TEST_KEYS_DIR or \$PWD/testing/keys.
          --work-dir PATH      Where disks/logs go (default \$PWD/.sb-install-$NAME)
          --memory MB          Guest memory (default 2560)
          --cores N            Guest vCPUs (default 4)
          -h, --help           Show this help
        EOF
        }

        while [[ $# -gt 0 ]]; do
          case "$1" in
            --ssh-key)   SSH_KEY_FILE="$2"; shift 2;;
            --keys-dir)  KEYS_DIR="$2"; shift 2;;
            --work-dir)  WORK_DIR="$2"; shift 2;;
            --memory)    MEMORY="$2"; shift 2;;
            --cores)     CORES="$2"; shift 2;;
            -h|--help)   usage; exit 0;;
            *) echo "Unknown option: $1" >&2; usage >&2; exit 1;;
          esac
        done

        # The committed test keys must exist at KEYS_DIR. We use the historical
        # nmbl-sign key naming (insecure-test-gen.key) the config defaults to;
        # the committed ml-dsa key file is insecure-test-ml-dsa-87.key, so accept
        # either basename for the generation key.
        GEN_KEY=""
        for c in "$KEYS_DIR/insecure-test-gen.key" "$KEYS_DIR/insecure-test-ml-dsa-87.key"; do
          [[ -f "$c" ]] && { GEN_KEY="$c"; break; }
        done
        SB_KEY="$KEYS_DIR/insecure-test-sb-db.key"
        SB_CRT="$KEYS_DIR/insecure-test-sb-db.crt"
        if [[ -z "$GEN_KEY" || ! -f "$SB_KEY" || ! -f "$SB_CRT" ]]; then
          echo "Error: install-time signing keys not found under '$KEYS_DIR'." >&2
          echo "       Need a generation key (insecure-test-gen.key or" >&2
          echo "       insecure-test-ml-dsa-87.key) plus insecure-test-sb-db.{key,crt}." >&2
          echo "       Pass --keys-dir or set NMBL_TEST_KEYS_DIR." >&2
          exit 1
        fi

        mkdir -p "$WORK_DIR"
        cd "$WORK_DIR"

        if [[ -z "$SSH_KEY_FILE" ]]; then
          if [[ -n "''${SSH_PRIVATE_KEY:-}" ]]; then
            SSH_KEY_FILE="$WORK_DIR/ssh-key-from-env"
            printf '%s\n' "$SSH_PRIVATE_KEY" > "$SSH_KEY_FILE"
            chmod 600 "$SSH_KEY_FILE"
          fi
        fi
        if [[ -z "$SSH_KEY_FILE" || ! -f "$SSH_KEY_FILE" ]]; then
          echo "Error: no SSH private key supplied (pass --ssh-key or NMBL_SSH_KEY/SSH_PRIVATE_KEY)." >&2
          echo "       nixos-anywhere REQUIRES a private key file for its bootstrap." >&2
          exit 1
        fi
        chmod 600 "$SSH_KEY_FILE" 2>/dev/null || true
        KEY="$SSH_KEY_FILE"
        PRIMARY_PUBKEY=$(ssh-keygen -y -f "$KEY")

        echo "Creating a fresh 16G install disk (overwriting any stale state)"
        rm -f disk1.qcow2
        qemu-img create -f qcow2 disk1.qcow2 16G >/dev/null

        AR_IMG="autorun.img"
        rm -f "$AR_IMG"
        truncate -s 16M "$AR_IMG"
        mkfs.vfat -n SYSRESCUE "$AR_IMG" >/dev/null
        AR_TMP=$(mktemp)
        {
          printf '%s\n' '#!/bin/bash'
          printf '%s\n' 'set -e'
          printf '%s\n' 'mkdir -p /root/.ssh && chmod 700 /root/.ssh'
          printf 'cat > /root/.ssh/authorized_keys <<KEYS\n%s\nKEYS\n' "$PRIMARY_PUBKEY"
          printf '%s\n' 'chmod 600 /root/.ssh/authorized_keys'
          printf '%s\n' 'systemctl restart sshd.service 2>/dev/null || true'
        } > "$AR_TMP"
        chmod +x "$AR_TMP"
        mcopy -i "$AR_IMG" "$AR_TMP" ::autorun
        rm -f "$AR_TMP"

        QEMU_PID=""
        cleanup() {
          if [[ -n "$QEMU_PID" ]] && kill -0 "$QEMU_PID" 2>/dev/null; then
            kill -TERM "$QEMU_PID" 2>/dev/null || true
            local i=0
            while kill -0 "$QEMU_PID" 2>/dev/null && (( i < 10 )); do sleep 1; i=$((i+1)); done
            kill -KILL "$QEMU_PID" 2>/dev/null || true
          fi
        }
        trap cleanup EXIT INT TERM

        SSH_OPTS=(
          -o StrictHostKeyChecking=no
          -o UserKnownHostsFile=/dev/null
          -o LogLevel=ERROR
          -o ConnectTimeout=10
          -o IdentitiesOnly=yes
        )

        wait_for_ssh() {
          local budget="$1" i=0
          echo "Polling SSH on port $PORT (budget ''${budget}s)..."
          while ! ssh -i "$KEY" -p "$PORT" "''${SSH_OPTS[@]}" -o BatchMode=yes root@localhost true 2>/dev/null; do
            i=$((i+1))
            if (( i >= budget )); then echo "Timed out waiting for SSH after ''${budget}s" >&2; return 1; fi
            sleep 1
          done
          echo "SSH reachable on port $PORT (after ''${i}s)"
        }

        echo
        echo "===== STAGE 1: Boot rescue VM (SystemRescue) ====="
        KARGS="archisobasedir=sysresccd archisolabel=RESCUE1300 iomem=relaxed console=ttyS0,115200n8 ar_source=/dev/vdb ar_ignorefail nofirewall"
        rm -f stage1.log qemu-stage1.pid
        qemu-system-x86_64 \
          -machine q35,accel=kvm:tcg \
          -cpu max \
          -m "$MEMORY" \
          -smp "$CORES" \
          -kernel "$SYSTEMRESCUE_KERNEL" \
          -initrd "$SYSTEMRESCUE_INITRD" \
          -append "$KARGS" \
          -drive "file=$SYSTEMRESCUE_ISO,media=cdrom,readonly=on,if=none,id=cd0" \
          -device ide-cd,drive=cd0,bus=ide.0 \
          -drive file=disk1.qcow2,format=qcow2,if=virtio \
          -drive "file=$AR_IMG,format=raw,if=virtio" \
          -netdev "user,id=net0,hostfwd=tcp::$PORT-:22" \
          -device virtio-net-pci,netdev=net0 \
          -display none \
          -serial file:stage1.log \
          -monitor none \
          -daemonize \
          -pidfile qemu-stage1.pid
        QEMU_PID=$(cat qemu-stage1.pid)
        echo "Stage 1 QEMU pid: $QEMU_PID  (serial log: $WORK_DIR/stage1.log)"

        wait_for_ssh 240
        PUBKEY=$(ssh-keygen -y -f "$KEY")
        ssh -i "$KEY" -p "$PORT" "''${SSH_OPTS[@]}" root@localhost \
          "mkdir -p /root/.ssh && chmod 700 /root/.ssh && printf '%s\n' '$PUBKEY' > /root/.ssh/authorized_keys && chmod 600 /root/.ssh/authorized_keys"

        echo
        echo "===== STAGE 2a: nixos-anywhere kexec + disko (no install yet) ====="
        # Split the run so we can stage the INSTALL-TIME signing keys into the
        # kexec-installer BEFORE installBootLoader runs. The keys are read by
        # PATH at install time — never imported into a derivation.
        nixos-anywhere \
          --store-paths "$DISKO_SCRIPT" "$NIXOS_SYSTEM" \
          --target-host "root@localhost" \
          --ssh-port "$PORT" \
          --post-kexec-ssh-port "$PORT" \
          --ssh-option StrictHostKeyChecking=no \
          --ssh-option UserKnownHostsFile=/dev/null \
          -i "$KEY" \
          --phases kexec,disko

        echo
        echo "===== STAGE 2b: stage install-time signing keys into the installer ====="
        # Copy the committed test keys to the paths the config declares
        # (generationKeyFile / uki.{keyFile,certFile}). The install-time signer
        # reads them from here; they are NEVER a Nix derivation input.
        ssh -i "$KEY" -p "$PORT" "''${SSH_OPTS[@]}" root@localhost \
          "mkdir -p $STAGE_DIR && chmod 700 $STAGE_DIR"
        scp -i "$KEY" -P "$PORT" "''${SSH_OPTS[@]}" "$GEN_KEY" "root@localhost:$STAGE_DIR/insecure-test-gen.key"
        scp -i "$KEY" -P "$PORT" "''${SSH_OPTS[@]}" "$SB_KEY"  "root@localhost:$STAGE_DIR/insecure-test-sb-db.key"
        scp -i "$KEY" -P "$PORT" "''${SSH_OPTS[@]}" "$SB_CRT"  "root@localhost:$STAGE_DIR/insecure-test-sb-db.crt"
        ssh -i "$KEY" -p "$PORT" "''${SSH_OPTS[@]}" root@localhost \
          "chmod 600 $STAGE_DIR/insecure-test-gen.key $STAGE_DIR/insecure-test-sb-db.key && chmod 644 $STAGE_DIR/insecure-test-sb-db.crt"
        echo "✓ install-time signing keys staged at $STAGE_DIR (read by PATH, not a derivation)"

        echo
        echo "===== STAGE 2c: nixos-anywhere install (NMBL signs UKI + sidecars in place) ====="
        nixos-anywhere \
          --store-paths "$DISKO_SCRIPT" "$NIXOS_SYSTEM" \
          --target-host "root@localhost" \
          --ssh-port "$PORT" \
          --post-kexec-ssh-port "$PORT" \
          --ssh-option StrictHostKeyChecking=no \
          --ssh-option UserKnownHostsFile=/dev/null \
          -i "$KEY" \
          --phases install

        echo "Powering off rescue VM..."
        ssh -i "$KEY" -p "$PORT" "''${SSH_OPTS[@]}" root@localhost 'poweroff -f' 2>/dev/null || true
        i=0; while kill -0 "$QEMU_PID" 2>/dev/null && (( i < 30 )); do sleep 1; i=$((i+1)); done
        kill -KILL "$QEMU_PID" 2>/dev/null || true
        QEMU_PID=""

        echo
        echo "===== INSTALL COMPLETE: signed disk ready ====="
        echo "SIGNED_DISK=$WORK_DIR/disk1.qcow2"
        echo "$WORK_DIR/disk1.qcow2"
      '';
    };
in
{
  inherit mkSbInstaller;
}
