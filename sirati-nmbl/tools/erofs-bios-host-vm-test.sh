set -euo pipefail
umask 077

# Boots the DNS-VPS / Stardust NMBL topology end to end from a real BIOS disk:
# GRUB (MBR + BIOS boot partition) -> NMBL kernel/initrd on vfat /boot ->
# stage-1 persistent ext4 store -> signed config + EROFS /nix generation
# selected by `active` -> kexec into NixOS on a tmpfs root. Then exercises
# rollback of a failed untested generation and, after a tested generation
# fails, the signed rescue with the signed network stage and recovery SSH.
# Generations are signed through key commands and delivered by the production
# nmbl-erofs-deploy -> nmbl-erofs-receive stream.

operator=$(mktemp -d /dev/shm/nmbl-bios-host-operator.XXXXXXXX)
work=$(mktemp -d "${TMPDIR:-/tmp}/nmbl-bios-host-vm.XXXXXXXX")
cleanup() {
  status=$?
  if [[ $status -ne 0 && -f "$work/serial.log" ]]; then
    cp "$work/serial.log" "${TMPDIR:-/tmp}/nmbl-bios-host-vm-failed.log" || true
    echo "serial transcript kept at ${TMPDIR:-/tmp}/nmbl-bios-host-vm-failed.log" >&2
  fi
  chmod -R u+w "$operator" "$work" 2>/dev/null || true
  rm -rf "$operator" "$work"
}
trap cleanup EXIT INT TERM
private="$operator/operator.key"
public="$operator/operator.pub"
ssh_key="$operator/rescue-client-ed25519"
marker="$operator/operator.marker"
[[ $(stat -f -c %T "$operator") = tmpfs ]]

# Pipe-only keys: the private key is captured from stdout (standing in for a
# secrets store) and every signature obtains it through a key command.
@signer@/bin/nmbl-sign keygen --alg ml-dsa-65 --stdio > "$private" 3> "$public"
export NMBL_SIGN_KEY_COMMAND="cat $private"
ssh-keygen -q -t ed25519 -N '' -f "$ssh_key"
head -c 64 /dev/urandom | base64 > "$marker"

export NMBL_PUBLIC_KEY="$public"
export NMBL_SSH_PUBLIC_KEY="$ssh_key.pub"
public_hash=$(nix hash path --type sha256 "$public")
ssh_hash=$(nix hash path --type sha256 "$ssh_key.pub")
artifacts=$(nix build --no-link --print-out-paths \
  --file @source@/testing/erofs-bios-host/eval.nix --argstr source @source@ \
  --argstr publicKeyPath "$public" --argstr publicKeyHash "$public_hash" \
  --argstr sshPublicKeyPath "$ssh_key.pub" --argstr sshPublicKeyHash "$ssh_hash")

store="$work/persistent"
state="$store/nmbl-generations"
mkdir -p "$store/nmbl-test"
cat > "$work/flake.nix" <<'EOF'
{
  inputs.nmbl.url = "path:@source@";
  inputs.nixpkgs.follows = "nmbl/nixpkgs";
  outputs = { nmbl, nixpkgs, ... }:
    let
      publicKey = builtins.path {
        path = builtins.getEnv "NMBL_PUBLIC_KEY";
        name = "nmbl-bios-host-vm-public.key";
      };
      sshPublicKey = builtins.readFile (builtins.path {
        path = builtins.getEnv "NMBL_SSH_PUBLIC_KEY";
        name = "nmbl-bios-host-vm-ssh.pub";
      });
      generation = variant: import "${nmbl}/testing/erofs-bios-host/configuration.nix" {
        inherit nixpkgs publicKey sshPublicKey variant;
        nmblModule = nmbl.nixosModules.default;
        vmTest = true;
        algorithm = "ml-dsa-65";
      };
    in { nixosConfigurations = { first = generation 1; second = generation 2; }; };
}
EOF
cat > "$work/test-ssh" <<EOF
#!/bin/sh
set -eu
test "\$1" = --; shift
test "\$1" = bios-host; shift
test "\$1" = nmbl-erofs-receive
exec @receive@/bin/nmbl-erofs-receive "$work/incoming" "$state" "$public"
EOF
chmod 0700 "$work/test-ssh"
deploy() {
  NMBL_EROFS_DEPLOY_IMPURE=1 NMBL_EROFS_SSH="$work/test-ssh" \
    @deploy@/bin/nmbl-erofs-deploy remote "path:$work#nixosConfigurations.$1" - bios-host \
    | tail -n1
}
first=$(deploy first)
second=$(deploy second)
test "$first" != "$second"
# The receiver activated the latest deploy and marked the first tested; boot
# the tested first generation as the baseline.
@ctl@/bin/nmbl-erofsctl activate "$first" "$state" >/dev/null
test "$(readlink "$state/tested")" = "generations/$first"
test ! -e "$state/pending"
printf '%s\n' "$first" > "$store/nmbl-test/first"
printf '%s\n' "$second" > "$store/nmbl-test/second"
# The network stage and rescue image travelled inside the generation.
for member in config.toml config.toml.sig rescue.sfs rescue.sfs.sig network.erofs network.erofs.sig; do
  test -s "$state/generations/$first/$member"
done
ssh-keygen -q -t ed25519 -N '' -f "$store/rescue-host-ed25519"

# vfat /boot: GRUB's config plus the NMBL kernel and initrd, nothing mutable.
boot_dir="$work/boot"
mkdir -p "$boot_dir/grub"
cp "$artifacts/grub.cfg" "$boot_dir/grub/grub.cfg"
cp "$artifacts/nmbl-kernel" "$boot_dir/nmbl-kernel"
cp "$artifacts/nmbl-initrd" "$boot_dir/nmbl-initrd"
if find "$boot_dir" -name 'nmbl-generations' | grep -q .; then
  echo "generation state leaked onto /boot" >&2; exit 1
fi
# The production installer must not stage an unsigned generation tree on /boot.
if grep -q 'nmbl-generations/active' "$artifacts/install-bootloader"; then
  echo "installBootLoader stages generation members on /boot" >&2; exit 1
fi

python3 @disk@ --grub @grub@ --out "$work/disk.raw" \
  --boot "$boot_dir" --persistent "$store"

# One scan over every root: the scanner consumes (deletes) the key and marker.
nix-store -qR "$artifacts" > "$work/closure-paths"
mapfile -t closure < "$work/closure-paths"
python3 @scanner@ "$private" "$marker" @source@ "$work/flake.nix" "$artifacts" \
  "$boot_dir" "$store" "$work/disk.raw" "${closure[@]}"
test ! -e "$private"

ssh_port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')
python3 @harness@ --qemu @qemu@ --passt @passt@ --ssh @ssh@ \
  --disk "$work/disk.raw" --transcript "$work/serial.log" \
  --ssh-port "$ssh_port" --ssh-key "$ssh_key" \
  --ssh-host-key "$store/rescue-host-ed25519.pub"
echo "NMBL BIOS/GRUB EROFS host VM test passed"
