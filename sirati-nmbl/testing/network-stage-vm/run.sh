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
ssh_private_key="$operator_root/rescue-client-ed25519"
ssh_public_key="$ssh_private_key.pub"
marker="NMBL_PRIVATE_MARKER_$(od -An -tx1 -N24 /dev/urandom | tr -d ' \n')"
printf '%s\n' "$marker" > "$operator_root/do-not-export.marker"
"$signer/bin/nmbl-sign" keygen \
  --alg ml-dsa-65 \
  --out-priv "$private_key" \
  --out-pub "$public_key"
chmod 0600 "$private_key"
ssh-keygen -q -t ed25519 -N '' -f "$ssh_private_key"
chmod 0600 "$ssh_private_key"

public_hash=$(nix hash path --type sha256 "$public_key")
ssh_public_hash=$(nix hash path --type sha256 "$ssh_public_key")
artifacts=$(nix build --no-link --print-out-paths \
  --file "$source_tree/testing/network-stage-vm/eval.nix" \
  --argstr source "$source_tree" \
  --argstr publicKeyPath "$public_key" \
  --argstr publicKeyHash "$public_hash" \
  --argstr sshPublicKeyPath "$ssh_public_key" \
  --argstr sshPublicKeyHash "$ssh_public_hash")

stage="$work_root/boot"
mkdir -p "$stage/nmbl"
cp "$artifacts/config.toml" "$stage/nmbl/config.toml"
ssh-keygen -q -t ed25519 -N '' -f "$stage/rescue-host-ed25519"
chmod 0600 "$stage/rescue-host-ed25519"

"$signer/bin/nmbl-sign" sign \
  --key "$private_key" \
  --domain boot-config \
  --out "$stage/nmbl/config.toml.sig" \
  "$stage/nmbl/config.toml"

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
  debugfs -w -R "set_inode_field /rescue-host-ed25519 uid 0" "$disk" >/dev/null
  debugfs -w -R "set_inode_field /rescue-host-ed25519 gid 0" "$disk" >/dev/null
  e2fsck -fn "$disk"
}

make_disk "$stage" "$work_root/good.img"
cp -a "$stage" "$work_root/tampered-tree"
printf 'NMBL_TAMPER' >> "$work_root/tampered-tree/nmbl/network.erofs"
make_disk "$work_root/tampered-tree" "$work_root/tampered.img"
cp -a "$stage" "$work_root/unsigned-tree"
rm "$work_root/unsigned-tree/nmbl/network.erofs.sig"
make_disk "$work_root/unsigned-tree" "$work_root/unsigned.img"

# Rebuild and correctly sign an EROFS whose data-only policy is malformed.
# This exercises the production signature and mount path, then proves the
# stage-1 parser fails closed before rescue networking or sshd can start.
mkdir "$work_root/malformed-root"
fsck.erofs --extract="$work_root/malformed-root" "$artifacts/network.erofs"
cat > "$work_root/malformed-root/etc/nmbl-network/network.conf" <<'EOF'
version 2
address-family dual-stack
profile interface eth0
address 4 999.0.0.1/24
end
EOF
mkfs.erofs -zlz4hc "$work_root/malformed.erofs" "$work_root/malformed-root"
cp -a "$stage" "$work_root/malformed-tree"
cp "$work_root/malformed.erofs" "$work_root/malformed-tree/nmbl/network.erofs"
"$signer/bin/nmbl-sign" sign \
  --key "$private_key" \
  --domain network-stage \
  --out "$work_root/malformed-tree/nmbl/network.erofs.sig" \
  "$work_root/malformed-tree/nmbl/network.erofs"
make_disk "$work_root/malformed-tree" "$work_root/malformed.img"

verify_disk_file() {
  local disk="$1"
  local disk_path="$2"
  local expected="$3"
  local output
  output="$work_root/disk-check-$(basename "$disk")-$(basename "$disk_path")"
  debugfs -R "dump $disk_path $output" "$disk" >/dev/null
  cmp "$expected" "$output"
}

for disk in good tampered unsigned malformed; do
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
  --key "$ssh_private_key" \
  --marker "$marker" \
  --target-list "$work_root/closure-paths" \
  "$source_tree" "$artifacts" "$stage" "$work_root/initrd" \
  "$work_root/rescue" "$work_root/network" "$work_root/disk" \
  "$work_root/malformed-root" "$work_root/malformed-tree" \
  "$work_root/malformed.erofs" "$work_root/good.img" \
  "$work_root/tampered.img" "$work_root/unsigned.img" \
  "$work_root/malformed.img"

rm -f "$private_key" "$operator_root/do-not-export.marker"
test ! -e "$private_key"

ssh_port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')

for scenario in good tampered unsigned malformed; do
  mode=invalid
  test "$scenario" = good && mode=good
  python3 "$harness" boot \
    --qemu "$qemu" \
    --passt @passt@ \
    --kernel "$artifacts/kernel" \
    --initrd "$artifacts/initrd" \
    --disk "$work_root/$scenario.img" \
    --transcript "$work_root/$scenario.log" \
    --ssh @ssh@ \
    --ssh-port "$ssh_port" \
    --ssh-key "$ssh_private_key" \
    --ssh-host-key "$stage/rescue-host-ed25519.pub" \
    --mode "$mode"
done

echo "NMBL signed network-stage VM test passed"
