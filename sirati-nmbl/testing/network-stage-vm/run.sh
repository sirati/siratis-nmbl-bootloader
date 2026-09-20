set -euo pipefail

source_tree=@source@
harness=@harness@
qemu=@qemu@
operator_root=$(mktemp -d /dev/shm/nmbl-network-operator.XXXXXX)
work_root=$(mktemp -d "${TMPDIR:-/tmp}/nmbl-network-vm.XXXXXX")
cleanup() {
  chmod -R u+w "$work_root" 2>/dev/null || true
  rm -rf "$operator_root" "$work_root"
}
trap cleanup EXIT INT TERM
chmod 0700 "$operator_root"

signer=$(nix build --no-link --print-out-paths "path:$source_tree/nmbl-init-rs#nmbl-sign")
private_key="$operator_root/image.key"
public_key="$operator_root/image.pub"
marker="NMBL_PRIVATE_MARKER_$(od -An -tx1 -N24 /dev/urandom | tr -d ' \n')"
printf '%s\n' "$marker" > "$operator_root/do-not-export.marker"
"$signer/bin/nmbl-sign" keygen \
  --alg ml-dsa-65 \
  --out-priv "$private_key" \
  --out-pub "$public_key"
chmod 0600 "$private_key"

public_hash=$(nix hash path --type sha256 "$public_key")
artifacts=$(nix build --no-link --print-out-paths \
  --file "$source_tree/testing/network-stage-vm/eval.nix" \
  --argstr source "$source_tree" \
  --argstr publicKeyPath "$public_key" \
  --argstr publicKeyHash "$public_hash")

stage="$work_root/boot"
mkdir -p "$stage/nmbl"
cp "$artifacts/config.toml" "$stage/nmbl/config.toml"
ssh-keygen -q -t ed25519 -N '' -f "$stage/rescue-host-ed25519"
chmod 0600 "$stage/rescue-host-ed25519"

installer="$artifacts/rescue-installer/bin/nmbl-install-rescue-stage"
NMBL_BOOT_ROOT="$stage" NMBL_IMAGE_KEY_FILE="$private_key" "$installer"
network_inode=$(stat -c %i "$stage/nmbl/network.erofs")
NMBL_BOOT_ROOT="$stage" NMBL_IMAGE_KEY_FILE="$private_key" "$installer"
test "$(stat -c %i "$stage/nmbl/network.erofs")" = "$network_inode"

make_disk() {
  local source_dir="$1"
  local disk="$2"
  local used_mb
  used_mb=$(du -sm "$source_dir" | cut -f1)
  truncate -s "$((used_mb + 256))M" "$disk"
  mkfs.ext4 -q -F -d "$source_dir" "$disk"
  e2fsck -fn "$disk"
}

make_disk "$stage" "$work_root/good.img"
cp -a "$stage" "$work_root/tampered-tree"
printf 'NMBL_TAMPER' >> "$work_root/tampered-tree/nmbl/network.erofs"
make_disk "$work_root/tampered-tree" "$work_root/tampered.img"
cp -a "$stage" "$work_root/unsigned-tree"
rm "$work_root/unsigned-tree/nmbl/network.erofs.sig"
make_disk "$work_root/unsigned-tree" "$work_root/unsigned.img"

verify_disk_file() {
  local disk="$1"
  local disk_path="$2"
  local expected="$3"
  local output
  output="$work_root/disk-check-$(basename "$disk")-$(basename "$disk_path")"
  debugfs -R "dump $disk_path $output" "$disk" >/dev/null
  cmp "$expected" "$output"
}

for disk in good tampered unsigned; do
  verify_disk_file "$work_root/$disk.img" /nmbl-rescue.sfs "$stage/nmbl-rescue.sfs"
  verify_disk_file "$work_root/$disk.img" /nmbl-rescue.sfs.sig "$stage/nmbl-rescue.sfs.sig"
  verify_disk_file "$work_root/$disk.img" /nmbl/config.toml "$stage/nmbl/config.toml"
done

mkdir "$work_root/initrd" "$work_root/rescue" "$work_root/network" "$work_root/disk"
(cd "$work_root/initrd" && lsinitrd --unpack "$artifacts/initrd")
unsquashfs -quiet -dest "$work_root/rescue" "$artifacts/rescue.sfs"
fsck.erofs --extract="$work_root/network" "$artifacts/network.erofs"
debugfs -R "rdump / $work_root/disk" "$work_root/good.img" >/dev/null
nix-store -qR "$artifacts" "$signer" > "$work_root/closure-paths"
python3 "$harness" scan \
  --key "$private_key" \
  --marker "$marker" \
  --target-list "$work_root/closure-paths" \
  "$source_tree" "$artifacts" "$stage" "$work_root/initrd" \
  "$work_root/rescue" "$work_root/network" "$work_root/disk" \
  "$work_root/good.img" "$work_root/tampered.img" "$work_root/unsigned.img"

rm -f "$private_key" "$operator_root/do-not-export.marker"
test ! -e "$private_key"

for scenario in good tampered unsigned; do
  mode=invalid
  test "$scenario" = good && mode=good
  python3 "$harness" boot \
    --qemu "$qemu" \
    --kernel "$artifacts/kernel" \
    --initrd "$artifacts/initrd" \
    --disk "$work_root/$scenario.img" \
    --transcript "$work_root/$scenario.log" \
    --mode "$mode"
done

echo "NMBL signed network-stage VM test passed"
