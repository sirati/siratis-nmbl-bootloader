set -euo pipefail
umask 077

# Boots a Hetzner-shape NMBL host from a real BIOS disk: GRUB -> NMBL on a
# vfat ESP -> Btrfs root with the normal Nix store and three system profiles.
# Stateful tracking must roll back from a failing newest generation to an
# older one, and once every candidate failed, `boot.nmbl.rescue.automatic`
# alone decides: true enters the signed-in rescue with recovery SSH, false
# opens the emergency menu.

work=$(mktemp -d "${TMPDIR:-/tmp}/nmbl-stateful-vm.XXXXXXXX")
cleanup() {
  status=$?
  if [[ $status -ne 0 ]]; then
    for log in "$work"/*/serial.log; do
      [[ -f "$log" ]] || continue
      kept="${TMPDIR:-/tmp}/nmbl-stateful-vm-failed-$(basename "$(dirname "$log")").log"
      cp "$log" "$kept" && echo "serial transcript kept at $kept" >&2
    done
  fi
  chmod -R u+w "$work" 2>/dev/null || true
  if [[ $status -ne 0 && ${NMBL_KEEP_FAILED:-0} = 1 ]]; then
    echo "work directory kept at $work" >&2
  else
    rm -rf "$work"
  fi
}
trap cleanup EXIT INT TERM

ssh_key="$work/rescue-client-ed25519"
ssh-keygen -q -t ed25519 -N '' -f "$ssh_key"
ssh_hash=$(nix hash path --type sha256 "$ssh_key.pub")

build_disk() {
  local automatic=$2 dir="$work/$1" artifacts
  mkdir -p "$dir"
  artifacts=$(nix build --no-link --print-out-paths \
    --file @source@/testing/stateful-bios-host/eval.nix --argstr source @source@ \
    --arg automatic "$automatic" \
    --argstr sshPublicKeyPath "$ssh_key.pub" --argstr sshPublicKeyHash "$ssh_hash")

  # /boot: exactly what installBootLoader stages for this host.
  local boot="$dir/boot"
  mkdir -p "$boot/grub" "$boot/nmbl" "$boot/nmbl-test"
  cp "$artifacts/grub.cfg" "$boot/grub/grub.cfg"
  cp "$artifacts/nmbl-kernel" "$boot/nmbl-kernel"
  cp "$artifacts/nmbl-initrd" "$boot/nmbl-initrd"
  cp "$artifacts/config.toml" "$boot/nmbl/config.toml"
  cp "$artifacts/rescue.sfs" "$boot/nmbl-rescue.sfs"
  "$artifacts/nmbl-init/bin/nmbl-init" --init-state "$boot/nmbl" >/dev/null
  # Every generation fails before multi-user.target.
  touch "$boot/nmbl-test/fail-1" "$boot/nmbl-test/fail-2" "$boot/nmbl-test/fail-3"
  chmod -R u+w "$boot"

  # Btrfs root: @nix carries the store, the Nix database and the profiles.
  local root="$dir/root"
  mkdir -p "$root/@root" "$root/@nix/store" "$root/@nix/var/nix/profiles"
  xargs -a "$artifacts/closure/store-paths" cp -a -t "$root/@nix/store"
  # The target Nix database, so the booted system sees a registered store.
  nix-store --store "local?root=$root/nix-db-root" --load-db \
    < "$artifacts/closure/registration"
  mkdir -p "$root/@nix/var/nix/db"
  cp -a "$root/nix-db-root/nix/var/nix/db/." "$root/@nix/var/nix/db/"
  rm -rf "$root/nix-db-root"
  for n in 1 2 3; do
    ln -s "$(readlink -f "$artifacts/system-$n")" "$root/@nix/var/nix/profiles/system-$n-link"
  done
  ln -s system-3-link "$root/@nix/var/nix/profiles/system"
  mkdir -p "$root/@root/etc" "$root/@root/nix" "$root/@root/boot"
  chmod -R u+w "$root"

  python3 @disk@ --grub @grub@ --out "$dir/disk.raw" --boot "$boot" --root "$root"
  rm -rf "$root"
}

build_disk automatic true
build_disk menu false

ssh_port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')
python3 @harness@ --qemu @qemu@ --passt @passt@ --ssh @ssh@ \
  --disk "$work/automatic/disk.raw" --transcript "$work/automatic/serial.log" \
  --ssh-port "$ssh_port" --ssh-key "$ssh_key" --expect rescue
python3 @harness@ --qemu @qemu@ --passt @passt@ --ssh @ssh@ \
  --disk "$work/menu/disk.raw" --transcript "$work/menu/serial.log" \
  --ssh-port "$ssh_port" --ssh-key "$ssh_key" --expect menu
echo "NMBL stateful BIOS/GRUB Btrfs host VM test passed"
