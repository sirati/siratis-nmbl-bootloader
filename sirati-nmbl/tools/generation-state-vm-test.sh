set -euo pipefail
umask 077

operator=$(mktemp -d /dev/shm/nmbl-generation-operator.XXXXXXXX)
work=$(mktemp -d "${TMPDIR:-/tmp}/nmbl-generation-state-vm.XXXXXXXX")
cleanup() { chmod -R u+w "$operator" "$work" 2>/dev/null || true; rm -rf "$operator" "$work"; }
trap cleanup EXIT INT TERM
private="$operator/operator.key"
public="$operator/operator.pub"
marker="$operator/operator.marker"
@signer@/bin/nmbl-sign keygen --alg ml-dsa-65 --out-priv "$private" --out-pub "$public"
head -c 64 /dev/urandom | base64 > "$marker"
[[ "$private" != /nix/store/* && $(stat -f -c %T "$operator") = tmpfs ]]

public_hash=$(nix hash path --type sha256 "$public")
artifacts=$(nix build --no-link --print-out-paths \
  --file @eval@ --argstr source @source@ \
  --argstr publicKeyPath "$public" --argstr publicKeyHash "$public_hash")

cat > "$work/flake.nix" <<'EOF'
{
  inputs.nmbl.url = "path:@source@";
  inputs.nixpkgs.follows = "nmbl/nixpkgs";
  outputs = { nmbl, nixpkgs, ... }:
    let
      publicKey = builtins.path {
        path = builtins.getEnv "NMBL_PUBLIC_KEY";
        name = "nmbl-generation-state-vm-public.key";
      };
      generation = import "${nmbl}/testing/generation-state-vm/configuration.nix" {
        inherit publicKey;
        nmblModule = nmbl.nixosModules.default;
        inherit nixpkgs;
      };
    in { nixosConfigurations.generation = generation; };
}
EOF
export NMBL_PUBLIC_KEY="$public"
root="$work/store-tree/nmbl-generations"
mkdir -p "$work/boot-tree/nmbl" "$work/store-tree"
cat > "$work/test-ssh" <<EOF
#!/bin/sh
set -eu
test "\$1" = --; shift
test "\$1" = generation-test-target; shift
test "\$1" = nmbl-erofs-receive
exec @receive@/bin/nmbl-erofs-receive "$work/incoming" "$root" "$public"
EOF
chmod 0700 "$work/test-ssh"
first=$(NMBL_EROFS_DEPLOY_IMPURE=1 NMBL_EROFS_SSH="$work/test-ssh" \
  @deploy@/bin/nmbl-erofs-deploy remote \
  "path:$work#nixosConfigurations.generation" "$private" generation-test-target | tail -n1)

make_extra_generation() {
  local bytes=$1 name=$2
  local image="$work/$name.erofs" bundle="$work/$name-bundle"
  cp "$artifacts/generation.erofs" "$image"; chmod u+w "$image"; truncate -s "+$bytes" "$image"
  @ctl@/bin/nmbl-erofsctl prepare "$image" "$private" "$bundle" | tail -n1
  @ctl@/bin/nmbl-erofsctl install "$bundle" "$root" >/dev/null
}
second=$(make_extra_generation 4096 second)
third=$(make_extra_generation 8192 third)
printf '%s\n' "$first" > "$work/boot-tree/nmbl-test-first"
printf '%s\n' "$second" > "$work/boot-tree/nmbl-test-second"
printf '%s\n' "$third" > "$work/boot-tree/nmbl-test-third"

cp "$root/active/config.toml" "$work/boot-tree/nmbl/config.toml"
cp "$root/active/config.toml.sig" "$work/boot-tree/nmbl/config.toml.sig"
cp "$artifacts/rescue.sfs" "$work/boot-tree/nmbl-rescue.sfs"
chmod u+w "$work/boot-tree/nmbl-rescue.sfs"
@signer@/bin/nmbl-sign sign --key "$private" --domain rescue-sfs \
  "$work/boot-tree/nmbl-rescue.sfs" --out "$work/boot-tree/nmbl-rescue.sfs.sig"
gen_id=$(basename "$(readlink -f "$artifacts/toplevel")")
mkdir -p "$work/boot-tree/nmbl/sigs/$gen_id"
@signer@/bin/nmbl-sign sign --key "$private" --domain gen-kernel \
  "$artifacts/toplevel/kernel" --out "$work/boot-tree/nmbl/sigs/$gen_id/kernel.sig"
@signer@/bin/nmbl-sign sign --key "$private" --domain gen-initrd \
  "$artifacts/toplevel/initrd" --out "$work/boot-tree/nmbl/sigs/$gen_id/initrd.sig"

make_disk() {
  local tree=$1 disk=$2 label=$3 size
  size=$(( $(du -sm "$tree" | cut -f1) + 512 ))
  truncate -s "${size}M" "$disk"
  mkfs.ext4 -q -F -L "$label" -d "$tree" "$disk"
  e2fsck -fn "$disk" >/dev/null
}
mkdir "$work/root-tree"
make_disk "$work/boot-tree" "$work/boot.raw" NMBLBOOT
make_disk "$work/root-tree" "$work/root.raw" NMBLROOT
make_disk "$work/store-tree" "$work/store.raw" NMBLSTORE
cp -a "$work/store-tree" "$work/tampered-tree"
chmod u+w "$work/tampered-tree/nmbl-generations/generations/$first/nix.erofs"
printf X | dd of="$work/tampered-tree/nmbl-generations/generations/$first/nix.erofs" \
  bs=1 seek=8192 conv=notrunc status=none
make_disk "$work/tampered-tree" "$work/tampered.raw" NMBLSTORE
cp -a "$work/store-tree" "$work/unsigned-tree"
rm -f "$work/unsigned-tree/nmbl-generations/generations/$first/nix.erofs.sig"
make_disk "$work/unsigned-tree" "$work/unsigned.raw" NMBLSTORE

nix-store -qR "$artifacts" @signer@ @ctl@ @receive@ @deploy@ > "$work/closure-paths"
mapfile -t closure < "$work/closure-paths"
python3 @scanner@ "$private" "$marker" @source@ "$work/flake.nix" "$artifacts" \
  "$work/boot-tree" "$work/boot.raw" "$work/root.raw" \
  "$work/store-tree" "$work/store.raw" \
  "$work/tampered.raw" "$work/unsigned.raw" "${closure[@]}"
test ! -e "$private"

python3 @harness@ --qemu @qemu@ --kernel "$artifacts/kernel" --initrd "$artifacts/initrd" \
  --boot "$work/boot.raw" --tampered "$work/tampered.raw" --unsigned "$work/unsigned.raw" \
  --root "$work/root.raw" --store "$work/store.raw" \
  --first "$first" --second "$second" --third "$third" \
  --transcript "$work/happy.log"
echo "NMBL generation state-machine production VM test passed"
