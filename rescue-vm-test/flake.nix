{
  description = "Boot SystemRescue in QEMU with two empty 16GB virtio disks and inject an SSH pubkey via autorun";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};

      sysrescueVersion = "13.00";
      sysrescueLabel = "RESCUE1300";

      systemRescueIso = pkgs.fetchurl {
        url = "https://fastly-cdn.system-rescue.org/releases/${sysrescueVersion}/systemrescue-${sysrescueVersion}-amd64.iso";
        hash = "sha256-R9I0i7y7rt4U5MkzlbijZYX2GAOEqcvpc7t1OkTKQKA=";
      };

      # Pull kernel + initrd out of the ISO so we can pass our own kernel args.
      # Booting via `-kernel/-initrd` is the only way to set `ar_source=` without
      # rewriting the ISO's bootloader config.
      systemRescueBoot =
        pkgs.runCommand "systemrescue-${sysrescueVersion}-boot"
          {
            nativeBuildInputs = [ pkgs.libarchive ];
          }
          ''
            mkdir -p $out
            bsdtar -xf ${systemRescueIso} -C . \
              sysresccd/boot/x86_64/vmlinuz \
              sysresccd/boot/x86_64/sysresccd.img \
              sysresccd/boot/intel_ucode.img \
              sysresccd/boot/amd_ucode.img
            cp sysresccd/boot/x86_64/vmlinuz $out/vmlinuz
            # Linux supports a stacked initrd via plain concatenation.
            cat sysresccd/boot/intel_ucode.img \
                sysresccd/boot/amd_ucode.img \
                sysresccd/boot/x86_64/sysresccd.img > $out/initrd
          '';

      runScript = pkgs.writeShellApplication {
        name = "rescue-vm-test";
        runtimeInputs = with pkgs; [
          qemu_kvm
          dosfstools
          mtools
          coreutils
        ];
        text = ''
          set -euo pipefail

          PUBKEY_FILE=""
          WORK_DIR="$PWD/.rescue-vm-test"
          SSH_PORT="2222"
          MEMORY="2048"
          CORES="4"
          DISK_SIZE="16G"

          usage() {
            cat <<EOF
          Usage: rescue-vm-test [options]
            --pubkey-file PATH   SSH public-key file to inject (default: auto-detect)
            --work-dir PATH      Where to place disks (default: $PWD/.rescue-vm-test)
            --port N             Host port forwarded to guest 22 (default: 2222)
            --memory MB          VM memory (default: 2048)
            --cores N            VM vCPUs (default: 4)
            --disk-size SIZE     Each data disk's size (default: 16G)
            -h, --help           Show this help

          Boots SystemRescue ${sysrescueVersion} with:
            - Two empty qcow2 virtio disks (guest: /dev/vda, /dev/vdb)
            - One auxiliary virtio disk holding an autorun script (guest: /dev/vdc)
              The script installs your SSH pubkey into /root/.ssh/authorized_keys
              and (re)starts sshd.

          Once it boots, ssh in:
            ssh -p \$SSH_PORT root@localhost
          Verify the two empty disks from inside the VM:
            lsblk -d -o NAME,SIZE,TYPE
          (expect vda and vdb both 16G, with no children/partitions)
          EOF
          }

          while [[ $# -gt 0 ]]; do
            case "$1" in
              --pubkey-file)  PUBKEY_FILE="$2"; shift 2;;
              --work-dir)     WORK_DIR="$2"; shift 2;;
              --port)         SSH_PORT="$2"; shift 2;;
              --memory)       MEMORY="$2"; shift 2;;
              --cores)        CORES="$2"; shift 2;;
              --disk-size)    DISK_SIZE="$2"; shift 2;;
              -h|--help)      usage; exit 0;;
              *) echo "Unknown option: $1" >&2; usage >&2; exit 1;;
            esac
          done

          if [[ -z "$PUBKEY_FILE" ]]; then
            for c in "$HOME/.ssh/authorized_keys" "$HOME/.ssh/id_ed25519.pub" "$HOME/.ssh/id_rsa.pub"; do
              if [[ -f "$c" ]]; then
                PUBKEY_FILE="$c"
                break
              fi
            done
          fi
          if [[ -z "$PUBKEY_FILE" || ! -f "$PUBKEY_FILE" ]]; then
            echo "Error: no SSH pubkey found. Pass --pubkey-file PATH." >&2
            exit 1
          fi

          PUBKEYS=$(grep -E '^(ssh-|ecdsa-|sk-)' "$PUBKEY_FILE" || true)
          if [[ -z "$PUBKEYS" ]]; then
            echo "Error: no valid SSH pubkeys in $PUBKEY_FILE" >&2
            exit 1
          fi
          KEY_COUNT=$(printf '%s\n' "$PUBKEYS" | wc -l)
          echo "Using $KEY_COUNT SSH key(s) from $PUBKEY_FILE"

          mkdir -p "$WORK_DIR"
          cd "$WORK_DIR"

          for n in 1 2; do
            f="disk$n.qcow2"
            if [[ ! -f "$f" ]]; then
              echo "Creating $f ($DISK_SIZE, empty, no partition table)"
              qemu-img create -f qcow2 "$f" "$DISK_SIZE" >/dev/null
            else
              echo "Reusing existing $f"
            fi
          done

          AR_IMG="autorun.img"
          echo "Building autorun aux disk ($AR_IMG)"
          rm -f "$AR_IMG"
          truncate -s 16M "$AR_IMG"
          mkfs.vfat -n SYSRESCUE "$AR_IMG" >/dev/null

          AR_TMP=$(mktemp)
          {
            printf '%s\n' '#!/bin/bash'
            printf '%s\n' 'set -e'
            printf '%s\n' 'mkdir -p /root/.ssh'
            printf '%s\n' 'chmod 700 /root/.ssh'
            printf '%s\n' "cat > /root/.ssh/authorized_keys <<'KEYS'"
            printf '%s\n' "$PUBKEYS"
            printf '%s\n' 'KEYS'
            printf '%s\n' 'chmod 600 /root/.ssh/authorized_keys'
            printf '%s\n' 'systemctl restart sshd.service 2>/dev/null || rc-service sshd restart 2>/dev/null || true'
            printf '%s\n' 'touch /root/.ssh/.rescue-vm-test-installed'
          } > "$AR_TMP"
          chmod +x "$AR_TMP"
          mcopy -i "$AR_IMG" "$AR_TMP" ::autorun
          rm -f "$AR_TMP"

          KARGS="archisobasedir=sysresccd archisolabel=${sysrescueLabel} iomem=relaxed console=ttyS0,115200n8 ar_source=/dev/vdc ar_ignorefail nofirewall"

          echo
          echo "=== Booting SystemRescue ${sysrescueVersion} ==="
          echo "  Memory:    $MEMORY MB,  Cores: $CORES"
          echo "  Data:      vda=disk1.qcow2 ($DISK_SIZE), vdb=disk2.qcow2 ($DISK_SIZE)"
          echo "  Autorun:   vdc=autorun.img (installs SSH key)"
          echo "  SSH:       ssh -p $SSH_PORT root@localhost   (after sshd starts)"
          echo "  Console:   serial on stdio; Ctrl+A then X to quit QEMU"
          echo

          exec qemu-system-x86_64 \
            -machine q35,accel=kvm:tcg \
            -cpu max \
            -m "$MEMORY" \
            -smp "$CORES" \
            -kernel ${systemRescueBoot}/vmlinuz \
            -initrd ${systemRescueBoot}/initrd \
            -append "$KARGS" \
            -drive file=${systemRescueIso},media=cdrom,readonly=on,if=none,id=cd0 \
            -device ide-cd,drive=cd0,bus=ide.0 \
            -drive file=disk1.qcow2,format=qcow2,if=virtio \
            -drive file=disk2.qcow2,format=qcow2,if=virtio \
            -drive file="$AR_IMG",format=raw,if=virtio \
            -netdev "user,id=net0,hostfwd=tcp::$SSH_PORT-:22" \
            -device virtio-net-pci,netdev=net0 \
            -nographic \
            -serial mon:stdio
        '';
      };
    in
    {
      packages.${system} = {
        default = runScript;
        inherit systemRescueIso systemRescueBoot;
      };

      apps.${system}.default = {
        type = "app";
        program = "${runScript}/bin/rescue-vm-test";
      };
    };
}
