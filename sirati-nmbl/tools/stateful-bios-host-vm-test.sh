set -euo pipefail
umask 077

# Boots a Hetzner-shape NMBL host from a real BIOS disk: GRUB -> NMBL on a
# vfat ESP -> Btrfs root with the normal Nix store and three system profiles.
# Stateful tracking must roll back from a failing newest generation to an
# older one, and once every candidate failed, `boot.nmbl.rescue.automatic`
# alone decides: true enters the signed-in rescue with recovery SSH, false
# opens the emergency menu.

work=$(mktemp -d "${TMPDIR:-/tmp}/nmbl-stateful-vm.XXXXXXXX")
artifact_roots=${NMBL_TEST_ROOTS:-"$work/artifact-roots"}
mkdir -p "$artifact_roots"
cleanup() {
  status=$?
  if [[ $status -ne 0 || -e "$work/automatic/.nmbl-preserve-failure" || -e "$work/menu/.nmbl-preserve-failure" || -e "$work/automatic/.snapshot-failed-live" || -e "$work/menu/.snapshot-failed-live" ]]; then
    echo "Failed stateful test retained at $work; artifact roots $artifact_roots" >&2
    return
  fi
  chmod -R u+w "$work" 2>/dev/null || true
  rm -f "$artifact_roots/automatic" "$artifact_roots/menu"
  rm -rf "$work"
}
trap cleanup EXIT INT TERM

ssh_key="$work/rescue-client-ed25519"
ssh-keygen -q -t ed25519 -N '' -f "$ssh_key"
ssh_hash=$(nix hash path --type sha256 "$ssh_key.pub")

build_disk() {
  local automatic=$2 dir="$work/$1" artifacts
  mkdir -p "$dir"
  artifacts=$(nix build --out-link "$artifact_roots/$1" --print-out-paths \
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
  # Like install-bootloader: the tools image (nmblctl) the config pins.
  [ ! -e "$artifacts/rescue-tools.erofs" ] \
    || cp "$artifacts/rescue-tools.erofs" "$boot/nmbl/rescue-tools.erofs"
  "$artifacts/nmbl-init/bin/nmbl-init" --init-state "$boot/nmbl" >/dev/null
  # Every generation fails before multi-user.target.
  touch "$boot/nmbl-test/fail-1" "$boot/nmbl-test/fail-2" "$boot/nmbl-test/fail-3"
  chmod -R u+w "$boot"

  # Btrfs root: @nix carries the store, the Nix database and the profiles.
  local root="$dir/root"
  mkdir -p "$root/@root" "$root/@nix/store" "$root/@nix/.profiles/nix/profiles"
  xargs -a "$artifacts/closure/store-paths" cp -a -t "$root/@nix/store"
  # The target Nix database, so the booted system sees a registered store.
  nix-store --store "local?root=$root/nix-db-root" --load-db \
    < "$artifacts/closure/registration"
  mkdir -p "$root/@nix/.profiles/nix/db"
  cp -a "$root/nix-db-root/nix/var/nix/db/." "$root/@nix/.profiles/nix/db/"
  rm -rf "$root/nix-db-root"
  for n in 1 2 3; do
    ln -s "$(readlink -f "$artifacts/system-$n")" "$root/@nix/.profiles/nix/profiles/system-$n-link"
  done
  ln -s system-3-link "$root/@nix/.profiles/nix/profiles/system"
  mkdir -p "$root/@root/etc" "$root/@root/nix" "$root/@root/boot"
  chmod -R u+w "$root"

  NMBL_TEST_ROOTDIR_OWNER=@rootdirOwner@ python3 @disk@ --grub @grub@ --out "$dir/disk.raw" --boot "$boot" --root "$root"
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
