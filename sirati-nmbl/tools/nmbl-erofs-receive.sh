set -euo pipefail

die() { echo "nmbl-erofs-receive: $*" >&2; exit 1; }
[[ $# -eq 2 ]] || die "usage: nmbl-erofs-receive INCOMING_ROOT IMAGE_ROOT"
incoming=$1
image_root=$2
[[ "$incoming" = /* && "$incoming" != /nix/store* ]] || die "invalid incoming root"
[[ "$image_root" = /* && "$image_root" != /nix/store* ]] || die "invalid image root"

read_line() { IFS= read -r "$1" || die "truncated protocol header"; }
magic='' id='' image_size='' signature_size='' system_size='' reboot=''
read_line magic
read_line id
read_line image_size
read_line signature_size
read_line system_size
read_line reboot
[[ "$magic" = NMBL-EROFS-BUNDLE-1 ]] || die "invalid protocol magic"
[[ "$id" =~ ^[0-9a-f]{128}$ ]] || die "invalid generation id"
for size in "$image_size" "$signature_size" "$system_size"; do
  [[ "$size" =~ ^[0-9]+$ ]] || die "invalid payload size"
  (( size <= 68719476736 )) || die "payload is too large"
done
(( image_size > 0 && signature_size > 0 )) || die "empty image or signature"
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
extra="$tmp/.extra"
head -c 1 > "$extra" || true
[[ ! -s "$extra" ]] || die "trailing protocol data"
printf '%s\n' "$id" > "$tmp/generation"
actual=$(sha512sum "$tmp/nix.erofs" | cut -d' ' -f1)
[[ "$actual" = "$id" ]] || die "image hash does not match generation id"
chmod 0444 "$tmp"/*

@ctl@/bin/nmbl-erofsctl install "$tmp" "$image_root" >/dev/null
@ctl@/bin/nmbl-erofsctl activate "$id" "$image_root"
printf '%s\n' "$id"
if [[ "$reboot" = 1 ]]; then @systemctl@ reboot; fi
