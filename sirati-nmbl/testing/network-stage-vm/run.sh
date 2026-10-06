set -euo pipefail

source_tree=@source@
harness=@harness@
qemu=@qemu@
operator_root=$(mktemp -d /dev/shm/nmbl-network-operator.XXXXXX)
work_root=$(mktemp -d "${TMPDIR:-/tmp}/nmbl-network-vm.XXXXXX")
artifact_roots=${NMBL_TEST_ROOTS:-"$work_root/artifact-roots"}
mkdir -p "$artifact_roots"
cleanup() {
  result=$?
  if [ "$result" -ne 0 ]; then
    echo "Retained failed test artifacts at $work_root and $artifact_roots" >&2
    return
  fi
  if [ -e "$work_root/.nmbl-preserve-failure" ]; then
    echo "Retained failed VM snapshot and evidence at $work_root" >&2
    return
  fi
  chmod -R u+w "$work_root" 2>/dev/null || true
  rm -f "$artifact_roots/signer" "$artifact_roots/signed" "$artifact_roots/baked-static" "$artifact_roots/baked-slaac" "$artifact_roots/native-identity" "$artifact_roots/network-rescue"
  rm -rf "$operator_root" "$work_root"
}
trap cleanup EXIT INT TERM
chmod 0700 "$operator_root"

signer=$(nix build --out-link "$artifact_roots/signer" --print-out-paths "path:$source_tree/nmbl-init-rs#nmbl-sign")
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
artifacts=$(nix build --out-link "$artifact_roots/signed" --print-out-paths \
  --file "$source_tree/testing/network-stage-vm/eval.nix" \
  --argstr source "$source_tree" \
  --argstr publicKeyPath "$public_key" \
  --argstr publicKeyHash "$public_hash" \
  --argstr sshPublicKeyPath "$ssh_public_key" \
  --argstr sshPublicKeyHash "$ssh_public_hash")

baked_artifacts=$(nix build --out-link "$artifact_roots/baked-static" --print-out-paths \
  --file "$source_tree/testing/network-stage-vm/eval.nix" --arg bakedStatic true \
  --argstr source "$source_tree" --argstr publicKeyPath "$public_key" \
  --argstr publicKeyHash "$public_hash" --argstr sshPublicKeyPath "$ssh_public_key" \
  --argstr sshPublicKeyHash "$ssh_public_hash")

slaac_artifacts=$(nix build --out-link "$artifact_roots/baked-slaac" --print-out-paths \
  --file "$source_tree/testing/network-stage-vm/eval.nix" --arg bakedSlaac true \
  --argstr source "$source_tree" --argstr publicKeyPath "$public_key" \
  --argstr publicKeyHash "$public_hash" --argstr sshPublicKeyPath "$ssh_public_key" \
  --argstr sshPublicKeyHash "$ssh_public_hash")

native_artifacts=$(nix build --out-link "$artifact_roots/native-identity" --print-out-paths \
  --file "$source_tree/testing/network-stage-vm/eval.nix" --arg nativeIdentity true \
  --argstr source "$source_tree" --argstr publicKeyPath "$public_key" \
  --argstr publicKeyHash "$public_hash" --argstr sshPublicKeyPath "$ssh_public_key" \
  --argstr sshPublicKeyHash "$ssh_public_hash")

netrescue_artifacts=$(nix build --out-link "$artifact_roots/network-rescue" --print-out-paths \
  --file "$source_tree/testing/network-stage-vm/eval.nix" --arg networkRescue true \
  --argstr source "$source_tree" --argstr publicKeyPath "$public_key" \
  --argstr publicKeyHash "$public_hash" --argstr sshPublicKeyPath "$ssh_public_key" \
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
# The installer staged the exact stage-2 image the signed config pins.
cmp "$artifacts/rescue.sfs" "$stage/nmbl-rescue.sfs"
grep -qx 'format = "erofs"' "$stage/nmbl/config.toml"
grep -qx "sha512 = \"$(sha512sum < "$artifacts/rescue.sfs" | cut -d' ' -f1)\"" "$stage/nmbl/config.toml"
grep -qx "sha512 = \"$(sha512sum < "$artifacts/network.erofs" | cut -d' ' -f1)\"" "$stage/nmbl/config.toml"
# ...and the separately pinned, signed tools image carrying nmblctl.
cmp "$artifacts/rescue-tools.erofs" "$stage/nmbl/rescue-tools.erofs"
test -s "$stage/nmbl/rescue-tools.erofs.sig"
grep -qx '\[rescue.tools\]' "$stage/nmbl/config.toml"
grep -qx 'path = "nmbl/rescue-tools.erofs"' "$stage/nmbl/config.toml"
grep -qx "sha512 = \"$(sha512sum < "$artifacts/rescue-tools.erofs" | cut -d' ' -f1)\"" "$stage/nmbl/config.toml"


make_disk() {
  local source_dir="$1"
  local disk="$2"
  local used_mb
  used_mb=$(du -sm "$source_dir" | cut -f1)
  truncate -s "$((used_mb + 256))M" "$disk"
  mkfs.ext4 -q -F -d "$source_dir" "$disk"
  # Root-owned like a real /boot: the host key, and the config path nmblctl
  # accepts as an operator path only when it and its ancestors are root's.
  for owned in /rescue-host-ed25519 / /nmbl /nmbl/config.toml; do
    debugfs -w -R "set_inode_field $owned uid 0" "$disk" >/dev/null
    debugfs -w -R "set_inode_field $owned gid 0" "$disk" >/dev/null
  done
  e2fsck -fn "$disk"
}

make_disk "$stage" "$work_root/good.img"
cp -a "$stage" "$work_root/tampered-tree"
printf 'NMBL_TAMPER' >> "$work_root/tampered-tree/nmbl/network.erofs"
make_disk "$work_root/tampered-tree" "$work_root/tampered.img"
cp -a "$stage" "$work_root/unsigned-tree"
rm -f "$work_root/unsigned-tree/nmbl/network.erofs.sig"
make_disk "$work_root/unsigned-tree" "$work_root/unsigned.img"
# A tampered tools image must be refused before it is mounted; the rescue
# itself (network stage, SSH) still comes up, only without nmblctl.
cp -a "$stage" "$work_root/tools-tampered-tree"
printf 'NMBL_TAMPER' >> "$work_root/tools-tampered-tree/nmbl/rescue-tools.erofs"
make_disk "$work_root/tools-tampered-tree" "$work_root/tools-tampered.img"

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
# NMBL's config pins the networking stage by SHA-512, so an operator
# deploying this (malformed) stage also pins and re-signs the config; only
# then does the strict profile parser get to reject it.
chmod u+w "$work_root/malformed-tree/nmbl/config.toml"
python3 - "$work_root/malformed-tree/nmbl/config.toml" \
  "$(sha512sum < "$work_root/malformed.erofs" | cut -d' ' -f1)" <<'PY'
import re, sys
path, digest = sys.argv[1:]
head, table, stage = open(path).read().rpartition("[rescue.network_stage]")
assert table, "config pins no network stage"
stage, count = re.subn(r'^sha512 = "[0-9a-f]{128}"$', f'sha512 = "{digest}"', stage, count=1, flags=re.M)
assert count == 1, "network stage carries no pin"
open(path, "w").write(head + table + stage)
PY
"$signer/bin/nmbl-sign" sign --key "$private_key" --domain boot-config \
  --out "$work_root/malformed-tree/nmbl/config.toml.sig" \
  "$work_root/malformed-tree/nmbl/config.toml"
make_disk "$work_root/malformed-tree" "$work_root/malformed.img"

# A different, validly signed rescue image in place of the pinned one (an
# older or foreign build signed with the same key): the stage-2 pin must
# refuse it before it is mounted.
# Stage 2 is shared by every host with this kernel and package set (the baked
# hosts' image is this very one), so build the other image explicitly: the
# same tree plus one file, as an older build would differ.
mkdir "$work_root/substitute-root"
fsck.erofs --extract="$work_root/substitute-root" "$artifacts/rescue.sfs"
chmod u+w "$work_root/substitute-root/etc"
printf 'an older stage-2 build\n' > "$work_root/substitute-root/etc/nmbl-substitute"
mkfs.erofs --quiet -zlz4hc "$work_root/substitute.erofs" "$work_root/substitute-root"
cp -a "$stage" "$work_root/substituted-tree"
install -m 0644 "$work_root/substitute.erofs" "$work_root/substituted-tree/nmbl-rescue.sfs"
"$signer/bin/nmbl-sign" sign --key "$private_key" --domain rescue-sfs \
  --out "$work_root/substituted-tree/nmbl-rescue.sfs.sig" \
  "$work_root/substituted-tree/nmbl-rescue.sfs"
make_disk "$work_root/substituted-tree" "$work_root/substituted.img"

verify_disk_file() {
  local disk="$1"
  local disk_path="$2"
  local expected="$3"
  local output
  output="$work_root/disk-check-$(basename "$disk")-$(basename "$disk_path")"
  debugfs -R "dump $disk_path $output" "$disk" >/dev/null
  cmp "$expected" "$output"
}

for disk in good tampered unsigned tools-tampered; do
  verify_disk_file "$work_root/$disk.img" /nmbl-rescue.sfs "$stage/nmbl-rescue.sfs"
  verify_disk_file "$work_root/$disk.img" /nmbl-rescue.sfs.sig "$stage/nmbl-rescue.sfs.sig"
  verify_disk_file "$work_root/$disk.img" /nmbl/config.toml "$stage/nmbl/config.toml"
done
verify_disk_file "$work_root/malformed.img" /nmbl-rescue.sfs "$stage/nmbl-rescue.sfs"
verify_disk_file "$work_root/malformed.img" /nmbl/config.toml "$work_root/malformed-tree/nmbl/config.toml"
verify_disk_file "$work_root/substituted.img" /nmbl-rescue.sfs "$work_root/substitute.erofs"
verify_disk_file "$work_root/substituted.img" /nmbl/config.toml "$stage/nmbl/config.toml"

verify_disk_file "$work_root/good.img" /nmbl/rescue-tools.erofs "$artifacts/rescue-tools.erofs"
if verify_disk_file "$work_root/tools-tampered.img" /nmbl/rescue-tools.erofs "$artifacts/rescue-tools.erofs"; then
  echo "tools-tampered disk carries the untampered tools image" >&2
  exit 1
fi

mkdir "$work_root/initrd" "$work_root/rescue" "$work_root/network" "$work_root/tools" "$work_root/disk"
(cd "$work_root/initrd" && lsinitrd --unpack "$artifacts/initrd")
python3 "$harness" console-image --initrd "$work_root/initrd"
fsck.erofs --extract="$work_root/rescue" "$artifacts/rescue.sfs"
fsck.erofs --extract="$work_root/network" "$artifacts/network.erofs"
fsck.erofs --extract="$work_root/tools" "$artifacts/rescue-tools.erofs"

# nmblctl (built with the signing keys) lives only in the tools image, which
# holds nothing but its closure.
test ! -e "$work_root/rescue/bin/nmblctl"
if find "$work_root/rescue/nix/store" -mindepth 1 -maxdepth 1 -name '*-nmblctl-*' | grep -q .; then
  echo "stage-2 rescue image still carries nmblctl" >&2
  exit 1
fi
test -L "$work_root/tools/bin/nmblctl"
test -x "$work_root/tools/bin/nmblctl"
test "$(find "$work_root/tools" -mindepth 1 -maxdepth 1 -printf '%f\n' | sort)" = "$(printf 'bin\nnix')"
nmblctl_path=/nix/store/$(readlink "$work_root/tools/bin/nmblctl" | cut -d/ -f4)
nix-store -qR "$nmblctl_path" | xargs -n1 basename | sort > "$work_root/tools-closure-expected"
find "$work_root/tools/nix/store" -mindepth 1 -maxdepth 1 -printf '%f\n' | sort > "$work_root/tools-closure-actual"
cmp "$work_root/tools-closure-expected" "$work_root/tools-closure-actual"

# The constrained-/boot profile must retain the actual recovery tools while
# keeping package-fetching out, and storage tools for stacks this host does
# not use (it has no Btrfs, LUKS, LVM or mdraid) out as well.
for tool in bash sshd nmbl; do
  test -x "$work_root/rescue/bin/$tool"
done
for forbidden in nix-daemon btop cryptsetup lvm mkfs.ext4 btrfs mdadm; do
  test ! -e "$work_root/rescue/bin/$forbidden"
done
if find "$work_root/rescue/nix/store" -mindepth 1 -maxdepth 1 \
  -printf '%f\n' | grep -Eq '(^|-)nix-|btop|cacert|cryptsetup|lvm|e2fsprogs|btrfs-progs|mdadm'; then
  echo "minimal rescue contains a forbidden package" >&2
  exit 1
fi

debugfs -R "rdump / $work_root/disk" "$work_root/good.img" >/dev/null
nix-store -qR "$artifacts" "$signer" > "$work_root/closure-paths"
python3 "$harness" scan \
  --key "$private_key" \
  --key "$ssh_private_key" \
  --marker "$marker" \
  --target-list "$work_root/closure-paths" \
  "$source_tree" "$artifacts" "$stage" "$work_root/initrd" \
  "$work_root/rescue" "$work_root/network" "$work_root/tools" "$work_root/disk" \
  "$work_root/malformed-root" "$work_root/malformed-tree" \
  "$work_root/malformed.erofs" "$work_root/good.img" \
  "$work_root/tampered.img" "$work_root/unsigned.img" \
  "$work_root/malformed.img" "$work_root/tools-tampered.img"

baked_stage="$work_root/baked-boot"
mkdir -p "$baked_stage/nmbl"
cp "$baked_artifacts/config.toml" "$baked_stage/nmbl/config.toml"
cp "$stage/rescue-host-ed25519" "$stage/rescue-host-ed25519.pub" "$baked_stage/"
# Match unsigned install-bootloader staging: immutable rescue artifact, mode 0644.
install -m 0644 "$baked_artifacts/rescue.sfs" "$baked_stage/nmbl-rescue.sfs"
install -m 0644 "$baked_artifacts/rescue-tools.erofs" "$baked_stage/nmbl/rescue-tools.erofs"
test ! -e "$baked_stage/nmbl-rescue.sfs.sig"
test ! -e "$baked_stage/nmbl/config.toml.sig"
test ! -e "$baked_stage/nmbl/network.erofs"
make_disk "$baked_stage" "$work_root/baked-static.img"
python3 "$harness" scan --key "$private_key" --key "$ssh_private_key" \
  --marker "$marker" "$baked_artifacts" "$baked_stage" "$work_root/baked-static.img"

slaac_stage="$work_root/slaac-boot"
mkdir -p "$slaac_stage/nmbl"
cp "$slaac_artifacts/config.toml" "$slaac_stage/nmbl/config.toml"
cp "$stage/rescue-host-ed25519" "$stage/rescue-host-ed25519.pub" "$slaac_stage/"
# Match unsigned install-bootloader staging: immutable rescue artifact, mode 0644.
install -m 0644 "$slaac_artifacts/rescue.sfs" "$slaac_stage/nmbl-rescue.sfs"
install -m 0644 "$slaac_artifacts/rescue-tools.erofs" "$slaac_stage/nmbl/rescue-tools.erofs"
test ! -e "$slaac_stage/nmbl-rescue.sfs.sig"
test ! -e "$slaac_stage/nmbl/config.toml.sig"
test ! -e "$slaac_stage/nmbl/network.erofs"
make_disk "$slaac_stage" "$work_root/baked-slaac.img"
python3 "$harness" scan --key "$private_key" --key "$ssh_private_key" \
  --marker "$marker" "$slaac_artifacts" "$slaac_stage" "$work_root/baked-slaac.img"

# The stage-2 rescue image carries no host data: two hosts that differ only
# in their network configuration share one image (and one store path).
test "$(readlink -f "$baked_artifacts/rescue.sfs")" = "$(readlink -f "$slaac_artifacts/rescue.sfs")"
# Nor key data: the enforced-signing host (whose nmblctl requires its baked
# keys) and the unsigned ones share it too; only their tools images differ.
test "$(readlink -f "$artifacts/rescue.sfs")" = "$(readlink -f "$baked_artifacts/rescue.sfs")"
test "$(readlink -f "$artifacts/rescue-tools.erofs")" != "$(readlink -f "$baked_artifacts/rescue-tools.erofs")"
test ! -e "$work_root/rescue/etc/nmbl-rescue"
test ! -e "$work_root/rescue/root/.ssh/authorized_keys"

mkdir "$work_root/native-initrd"
(cd "$work_root/native-initrd" && lsinitrd --unpack "$native_artifacts/initrd")
python3 "$harness" console-image --initrd "$work_root/native-initrd" \
  --identity-config "$native_artifacts/config.toml"

native_stage="$work_root/native-boot"
mkdir -p "$native_stage/nmbl" "$work_root/native-state/@persistent/etc/ssh"
cp "$native_artifacts/config.toml" "$native_stage/nmbl/config.toml"
cp "$stage/rescue-host-ed25519" "$stage/rescue-host-ed25519.pub" "$native_stage/"
install -m 0644 "$native_artifacts/rescue.sfs" "$native_stage/nmbl-rescue.sfs"
install -m 0644 "$native_artifacts/rescue-tools.erofs" "$native_stage/nmbl/rescue-tools.erofs"
make_disk "$native_stage" "$work_root/native-identity.img"
# The identity differs from the unrelated boot-partition key, proving its source.
ssh-keygen -q -t ed25519 -N '' -f "$work_root/native-state/@persistent/etc/ssh/ssh_host_ed25519_key"
chmod 0600 "$work_root/native-state/@persistent/etc/ssh/ssh_host_ed25519_key"
ssh-keygen -lf "$work_root/native-state/@persistent/etc/ssh/ssh_host_ed25519_key.pub"
truncate -s 512M "$work_root/native-state.img"
LD_PRELOAD=@rootdirOwner@ mkfs.btrfs -q -f --rootdir "$work_root/native-state" \
  --subvol rw:@persistent "$work_root/native-state.img"
btrfs inspect-internal dump-tree "$work_root/native-state.img" > "$work_root/native-state-metadata.log"
grep -F 'name: @persistent' "$work_root/native-state-metadata.log"
grep -E 'mode 100600 links 1 uid 0 gid 0' "$work_root/native-state-metadata.log"
qemu-img create -f qcow2 -F raw -b "$work_root/native-state.img" "$work_root/native-state.qcow2"
python3 "$harness" scan --key "$private_key" --key "$ssh_private_key" --marker "$marker" \
  "$native_artifacts" "$native_stage" "$work_root/native-initrd"

# Network rescue: the boot partition carries no rescue image, so NMBL must
# download the stage-2 EROFS image and loop-mount it itself.
netrescue_stage="$work_root/netrescue-boot"
mkdir -p "$netrescue_stage/nmbl" "$work_root/netrescue-http"
cp "$netrescue_artifacts/config.toml" "$netrescue_stage/nmbl/config.toml"
cp "$stage/rescue-host-ed25519" "$stage/rescue-host-ed25519.pub" "$netrescue_stage/"
install -m 0644 "$netrescue_artifacts/rescue-tools.erofs" "$netrescue_stage/nmbl/rescue-tools.erofs"
grep -qx 'network = true' "$netrescue_stage/nmbl/config.toml"
grep -qx 'format = "erofs"' "$netrescue_stage/nmbl/config.toml"
grep -qx "sha512 = \"$(sha512sum < "$netrescue_artifacts/rescue.sfs" | cut -d' ' -f1)\"" "$netrescue_stage/nmbl/config.toml"
test ! -e "$netrescue_stage/nmbl-rescue.sfs"
# Signing stays enforced: the config and tools image are signed as the
# installer signs them, and the downloaded image must verify too.
"$signer/bin/nmbl-sign" sign --key "$private_key" --domain boot-config \
  --out "$netrescue_stage/nmbl/config.toml.sig" "$netrescue_stage/nmbl/config.toml"
"$signer/bin/nmbl-sign" sign --key "$private_key" --domain rescue-tools \
  --out "$netrescue_stage/nmbl/rescue-tools.erofs.sig" "$netrescue_stage/nmbl/rescue-tools.erofs"
make_disk "$netrescue_stage" "$work_root/network-rescue.img"
netrescue_http="$work_root/netrescue-http"
install -m 0644 "$netrescue_artifacts/rescue.sfs" "$netrescue_http/nmbl-rescue.sfs"
"$signer/bin/nmbl-sign" sign --key "$private_key" --domain rescue-sfs \
  --out "$netrescue_http/nmbl-rescue.sfs.sig" "$netrescue_http/nmbl-rescue.sfs"
# The pinned image without its signature, and a tampered image served with
# the genuine image's signature: both must be refused before mounting.
install -m 0644 "$netrescue_artifacts/rescue.sfs" "$netrescue_http/nmbl-rescue-unsigned.sfs"
install -m 0644 "$netrescue_artifacts/rescue.sfs" "$netrescue_http/nmbl-rescue-badsig.sfs"
printf 'NMBL_TAMPER' >> "$netrescue_http/nmbl-rescue-badsig.sfs"
cp "$netrescue_http/nmbl-rescue.sfs.sig" "$netrescue_http/nmbl-rescue-badsig.sfs.sig"
python3 "$harness" scan --key "$private_key" --key "$ssh_private_key" --marker "$marker" \
  "$netrescue_artifacts" "$netrescue_stage" "$work_root/network-rescue.img"

rm -f "$private_key" "$operator_root/do-not-export.marker"
test ! -e "$private_key"

ssh_port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')

python3 "$harness" boot --qemu "$qemu" --passt @passt@ \
  --kernel "$netrescue_artifacts/kernel" --initrd "$netrescue_artifacts/initrd" \
  --disk "$work_root/network-rescue.img" --transcript "$work_root/network-rescue.log" \
  --ssh @ssh@ --ssh-port "$ssh_port" --ssh-key "$ssh_private_key" \
  --ssh-host-key "$stage/rescue-host-ed25519.pub" \
  --rescue-image "$netrescue_http/nmbl-rescue.sfs" \
  --unsigned-rescue-image "$netrescue_http/nmbl-rescue-unsigned.sfs" \
  --badsig-rescue-image "$netrescue_http/nmbl-rescue-badsig.sfs" --mode network-rescue

for scenario in good tampered unsigned malformed substituted tools-tampered; do
  mode=invalid
  test "$scenario" = good && mode=good
  test "$scenario" = substituted && mode=substituted
  test "$scenario" = tools-tampered && mode=tools-tampered
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

python3 "$harness" boot --qemu "$qemu" --passt @passt@ \
  --kernel "$baked_artifacts/kernel" --initrd "$baked_artifacts/initrd" \
  --disk "$work_root/baked-static.img" --transcript "$work_root/baked-static.log" \
  --ssh @ssh@ --ssh-port "$ssh_port" --ssh-key "$ssh_private_key" \
  --ssh-host-key "$stage/rescue-host-ed25519.pub" --mode baked-static

python3 "$harness" boot --qemu "$qemu" --passt @passt@ \
  --kernel "$slaac_artifacts/kernel" --initrd "$slaac_artifacts/initrd" \
  --disk "$work_root/baked-slaac.img" --transcript "$work_root/baked-slaac.log" \
  --ssh @ssh@ --ssh-port "$ssh_port" --ssh-key "$ssh_private_key" \
  --ssh-host-key "$stage/rescue-host-ed25519.pub" --mode baked-slaac

python3 "$harness" boot --qemu "$qemu" --passt @passt@ \
  --kernel "$native_artifacts/kernel" --initrd "$native_artifacts/initrd" \
  --disk "$work_root/native-identity.img" --identity-disk "$work_root/native-state.qcow2" \
  --transcript "$work_root/native-identity.log" --ssh @ssh@ --ssh-port "$ssh_port" \
  --ssh-key "$ssh_private_key" --ssh-host-key "$work_root/native-state/@persistent/etc/ssh/ssh_host_ed25519_key.pub" --mode native-identity
python3 "$harness" boot --qemu "$qemu" --passt @passt@ \
  --kernel "$native_artifacts/kernel" --initrd "$native_artifacts/initrd" \
  --disk "$work_root/native-identity.img" --transcript "$work_root/missing-identity.log" \
  --ssh @ssh@ --ssh-port "$ssh_port" --ssh-key "$ssh_private_key" \
  --ssh-host-key "$work_root/native-state/@persistent/etc/ssh/ssh_host_ed25519_key.pub" --mode missing-identity

echo "NMBL signed and baked static rescue network VM tests passed"
