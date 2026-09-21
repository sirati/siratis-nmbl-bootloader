set -euo pipefail

source_tree=@source@
qemu=@qemu@
ovmf_code=@ovmf_code@
ovmf_vars_template=@ovmf_vars@
grub=@grub@
signer=$(nix build --no-link --print-out-paths "path:$source_tree/nmbl-init-rs#nmbl-sign")
updater=$(nix build --no-link --print-out-paths "path:$source_tree#nmbl-boot-update")
operator=$(mktemp -d /dev/shm/nmbl-boot-update.XXXXXX)
work=$(mktemp -d "${TMPDIR:-/tmp}/nmbl-boot-update-vm.XXXXXX")
cleanup() {
  test -z "${server_pid:-}" || kill "$server_pid" 2>/dev/null || true
  chmod -R u+w "$work" 2>/dev/null || true
  rm -rf "$operator" "$work"
}
trap cleanup EXIT INT TERM
chmod 0700 "$operator"

for key in A B; do
  "$signer/bin/nmbl-sign" keygen --alg ml-dsa-65 \
    --out-priv "$operator/$key.key" --out-pub "$operator/$key.pub"
done
printf 'NMBL_BOOT_UPDATE_PRIVATE_%s' "$(od -An -tx1 -N24 /dev/urandom | tr -d ' \n')" \
  > "$operator/marker"

build_artifacts() {
  key="$1"
  public_hash=$(nix hash path --type sha256 "$operator/$key.pub")
  nix build --no-link --print-out-paths \
    --file "$source_tree/testing/boot-update-vm/eval.nix" \
    --argstr source "$source_tree" \
    --argstr publicKeyPath "$operator/$key.pub" \
    --argstr publicKeyHash "$public_hash"
}
artifacts_a=$(build_artifacts A)
artifacts_b=$(build_artifacts B)

mkdir -p "$work/spool" "$work/boot"
start_service() {
  public="$1"
  "$updater/bin/nmbl-boot-update" serve "$work/update.sock" "$work/spool" \
    "$public" "$(id -u)" "$work/boot" > "$work/service.log" 2>&1 &
  server_pid=$!
  for _ in $(seq 1 100); do test -S "$work/update.sock" && return; sleep 0.05; done
  echo "boot update service did not start" >&2; exit 1
}
stop_service() {
  kill "$server_pid" 2>/dev/null || true
  wait "$server_pid" 2>/dev/null || true
  server_pid=
}
activate() {
  artifacts="$1"; slot="$2"; private="$3"; verify="$4"; name="$5"
  "$updater/bin/nmbl-boot-update" prepare "$slot" "$artifacts/source-$slot" \
    "$work/spool/$name" "$private" "$verify"
  "$updater/bin/nmbl-boot-update" request "$work/update.sock" "$work/spool/$name" "$verify"
}

start_service "$operator/A.pub"
activate "$artifacts_a" A "$operator/A.key" "$operator/A.pub" generation-a
stop_service

make_boot_disk() {
  disk="$1"
  used=$(du -sm "$work/boot" | cut -f1)
  truncate -s "$((used + 128))M" "$disk"
  mkfs.ext4 -q -F -d "$work/boot" "$disk"
}
cp "$artifacts_a/grub.cfg" "$work/grub.cfg"
"$grub/bin/grub-mkstandalone" -O x86_64-efi -o "$work/BOOTX64.EFI" \
  "boot/grub/grub.cfg=$work/grub.cfg"
truncate -s 64M "$work/esp.img"
mkfs.vfat -F 32 "$work/esp.img"
mmd -i "$work/esp.img" ::/EFI ::/EFI/BOOT
mcopy -i "$work/esp.img" "$work/BOOTX64.EFI" ::/EFI/BOOT/BOOTX64.EFI

boot_slot() {
  slot="$1"; disk="$work/boot-$slot.img"; vars="$work/vars-$slot.fd"
  make_boot_disk "$disk"
  cp "$ovmf_vars_template" "$vars"
  chmod u+w "$vars"
  python3 @harness@ --qemu "$qemu" --ovmf-code "$ovmf_code" --ovmf-vars "$vars" \
    --esp "$work/esp.img" --boot "$disk" --slot "$slot" --log "$work/boot-$slot.log"
}
boot_slot A

# Key A authorizes the complete B trust-artifact replacement. Once B boots,
# only B may authorize the following A set.
start_service "$operator/A.pub"
activate "$artifacts_b" B "$operator/A.key" "$operator/A.pub" transition-b
stop_service
boot_slot B
start_service "$operator/B.pub"
activate "$artifacts_b" A "$operator/B.key" "$operator/B.pub" generation-b-a
if "$updater/bin/nmbl-boot-update" request "$work/update.sock" \
  "$work/spool/transition-b" "$operator/B.pub"; then
  echo "old key-A bundle was accepted after rotation" >&2; exit 1
fi
stop_service
boot_slot A

nix-store -qR "$artifacts_a" "$artifacts_b" "$updater" "$signer" > "$work/closure-paths"
mapfile -t closure_roots < "$work/closure-paths"
cp "$operator/marker" "$operator/marker-a"
python3 "$source_tree/testing/scan-private-key.py" "$operator/A.key" "$operator/marker-a" \
  "${closure_roots[@]}" "$work/boot" "$work/esp.img" "$work/boot-A.img" "$work/boot-B.img"
python3 "$source_tree/testing/scan-private-key.py" "$operator/B.key" "$operator/marker" \
  "${closure_roots[@]}" "$work/boot" "$work/esp.img" "$work/boot-A.img" "$work/boot-B.img"
echo NMBL_BOOT_UPDATE_VM_PASS
