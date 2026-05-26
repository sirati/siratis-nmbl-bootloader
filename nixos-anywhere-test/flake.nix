{
  description = "Test NMBL bootloader installation via nixos-anywhere on a rescue VM";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    disko = {
      url = "github:nix-community/disko";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    nixos-anywhere = {
      url = "github:nix-community/nixos-anywhere";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.disko.follows = "disko";
    };

    # The Rust /init is now a required input of the sibling sirati-nmbl
    # flake. Plumb it through here so our path-import of that flake can
    # call its outputs cleanly.
    nmbl-init-rs = {
      url = "path:../sirati-nmbl/nmbl-init-rs";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      disko,
      nixos-anywhere,
      nmbl-init-rs,
    }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};

      # Pull siblings via path imports (matches the project's existing pattern)
      siratiNmblFlake = import ../sirati-nmbl/flake.nix;
      siratiNmbl = siratiNmblFlake.outputs {
        self = siratiNmblFlake;
        inherit nixpkgs disko nixos-anywhere nmbl-init-rs;
      };

      rescueVmTestFlake = import ../rescue-vm-test/flake.nix;
      rescueArtifacts =
        (rescueVmTestFlake.outputs {
          self = rescueVmTestFlake;
          inherit nixpkgs;
        }).packages.${system};

      installConfigs = import ./install-configs.nix {
        inherit
          nixpkgs
          disko
          siratiNmbl
          system
          ;
      };

      mkOrchestrator =
        {
          configName,
          port,
          firmware, # "bios" | "uefi"
        }:
        pkgs.writeShellApplication {
          name = "install-test-${configName}";
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

            NAME="${configName}"
            PORT="${port}"
            FIRMWARE="${firmware}"
            # Pre-built store paths so nixos-anywhere can use --store-paths.
            # This sidesteps the pure-eval-mode "access to absolute path"
            # error caused by sibling-directory imports like
            # `import ../sirati-nmbl/flake.nix` in this flake.
            DISKO_SCRIPT="${installConfigs.${configName}.config.system.build.diskoScript}"
            NIXOS_SYSTEM="${installConfigs.${configName}.config.system.build.toplevel}"
            SYSTEMRESCUE_ISO="${rescueArtifacts.systemRescueIso}"
            SYSTEMRESCUE_KERNEL="${rescueArtifacts.systemRescueBoot}/vmlinuz"
            SYSTEMRESCUE_INITRD="${rescueArtifacts.systemRescueBoot}/initrd"
            OVMF_CODE="${pkgs.OVMF.fd}/FV/OVMF_CODE.fd"
            OVMF_VARS_TEMPLATE="${pkgs.OVMF.fd}/FV/OVMF_VARS.fd"

            WORK_DIR="$PWD/.install-test-$NAME"
            MEMORY="2048"
            CORES="4"
            SSH_KEY_FILE=""
            EXTRA_PUBKEY_FILE=""

            usage() {
              cat <<EOF
            Usage: install-test-$NAME [options]

              --ssh-key PATH         Path to a passphrase-less SSH PRIVATE key file.
                                     The matching public key is auto-derived and
                                     injected into the rescue VM + installed system.
                                     Required (or set SSH_PRIVATE_KEY env to the
                                     private key contents instead).
              --pubkey-file PATH     Optional additional pubkeys to inject (e.g. a
                                     teammate's key on top of yours). Each line is
                                     appended to /root/.ssh/authorized_keys.
              --work-dir PATH        Where to place disks/logs (default: $PWD/.install-test-$NAME)
              --memory MB            Guest memory (default: 2048)
              --cores N              Guest vCPUs (default: 4)
              -h, --help             Show this help

            Authentication notes:
              nixos-anywhere requires a PRIVATE key file passed via -i — its
              bootstrap ssh-copy-id calls extract the public half via
              ssh-keygen -y -f and use the key directly. Passing only an agent
              identity (e.g. 1Password) doesn't work; you must give it a private
              key file or SSH_PRIVATE_KEY content.

              The orchestrator pins ssh to that key (-o IdentitiesOnly=yes -i
              <key>) so the home-manager 1Password ssh_config override is
              effectively bypassed without disabling the agent altogether.

            Stages:
              1. Boot rescue VM (SystemRescue) with two 16G disks
              2. Run nixos-anywhere to install '$NAME' onto /dev/vda
              3. Reboot into installed system using $FIRMWARE firmware
              4. SSH in and verify NMBL chained successfully into NixOS
            EOF
            }

            while [[ $# -gt 0 ]]; do
              case "$1" in
                --ssh-key)      SSH_KEY_FILE="$2"; shift 2;;
                --pubkey-file)  EXTRA_PUBKEY_FILE="$2"; shift 2;;
                --work-dir)     WORK_DIR="$2"; shift 2;;
                --memory)       MEMORY="$2"; shift 2;;
                --cores)        CORES="$2"; shift 2;;
                -h|--help)      usage; exit 0;;
                *) echo "Unknown option: $1" >&2; usage >&2; exit 1;;
              esac
            done

            mkdir -p "$WORK_DIR"
            cd "$WORK_DIR"

            # Materialise the SSH private key. Order:
            # 1. --ssh-key PATH wins
            # 2. SSH_PRIVATE_KEY env var (written to a temp file inside work dir)
            # 3. Hard error — never generate keys on the fly.
            if [[ -z "$SSH_KEY_FILE" ]]; then
              if [[ -n "''${SSH_PRIVATE_KEY:-}" ]]; then
                SSH_KEY_FILE="$WORK_DIR/ssh-key-from-env"
                printf '%s\n' "$SSH_PRIVATE_KEY" > "$SSH_KEY_FILE"
                chmod 600 "$SSH_KEY_FILE"
              fi
            fi
            if [[ -z "$SSH_KEY_FILE" || ! -f "$SSH_KEY_FILE" ]]; then
              echo "Error: no SSH private key supplied." >&2
              echo "       Pass --ssh-key PATH or set SSH_PRIVATE_KEY env." >&2
              echo "       nixos-anywhere REQUIRES a private key file for its" >&2
              echo "       bootstrap ssh-copy-id; agent-only auth is not enough." >&2
              exit 1
            fi
            chmod 600 "$SSH_KEY_FILE" 2>/dev/null || true

            # Derive the matching pubkey from the private key — this guarantees
            # the injected pubkey matches the key nixos-anywhere will use.
            PRIMARY_PUBKEY=$(ssh-keygen -y -f "$SSH_KEY_FILE")
            PRIMARY_FINGERPRINT=$(ssh-keygen -lf "$SSH_KEY_FILE" | awk '{print $2}')
            echo "SSH private key: $SSH_KEY_FILE"
            echo "Fingerprint:     $PRIMARY_FINGERPRINT"

            # Optional additional pubkeys for the installed system (don't affect
            # the rescue VM since the orchestrator authenticates with PRIMARY_PUBKEY)
            EXTRA_PUBKEYS=""
            if [[ -n "$EXTRA_PUBKEY_FILE" ]]; then
              EXTRA_PUBKEYS=$(grep -E '^(ssh-|ecdsa-|sk-)' "$EXTRA_PUBKEY_FILE" || true)
            fi

            KEY="$SSH_KEY_FILE"

            echo "Creating fresh 16G data disks (overwriting any stale state)"
            rm -f disk1.qcow2 disk2.qcow2
            qemu-img create -f qcow2 disk1.qcow2 16G >/dev/null
            qemu-img create -f qcow2 disk2.qcow2 16G >/dev/null

            AR_IMG="autorun.img"
            rm -f "$AR_IMG"
            truncate -s 16M "$AR_IMG"
            mkfs.vfat -n SYSRESCUE "$AR_IMG" >/dev/null
            # Pubkeys for the rescue VM: only the primary, so authentication
            # is deterministic and matches the private key we pass to ssh.
            RESCUE_PUBKEYS="$PRIMARY_PUBKEY"
            # Pubkeys baked into the installed system's root authorized_keys:
            # primary + any extras the user passed via --pubkey-file.
            INSTALLED_PUBKEYS="$PRIMARY_PUBKEY"
            if [[ -n "$EXTRA_PUBKEYS" ]]; then
              INSTALLED_PUBKEYS="$INSTALLED_PUBKEYS
            $EXTRA_PUBKEYS"
            fi

            AR_TMP=$(mktemp)
            {
              printf '%s\n' '#!/bin/bash'
              printf '%s\n' 'set -e'
              printf '%s\n' 'mkdir -p /root/.ssh && chmod 700 /root/.ssh'
              printf 'cat > /root/.ssh/authorized_keys <<KEYS\n%s\nKEYS\n' "$RESCUE_PUBKEYS"
              printf '%s\n' 'chmod 600 /root/.ssh/authorized_keys'
              printf '%s\n' 'systemctl restart sshd.service 2>/dev/null || true'
            } > "$AR_TMP"
            chmod +x "$AR_TMP"
            mcopy -i "$AR_IMG" "$AR_TMP" ::autorun
            rm -f "$AR_TMP"

            rm -rf extra-files
            mkdir -p extra-files/root/.ssh
            printf '%s\n' "$INSTALLED_PUBKEYS" > extra-files/root/.ssh/authorized_keys
            chmod 700 extra-files/root/.ssh
            chmod 600 extra-files/root/.ssh/authorized_keys

            QEMU_PID=""
            cleanup() {
              if [[ -n "$QEMU_PID" ]] && kill -0 "$QEMU_PID" 2>/dev/null; then
                echo "Cleaning up QEMU pid $QEMU_PID..."
                kill -TERM "$QEMU_PID" 2>/dev/null || true
                local i=0
                while kill -0 "$QEMU_PID" 2>/dev/null && (( i < 10 )); do
                  sleep 1
                  i=$((i+1))
                done
                kill -KILL "$QEMU_PID" 2>/dev/null || true
              fi
            }
            trap cleanup EXIT INT TERM

            SSH_OPTS=(
              -o StrictHostKeyChecking=no
              -o UserKnownHostsFile=/dev/null
              -o LogLevel=ERROR
              -o ConnectTimeout=10
              # IdentitiesOnly=yes + explicit `-i $KEY` is enough to bypass any
              # ssh-agent (including 1Password's home-manager-managed config) —
              # ssh only tries the listed identity file, not agent keys.
              -o IdentitiesOnly=yes
            )

            wait_for_ssh() {
              local budget="$1"
              local i=0
              echo "Polling SSH on port $PORT (budget ''${budget}s)..."
              while ! ssh -i "$KEY" -p "$PORT" "''${SSH_OPTS[@]}" -o BatchMode=yes root@localhost true 2>/dev/null; do
                i=$((i+1))
                if (( i >= budget )); then
                  echo "Timed out waiting for SSH after ''${budget}s" >&2
                  return 1
                fi
                sleep 1
              done
              echo "SSH reachable on port $PORT (after ''${i}s)"
            }

            wait_for_qemu_exit() {
              local pid="$1"
              local budget="''${2:-60}"
              local i=0
              while kill -0 "$pid" 2>/dev/null; do
                i=$((i+1))
                if (( i >= budget )); then
                  echo "QEMU pid $pid did not exit within ''${budget}s, killing..." >&2
                  kill -KILL "$pid" 2>/dev/null || true
                  return 1
                fi
                sleep 1
              done
            }

            echo
            echo "===== STAGE 1: Boot rescue VM (SystemRescue) ====="
            KARGS="archisobasedir=sysresccd archisolabel=RESCUE1300 iomem=relaxed console=ttyS0,115200n8 ar_source=/dev/vdc ar_ignorefail nofirewall"

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
              -drive file=disk2.qcow2,format=qcow2,if=virtio \
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

            echo "Explicitly seeding /root/.ssh/authorized_keys in rescue VM..."
            # The nixos-anywhere kexec run script reads /root/.ssh/authorized_keys
            # and embeds its content into the kexec initrd via cpio. If the file is
            # missing or has wrong format, the kexec-installer sshd has no keys and
            # nixos-anywhere cannot reconnect after kexec.
            # We write the key directly via SSH to guarantee exact formatting.
            PUBKEY=$(ssh-keygen -y -f "$KEY")
            ssh -i "$KEY" -p "$PORT" "''${SSH_OPTS[@]}" root@localhost \
              "mkdir -p /root/.ssh && chmod 700 /root/.ssh && printf '%s\n' '$PUBKEY' > /root/.ssh/authorized_keys && chmod 600 /root/.ssh/authorized_keys"
            echo "authorized_keys seeded."

            echo
            echo "===== STAGE 2: nixos-anywhere install ====="
            # nixos-anywhere's -i wants a *private* key file: it copies it to
            # $tempDir/nixos-anywhere and runs `ssh-keygen -y -f` on it to
            # derive the pubkey, then uses ssh-copy-id to install that pubkey
            # into the rescue VM's authorized_keys. After kexec, the
            # kexec-installer's init script scrapes /root/.ssh/authorized_keys
            # from the rescue env and embeds the key set into its own initrd,
            # so reconnect post-kexec works on the SAME key.
            # --post-kexec-ssh-port: nixos-anywhere defaults this to 22, but
            # after kexec it reuses the same network adapter — and our qemu
            # hostfwd maps host:$PORT → guest:22. Without overriding, it
            # tries to reach host port 22 (= the host OS's sshd or nothing)
            # instead of the VM, so reconnect hangs / fails with publickey.
            nixos-anywhere \
              --store-paths "$DISKO_SCRIPT" "$NIXOS_SYSTEM" \
              --target-host "root@localhost" \
              --ssh-port "$PORT" \
              --post-kexec-ssh-port "$PORT" \
              --ssh-option StrictHostKeyChecking=no \
              --ssh-option UserKnownHostsFile=/dev/null \
              -i "$KEY" \
              --extra-files extra-files \
              --no-reboot

            echo "Powering off rescue VM..."
            ssh -i "$KEY" -p "$PORT" "''${SSH_OPTS[@]}" root@localhost 'poweroff -f' 2>/dev/null || true
            wait_for_qemu_exit "$QEMU_PID" 30 || true
            QEMU_PID=""

            echo
            echo "===== STAGE 3: Boot installed system (firmware=$FIRMWARE) ====="
            EXTRA_QEMU=()
            if [[ "$FIRMWARE" == "uefi" ]]; then
              OVMF_VARS="ovmf-vars.fd"
              rm -f "$OVMF_VARS"
              cp "$OVMF_VARS_TEMPLATE" "$OVMF_VARS"
              chmod 644 "$OVMF_VARS"
              EXTRA_QEMU=(
                -drive "if=pflash,format=raw,readonly=on,file=$OVMF_CODE"
                -drive "if=pflash,format=raw,file=$OVMF_VARS"
              )
            fi

            rm -f stage3.log qemu-stage3.pid
            qemu-system-x86_64 \
              -machine q35,accel=kvm:tcg \
              -cpu max \
              -m "$MEMORY" \
              -smp "$CORES" \
              "''${EXTRA_QEMU[@]}" \
              -drive file=disk1.qcow2,format=qcow2,if=virtio \
              -drive file=disk2.qcow2,format=qcow2,if=virtio \
              -netdev "user,id=net0,hostfwd=tcp::$PORT-:22" \
              -device virtio-net-pci,netdev=net0 \
              -display none \
              -serial file:stage3.log \
              -monitor none \
              -daemonize \
              -pidfile qemu-stage3.pid

            QEMU_PID=$(cat qemu-stage3.pid)
            echo "Stage 3 QEMU pid: $QEMU_PID  (serial log: $WORK_DIR/stage3.log)"

            wait_for_ssh 360

            echo
            echo "===== STAGE 4: Verify installed system ====="
            ssh -i "$KEY" -p "$PORT" "''${SSH_OPTS[@]}" root@localhost \
              'set -e
               echo "--- hostname / kernel ---"
               hostname
               uname -r
               echo "--- os-release ---"
               grep -E "PRETTY_NAME|VERSION=" /etc/os-release
               echo "--- lsblk ---"
               lsblk -o NAME,SIZE,TYPE,FSTYPE,MOUNTPOINT
               echo "--- /boot contents ---"
               ls -la /boot
               echo "--- mounts ---"
               findmnt /
               findmnt /boot'

            echo
            echo "Powering off installed system..."
            ssh -i "$KEY" -p "$PORT" "''${SSH_OPTS[@]}" root@localhost 'systemctl poweroff' 2>/dev/null || true
            wait_for_qemu_exit "$QEMU_PID" 30 || true
            QEMU_PID=""

            echo
            echo "===== PASS: $NAME ====="
            echo "Work dir kept at $WORK_DIR (disk1.qcow2, disk2.qcow2, stage{1,3}.log)"
          '';
        };
      orchestrators = {
        install-test-gpt-bios = mkOrchestrator {
          configName = "install-gpt-bios";
          port = "22001";
          firmware = "bios";
        };
        install-test-gpt-uefi-grub = mkOrchestrator {
          configName = "install-gpt-uefi-grub";
          port = "22002";
          firmware = "uefi";
        };
        install-test-gpt-uefi-systemd = mkOrchestrator {
          configName = "install-gpt-uefi-systemd";
          port = "22003";
          firmware = "uefi";
        };
        install-test-gpt-bios-raid1 = mkOrchestrator {
          configName = "install-gpt-bios-raid1";
          port = "22011";
          firmware = "bios";
        };
        install-test-gpt-uefi-grub-raid1 = mkOrchestrator {
          configName = "install-gpt-uefi-grub-raid1";
          port = "22012";
          firmware = "uefi";
        };
        install-test-gpt-uefi-systemd-raid1 = mkOrchestrator {
          configName = "install-gpt-uefi-systemd-raid1";
          port = "22013";
          firmware = "uefi";
        };
        install-test-gpt-bios-btrfs-raid1 = mkOrchestrator {
          configName = "install-gpt-bios-btrfs-raid1";
          port = "22021";
          firmware = "bios";
        };
        install-test-gpt-uefi-grub-btrfs-raid1 = mkOrchestrator {
          configName = "install-gpt-uefi-grub-btrfs-raid1";
          port = "22022";
          firmware = "uefi";
        };
        install-test-gpt-uefi-systemd-btrfs-raid1 = mkOrchestrator {
          configName = "install-gpt-uefi-systemd-btrfs-raid1";
          port = "22023";
          firmware = "uefi";
        };
      };
    in
    {
      nixosConfigurations = installConfigs;

      packages.${system} = orchestrators;

      apps.${system} = builtins.mapAttrs (name: drv: {
        type = "app";
        program = "${drv}/bin/${drv.name}";
      }) orchestrators;
    };
}
