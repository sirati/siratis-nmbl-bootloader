set -euo pipefail

die() { echo "nmbl-erofs-receive: $*" >&2; exit 1; }
[[ $# -eq 3 ]] || die "usage: nmbl-erofs-receive INCOMING_ROOT IMAGE_ROOT PUBLIC_KEY"
incoming=$1
image_root=$2
public_key=$3
[[ "$incoming" = /* && "$incoming" != /nix/store* ]] || die "invalid incoming root"
[[ "$image_root" = /* && "$image_root" != /nix/store* ]] || die "invalid image root"
[[ -f "$public_key" ]] || die "trusted public key is missing"

read_line() { IFS= read -r "$1" || die "truncated protocol header"; }
magic='' id='' image_size='' signature_size='' system_size=''
config_id='' config_size='' config_signature_size='' reboot=''
read_line magic
read_line id
read_line image_size
read_line signature_size
read_line system_size
read_line config_id
read_line config_size
read_line config_signature_size
read_line reboot
[[ "$magic" = NMBL-EROFS-BUNDLE-2 ]] || die "invalid protocol magic"
[[ "$id" =~ ^[0-9a-f]{128}$ ]] || die "invalid generation id"
[[ "$config_id" =~ ^[0-9a-f]{128}$ ]] || die "invalid config id"
for size in "$image_size" "$signature_size" "$system_size" "$config_size" "$config_signature_size"; do
  [[ "$size" =~ ^[0-9]+$ ]] || die "invalid payload size"
  (( size <= 68719476736 )) || die "payload is too large"
done
(( image_size > 0 && signature_size > 0 && config_size > 0 && config_signature_size > 0 )) || die "empty payload or signature"
[[ "$reboot" = 0 || "$reboot" = 1 ]] || die "invalid reboot flag"

install -d -m 0700 "$incoming"
tmp=$(mktemp -d "$incoming/.receive-$id.XXXXXXXX")
trap 'chmod -R u+w "$tmp" 2>/dev/null || true; rm -rf "$tmp"' EXIT
receive_file() {
  local size=$1 path=$2 actual
  head -c "$size" > "$path"
  actual=$(stat -c %s "$path")
  [[ "$actual" = "$size" ]] || die "truncated payload"
}
receive_file "$image_size" "$tmp/nix.erofs"
receive_file "$signature_size" "$tmp/nix.erofs.sig"
if (( system_size > 0 )); then receive_file "$system_size" "$tmp/system"; fi
receive_file "$config_size" "$tmp/config.toml"
receive_file "$config_signature_size" "$tmp/config.toml.sig"
extra="$tmp/.extra"
head -c 1 > "$extra" || true
[[ ! -s "$extra" ]] || die "trailing protocol data"
printf '%s\n' "$id" > "$tmp/generation"
actual=$(sha512sum "$tmp/nix.erofs" | cut -d' ' -f1)
[[ "$actual" = "$id" ]] || die "image hash does not match generation id"
actual=$(sha512sum "$tmp/config.toml" | cut -d' ' -f1)
[[ "$actual" = "$config_id" ]] || die "config hash does not match config id"
@nmblSign@/bin/nmbl-sign verify --key "$public_key" --domain generation-image \
  --sig "$tmp/nix.erofs.sig" "$tmp/nix.erofs" >/dev/null
@nmblSign@/bin/nmbl-sign verify --key "$public_key" --domain boot-config \
  --sig "$tmp/config.toml.sig" "$tmp/config.toml" >/dev/null
chmod 0444 "$tmp"/*

existing_generation="$image_root/generations/$id"
if [[ -e "$existing_generation" ]]; then
  [[ -d "$existing_generation" ]] || die "existing generation path is not a directory"
  cmp -s "$tmp/nix.erofs" "$existing_generation/nix.erofs" || die "existing generation image differs"
  @nmblSign@/bin/nmbl-sign verify --key "$public_key" --domain generation-image \
    --sig "$existing_generation/nix.erofs.sig" "$existing_generation/nix.erofs" >/dev/null
  cmp -s "$tmp/config.toml" "$existing_generation/config.toml" || die "existing config differs"
  @nmblSign@/bin/nmbl-sign verify --key "$public_key" --domain boot-config \
    --sig "$existing_generation/config.toml.sig" "$existing_generation/config.toml" >/dev/null
fi
@ctl@/bin/nmbl-erofsctl install "$tmp" "$image_root" >/dev/null
installed="$image_root/generations/$id"
cmp -s "$tmp/config.toml" "$installed/config.toml" || die "installed config differs"
@nmblSign@/bin/nmbl-sign verify --key "$public_key" --domain boot-config \
  --sig "$installed/config.toml.sig" "$installed/config.toml" >/dev/null
@ctl@/bin/nmbl-erofsctl activate "$id" "$image_root"
printf '%s\n' "$id"
if [[ "$reboot" = 1 ]]; then @systemctl@ reboot; fi
