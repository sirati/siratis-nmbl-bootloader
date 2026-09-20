set -euo pipefail
umask 077

base=${NMBL_VM_TMPDIR:-/dev/shm}
[[ $(stat -f -c %T "$base") = tmpfs ]] || {
  echo "NMBL_VM_TMPDIR must reside on tmpfs" >&2
  exit 1
}
work=$(mktemp -d --tmpdir="$base" nmbl-generation-vm.XXXXXXXX)
cleanup() { chmod -R u+w "$work" 2>/dev/null || true; find "$work" -delete; }
trap cleanup EXIT
private="$work/operator.key"
public="$work/operator.pub"
marker="$work/operator.marker"
@signer@/bin/nmbl-sign keygen --alg ml-dsa-65 --out-priv "$private" --out-pub "$public"
head -c 64 /dev/urandom | base64 > "$marker"
[[ "$private" != /nix/store/* ]] || exit 1

cat > "$work/flake.nix" <<'EOF'
{
  inputs.nmbl.url = "path:@source@";
  inputs.nixpkgs.follows = "nmbl/nixpkgs";
  outputs = { nmbl, nixpkgs, ... }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      publicKey = builtins.path {
        path = builtins.getEnv "NMBL_PUBLIC_KEY";
        name = "nmbl-ephemeral-operator.pub";
      };
      node = import "${nmbl}/testing/generation-image-node.nix" {
        nmblModule = nmbl.nixosModules.default;
        inherit publicKey;
        diskEnvironment = "NMBL_DISK_HAPPY";
        withVm = false;
      };
      test = import "${nmbl}/testing/generation-image-vm.nix" {
        inherit pkgs publicKey;
        nmblModule = nmbl.nixosModules.default;
      };
      combinedImage = import "${nmbl}/lib/generation-image.nix" {
        inherit pkgs;
        lib = nixpkgs.lib;
        rootPaths = map (node: node.system.build.toplevel) (builtins.attrValues test.nodes);
      };
      generation = nixpkgs.lib.nixosSystem {
        inherit system;
        modules = [ node { system.build.nmblGenerationImage = nixpkgs.lib.mkForce combinedImage; } ];
      };
    in {
      nixosConfigurations.generation = generation;
      packages.${system} = {
        driver = test.driver;
        initrd = generation.config.system.build.initialRamdisk;
        unsigned = combinedImage;
      };
    };
}
EOF
export NMBL_PUBLIC_KEY="$public"
flake="path:$work"
driver=$(nix build --impure --no-link --print-out-paths "$flake#driver")
initrd=$(nix build --impure --no-link --print-out-paths "$flake#initrd")
unsigned=$(nix build --impure --no-link --print-out-paths "$flake#unsigned")

root="$work/generation-root"
NMBL_EROFS_DEPLOY_IMPURE=1 @deploy@/bin/nmbl-erofs-deploy \
  "$flake#nixosConfigurations.generation" "$private" "$root"
cp "$unsigned" "$work/second.erofs"
chmod u+w "$work/second.erofs"
truncate -s +4096 "$work/second.erofs"
second=$(@ctl@/bin/nmbl-erofsctl prepare \
  "$work/second.erofs" "$private" "$work/second-bundle")
@ctl@/bin/nmbl-erofsctl install "$work/second-bundle" "$root"

make_disk() {
  local tree=$1 disk=$2
  truncate -s 4096M "$disk"
  mkfs.ext4 -q -F -L NMBLBOOT -d "$tree" "$disk"
}
mkdir "$work/happy-tree" "$work/tampered-tree" "$work/unsigned-tree"
cp -a "$root" "$work/happy-tree/nmbl-generations"
cp -a "$root" "$work/tampered-tree/nmbl-generations"
cp -a "$root" "$work/unsigned-tree/nmbl-generations"
active=$(basename "$(readlink "$root/active")")
chmod u+w "$work/tampered-tree/nmbl-generations/generations/$active/nix.erofs"
printf X | dd of="$work/tampered-tree/nmbl-generations/generations/$active/nix.erofs" \
  bs=1 seek=8192 conv=notrunc status=none
find "$work/unsigned-tree/nmbl-generations/generations/$active" \
  -name nix.erofs.sig -delete
make_disk "$work/happy-tree" "$work/happy.raw"
make_disk "$work/tampered-tree" "$work/tampered.raw"
make_disk "$work/unsigned-tree" "$work/unsigned.raw"

mapfile -t closure < <(nix-store -qR "$driver" "$initrd" "$unsigned")
python3 @scanner@ "$private" "$marker" \
  @source@ "$work/flake.nix" "$driver" "$initrd" "$unsigned" \
  "$work/generation-root" "$work/second-bundle" \
  "$work/happy.raw" "$work/tampered.raw" "$work/unsigned.raw" \
  "${closure[@]}"
test ! -e "$private"

export NMBL_DISK_HAPPY="$work/happy.raw"
export NMBL_DISK_TAMPERED="$work/tampered.raw"
export NMBL_DISK_UNSIGNED="$work/unsigned.raw"
export TMPDIR="$work/driver-tmp"
mkdir "$TMPDIR"
cd "$work"
"$driver/bin/nixos-test-driver"
echo "positive generation-image VM test passed; second generation was $second"
