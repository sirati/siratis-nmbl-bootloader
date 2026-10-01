set -euo pipefail
umask 077

# boot.nmbl.instantBoot on the Hetzner-shape stateful BIOS/GRUB host: an
# untouched healthy boot skips the menu; a keypress during early boot (typed on
# the serial console from the GRUB hand-off, before the selector exists) shows
# the menu and holds it.

work=$(mktemp -d "${TMPDIR:-/tmp}/nmbl-instant-vm.XXXXXXXX")
cleanup() {
  status=$?
  if [[ $status -ne 0 && -f "$work/serial.log" ]]; then
    cp "$work/serial.log" "${TMPDIR:-/tmp}/nmbl-instant-vm-failed.log" \
      && echo "serial transcript kept at ${TMPDIR:-/tmp}/nmbl-instant-vm-failed.log" >&2
  fi
  chmod -R u+w "$work" 2>/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT INT TERM

ssh-keygen -q -t ed25519 -N '' -f "$work/key"
ssh_hash=$(nix hash path --type sha256 "$work/key.pub")
artifacts=$(nix build --no-link --print-out-paths \
  --file @source@/testing/stateful-bios-host/eval.nix --argstr source @source@ \
  --arg automatic false --arg instantBoot true \
  --argstr sshPublicKeyPath "$work/key.pub" --argstr sshPublicKeyHash "$ssh_hash")

boot="$work/boot"
mkdir -p "$boot/grub" "$boot/nmbl"
cp "$artifacts/grub.cfg" "$boot/grub/grub.cfg"
cp "$artifacts/nmbl-kernel" "$boot/nmbl-kernel"
cp "$artifacts/nmbl-initrd" "$boot/nmbl-initrd"
cp "$artifacts/config.toml" "$boot/nmbl/config.toml"
cp "$artifacts/rescue.sfs" "$boot/nmbl-rescue.sfs"
"$artifacts/nmbl-init/bin/nmbl-init" --init-state "$boot/nmbl" >/dev/null
chmod -R u+w "$boot"

root="$work/root"
mkdir -p "$root/@root" "$root/@nix/store" "$root/@nix/.profiles/nix/profiles"
xargs -a "$artifacts/closure/store-paths" cp -a -t "$root/@nix/store"
nix-store --store "local?root=$root/nix-db-root" --load-db < "$artifacts/closure/registration"
mkdir -p "$root/@nix/.profiles/nix/db"
cp -a "$root/nix-db-root/nix/var/nix/db/." "$root/@nix/.profiles/nix/db/"
rm -rf "$root/nix-db-root"
for n in 1 2 3; do
  ln -s "$(readlink -f "$artifacts/system-$n")" "$root/@nix/.profiles/nix/profiles/system-$n-link"
done
ln -s system-3-link "$root/@nix/.profiles/nix/profiles/system"
mkdir -p "$root/@root/etc" "$root/@root/nix" "$root/@root/boot"
chmod -R u+w "$root"

python3 @disk@ --grub @grub@ --out "$work/disk.raw" --boot "$boot" --root "$root"
rm -rf "$root"
python3 @harness@ --qemu @qemu@ --disk "$work/disk.raw" --transcript "$work/serial.log"
