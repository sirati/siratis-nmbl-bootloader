set -euo pipefail
umask 077

operator=$(mktemp -d /dev/shm/nmbl-generation-operator.XXXXXXXX)
work=$(mktemp -d "${TMPDIR:-/tmp}/nmbl-generation-state-vm.XXXXXXXX")
cleanup() { chmod -R u+w "$operator" "$work" 2>/dev/null || true; rm -rf "$operator" "$work"; }
trap cleanup EXIT INT TERM
private="$operator/operator.key"
public="$operator/operator.pub"
marker="$operator/operator.marker"
# keygen --stdio: private key on stdout (captured on tmpfs, standing in for a
# secrets store), raw public key on fd 3. No key file is created by the tool.
@signer@/bin/nmbl-sign keygen --alg ml-dsa-65 --stdio > "$private" 3> "$public"
# The first and third deploys sign through NMBL_SIGN_KEY_COMMAND (piped into
# nmbl-sign --key-stdin); the second keeps the key-file path.
key_command="cat $private"
head -c 64 /dev/urandom | base64 > "$marker"
[[ "$private" != /nix/store/* && $(stat -f -c %T "$operator") = tmpfs ]]

public_hash=$(nix hash path --type sha256 "$public")
export NMBL_PUBLIC_KEY="$public"
scan_targets=(@source@)
closure_paths=(@signer@ @ctl@ @receive@ @deploy@)

make_disk() {
  local tree=$1 disk=$2 label=$3 used reserve size
  # mkfs copies logical bytes, including holes in sparse EROFS images.
  # Count hard links separately because mkfs.ext4's directory importer may
  # materialize each path even when the host tree shares the inode.
  used=$(du -sm --apparent-size --count-links "$tree" | cut -f1)
  # EROFS generation files are already dense. Account for ext4 metadata,
  # reserved blocks, and copy-time rounding instead of assuming 512 MiB is
  # sufficient for an arbitrarily large three-generation fixture.
  reserve=$(( used + 1024 ))
  size=$(( used + reserve ))
  echo "building $label test disk: logical tree ${used} MiB, image ${size} MiB"
  truncate -s "${size}M" "$disk"
  mkfs.ext4 -q -F -L "$label" -d "$tree" "$disk"
  e2fsck -fn "$disk" >/dev/null
}

prepare_layout() {
  local root_store=$2 dir="$work/$1"
  local state_tree state_root invalid_label artifacts first second third
  mkdir -p "$dir/boot-tree" "$dir/root-tree" "$dir/store-tree"
  if [[ "$root_store" == true ]]; then
    state_tree="$dir/root-tree"
    invalid_label=NMBLROOT
  else
    state_tree="$dir/store-tree"
    invalid_label=NMBLSTORE
  fi
  state_root="$state_tree/nmbl-generations"

  artifacts=$(nix build --no-link --print-out-paths \
    --file @eval@ --argstr source @source@ \
    --argstr publicKeyPath "$public" --argstr publicKeyHash "$public_hash" \
    --arg rootStore "$root_store")
  ln -s "$artifacts" "$dir/artifacts"

  cat > "$dir/flake.nix" <<EOF
{
  inputs.nmbl.url = "path:@source@";
  inputs.nixpkgs.follows = "nmbl/nixpkgs";
  outputs = { nmbl, nixpkgs, ... }:
    let
      publicKey = builtins.path {
        path = builtins.getEnv "NMBL_PUBLIC_KEY";
        name = "nmbl-generation-state-vm-public.key";
      };
      generation = variant: import "\${nmbl}/testing/generation-state-vm/configuration.nix" {
        inherit publicKey variant;
        nmblModule = nmbl.nixosModules.default;
        inherit nixpkgs;
        rootStore = $root_store;
      };
    in { nixosConfigurations = {
      first = generation 1;
      second = generation 2;
      third = generation 3;
    }; };
}
EOF
  cat > "$dir/test-ssh" <<EOF
#!/bin/sh
set -eu
test "\$1" = --; shift
test "\$1" = generation-test-target; shift
test "\$1" = nmbl-erofs-receive
exec @receive@/bin/nmbl-erofs-receive "$dir/incoming" "$state_root" "$public"
EOF
  chmod 0700 "$dir/test-ssh"
  first=$(NMBL_EROFS_DEPLOY_IMPURE=1 NMBL_EROFS_SSH="$dir/test-ssh" \
    NMBL_SIGN_KEY_COMMAND="$key_command" @deploy@/bin/nmbl-erofs-deploy remote \
    "path:$dir#nixosConfigurations.first" - generation-test-target | tail -n1)
  second=$(NMBL_EROFS_DEPLOY_IMPURE=1 NMBL_EROFS_SSH="$dir/test-ssh" \
    @deploy@/bin/nmbl-erofs-deploy remote \
    "path:$dir#nixosConfigurations.second" "$private" generation-test-target | tail -n1)
  third=$(NMBL_EROFS_DEPLOY_IMPURE=1 NMBL_EROFS_SSH="$dir/test-ssh" \
    NMBL_SIGN_KEY_COMMAND="$key_command" @deploy@/bin/nmbl-erofs-deploy remote \
    "path:$dir#nixosConfigurations.third" - generation-test-target | tail -n1)
  test "$first" != "$second"
  test "$second" != "$third"
  test "$first" != "$third"
  printf '%s\n' "$first" > "$dir/boot-tree/nmbl-test-first"
  printf '%s\n' "$second" > "$dir/boot-tree/nmbl-test-second"
  printf '%s\n' "$third" > "$dir/boot-tree/nmbl-test-third"

  @ctl@/bin/nmbl-erofsctl activate "$first" "$state_root" >/dev/null

  make_disk "$dir/boot-tree" "$dir/boot.raw" NMBLBOOT
  make_disk "$dir/root-tree" "$dir/root.raw" NMBLROOT
  make_disk "$dir/store-tree" "$dir/store.raw" NMBLSTORE
  cp -a "$state_tree" "$dir/tampered-tree"
  chmod u+w "$dir/tampered-tree/nmbl-generations/generations/$first/nix.erofs"
  printf X | dd of="$dir/tampered-tree/nmbl-generations/generations/$first/nix.erofs" \
    bs=1 seek=8192 conv=notrunc status=none
  make_disk "$dir/tampered-tree" "$dir/tampered.raw" "$invalid_label"
  cp -a "$state_tree" "$dir/unsigned-tree"
  rm -f "$dir/unsigned-tree/nmbl-generations/generations/$first/nix.erofs.sig"
  make_disk "$dir/unsigned-tree" "$dir/unsigned.raw" "$invalid_label"

  printf '%s\n' "$first" "$second" "$third" > "$dir/generation-ids"
  scan_targets+=("$dir/flake.nix" "$artifacts" "$dir/boot-tree" "$dir/root-tree"
    "$dir/store-tree" "$dir/boot.raw" "$dir/root.raw" "$dir/store.raw"
    "$dir/tampered.raw" "$dir/unsigned.raw")
  closure_paths+=("$artifacts")
}

run_layout() {
  local target=$2 dir="$work/$1"
  readarray -t ids < "$dir/generation-ids"
  python3 @harness@ --qemu @qemu@ --kernel "$dir/artifacts/kernel" \
    --initrd "$dir/artifacts/initrd" --boot "$dir/boot.raw" \
    --tampered "$dir/tampered.raw" --unsigned "$dir/unsigned.raw" \
    --root "$dir/root.raw" --store "$dir/store.raw" --store-target "$target" \
    --first "${ids[0]}" --second "${ids[1]}" --third "${ids[2]}" \
    --transcript "$dir/happy.log"
}

prepare_layout persistent false
prepare_layout root true

nix-store -qR "${closure_paths[@]}" > "$work/closure-paths"
mapfile -t closure < "$work/closure-paths"
python3 @scanner@ "$private" "$marker" "${scan_targets[@]}" "${closure[@]}"
test ! -e "$private"

run_layout persistent store
run_layout root root
echo "NMBL generation state-machine production VM tests passed (persistent and root stores)"
