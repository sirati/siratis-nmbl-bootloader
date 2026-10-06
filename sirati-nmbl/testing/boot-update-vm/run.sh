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

# build_artifacts KEY [PREVIOUS]: NMBL trusting KEY (and PREVIOUS, for a
# trust-key transition slot that PREVIOUS signs).
build_artifacts() {
  key="$1"; previous="${2:-}"
  public_hash=$(nix hash path --type sha256 "$operator/$key.pub")
  previous_args=()
  if [ -n "$previous" ]; then
    previous_args=(--argstr previousKeyPath "$operator/$previous.pub"
      --argstr previousKeyHash "$(nix hash path --type sha256 "$operator/$previous.pub")")
  fi
  nix build --no-link --print-out-paths \
    --file "$source_tree/testing/boot-update-vm/eval.nix" \
    --argstr source "$source_tree" \
    --argstr publicKeyPath "$operator/$key.pub" \
    --argstr publicKeyHash "$public_hash" "${previous_args[@]}"
}
artifacts_a=$(build_artifacts A)
artifacts_transition=$(build_artifacts B A)
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
cp "$artifacts_a/grub.cfg" "$work/grub.cfg"
"$grub/bin/grub-mkstandalone" -O x86_64-efi -o "$work/BOOTX64.EFI" \
  "boot/grub/grub.cfg=$work/grub.cfg"
truncate -s 64M "$work/esp.img"
mkfs.vfat -F 32 "$work/esp.img"
mmd -i "$work/esp.img" ::/EFI ::/EFI/BOOT
mcopy -i "$work/esp.img" "$work/BOOTX64.EFI" ::/EFI/BOOT/BOOTX64.EFI

# The root disk holds one generation: its profile links plus the kernel and
# initrd NMBL verifies and kexecs (the rest of its closure is not needed).
toplevel=$(readlink -f "$artifacts_a/toplevel")
gen_id=$(basename "$toplevel")
mkdir -p "$work/root/boot" "$work/root/nix/store" "$work/root/nix/var/nix/profiles"
cp -a "$toplevel" "$work/root/nix/store/"
for member in kernel initrd; do
  target=$(readlink -f "$toplevel/$member")
  mkdir -p "$work/root$(dirname "$target")"
  cp "$target" "$work/root$target"
done
ln -s "$toplevel" "$work/root/nix/var/nix/profiles/system-1-link"
ln -s system-1-link "$work/root/nix/var/nix/profiles/system"
truncate -s "$(( $(du -sm "$work/root" | cut -f1) + 128 ))M" "$work/root.img"
mkfs.ext4 -q -F -d "$work/root" "$work/root.img"

# boot_slot SLOT KEY [EXPECT [CONFIG_SIG]]: boot the current boot store with
# the generation signed by KEY, optionally replacing the slot's config.sig.
boot_slot() {
  slot="$1"; key="$2"; expect="${3:-boot}"; config_sig="${4:-}"
  name="$slot-$expect"; disk="$work/boot-$name.img"; vars="$work/vars-$name.fd"
  stage="$work/boot-stage-$name"
  rm -rf "$stage"
  cp -a "$work/boot" "$stage"
  chmod -R u+w "$stage"
  mkdir -p "$stage/nmbl/sigs/$gen_id"
  for member in kernel initrd; do
    "$signer/bin/nmbl-sign" sign --key "$key" --domain "gen-$member" \
      --out "$stage/nmbl/sigs/$gen_id/$member.sig" "$(readlink -f "$toplevel/$member")"
  done
  if [ -n "$config_sig" ]; then
    cp "$config_sig" "$stage/nmbl-boot-sets/$slot/config.sig"
  fi
  truncate -s "$(( $(du -sm "$stage" | cut -f1) + 128 ))M" "$disk"
  mkfs.ext4 -q -F -d "$stage" "$disk"
  cp "$ovmf_vars_template" "$vars"
  chmod u+w "$vars"
  python3 @harness@ --qemu "$qemu" --ovmf-code "$ovmf_code" --ovmf-vars "$vars" \
    --esp "$work/esp.img" --boot "$disk" --root "$work/root.img" --slot "$slot" \
    --expect "$expect" --log "$work/boot-$name.log"
}
boot_slot A "$operator/A.key"

# Key A authorizes the complete B trust-artifact replacement; its NMBL also
# trusts A, which signed its config. Once B boots, only B may authorize the
# following A set, whose NMBL trusts B alone.
start_service "$operator/A.pub"
activate "$artifacts_transition" B "$operator/A.key" "$operator/A.pub" transition-b
stop_service
boot_slot B "$operator/B.key"
start_service "$operator/B.pub"
activate "$artifacts_b" A "$operator/B.key" "$operator/B.pub" generation-b-a
if "$updater/bin/nmbl-boot-update" request "$work/update.sock" \
  "$work/spool/transition-b" "$operator/B.pub"; then
  echo "old key-A bundle was accepted after rotation" >&2; exit 1
fi
stop_service
boot_slot A "$operator/B.key"
# A failed check of the slot config's signature stops the boot: the config
# signed under any other domain (here the boot-set artifact domain the
# update bundle itself uses) is refused before any generation is touched.
"$signer/bin/nmbl-sign" sign --key "$operator/B.key" --domain boot-set-artifact \
  --out "$work/wrong-domain-config.sig" "$work/boot/nmbl-boot-sets/A/config"
boot_slot A "$operator/B.key" refuse "$work/wrong-domain-config.sig"

nix-store -qR "$artifacts_a" "$artifacts_transition" "$artifacts_b" "$updater" "$signer" > "$work/closure-paths"
mapfile -t closure_roots < "$work/closure-paths"
cp "$operator/marker" "$operator/marker-a"
python3 "$source_tree/testing/scan-private-key.py" "$operator/A.key" "$operator/marker-a" \
  "${closure_roots[@]}" "$work/boot" "$work/esp.img" "$work"/boot-*.img "$work/root.img"
python3 "$source_tree/testing/scan-private-key.py" "$operator/B.key" "$operator/marker" \
  "${closure_roots[@]}" "$work/boot" "$work/esp.img" "$work"/boot-*.img "$work/root.img"
echo NMBL_BOOT_UPDATE_VM_PASS
