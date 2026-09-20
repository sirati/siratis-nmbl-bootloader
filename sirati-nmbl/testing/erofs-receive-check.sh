set -euo pipefail
mkdir -p "$TMPDIR/keys" "$TMPDIR/incoming" "$TMPDIR/root"
@sign@/bin/nmbl-sign keygen --alg ml-dsa-65 \
  --out-priv "$TMPDIR/keys/private" --out-pub "$TMPDIR/keys/public"
printf 'nmbl-receiver-private-marker-%s' "$RANDOM" > "$TMPDIR/keys/marker"
printf first > "$TMPDIR/first.erofs"
printf second > "$TMPDIR/second.erofs"
first=$(@ctl@/bin/nmbl-erofsctl prepare \
  "$TMPDIR/first.erofs" "$TMPDIR/keys/private" "$TMPDIR/first")
second=$(@ctl@/bin/nmbl-erofsctl prepare \
  "$TMPDIR/second.erofs" "$TMPDIR/keys/private" "$TMPDIR/second")

printf 'first config' > "$TMPDIR/first.config"
printf 'second config' > "$TMPDIR/second.config"
for name in first second; do
  @sign@/bin/nmbl-sign sign --key "$TMPDIR/keys/private" --domain boot-config \
    --out "$TMPDIR/$name.config.sig" "$TMPDIR/$name.config" >/dev/null
done
printf kernel > "$TMPDIR/kernel"
printf initrd > "$TMPDIR/initrd"
printf 'first rescue' > "$TMPDIR/rescue"
@sign@/bin/nmbl-sign sign --key "$TMPDIR/keys/private" --domain gen-kernel \
  --out "$TMPDIR/kernel.sig" "$TMPDIR/kernel" >/dev/null
@sign@/bin/nmbl-sign sign --key "$TMPDIR/keys/private" --domain gen-initrd \
  --out "$TMPDIR/initrd.sig" "$TMPDIR/initrd" >/dev/null
@sign@/bin/nmbl-sign sign --key "$TMPDIR/keys/private" --domain rescue-sfs \
  --out "$TMPDIR/rescue.sig" "$TMPDIR/rescue" >/dev/null

send() {
  local bundle=$1 config=$2 image_sig=$3 config_sig=$4
  local kernel_sig=${5:-$TMPDIR/kernel.sig}
  printf 'NMBL-EROFS-BUNDLE-3\n%s\n%s\n%s\n0\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n0\n0\n0\n' \
    "$(cat "$bundle/generation")" "$(stat -c %s "$bundle/nix.erofs")" \
    "$(stat -c %s "$image_sig")" "$(sha512sum "$config" | cut -d' ' -f1)" \
    "$(stat -c %s "$config")" "$(stat -c %s "$config_sig")" \
    "$(stat -c %s "$TMPDIR/kernel")" "$(stat -c %s "$kernel_sig")" \
    "$(stat -c %s "$TMPDIR/initrd")" "$(stat -c %s "$TMPDIR/initrd.sig")" \
    "$(stat -c %s "$TMPDIR/rescue")" "$(stat -c %s "$TMPDIR/rescue.sig")"
  cat "$bundle/nix.erofs" "$image_sig" "$config" "$config_sig" \
    "$TMPDIR/kernel" "$kernel_sig" "$TMPDIR/initrd" "$TMPDIR/initrd.sig" \
    "$TMPDIR/rescue" "$TMPDIR/rescue.sig"
}
receive() { @receive@/bin/nmbl-erofs-receive "$TMPDIR/incoming" "$TMPDIR/root" "$TMPDIR/keys/public"; }

cp "$TMPDIR/second/nix.erofs.sig" "$TMPDIR/bad-image.sig"
cp "$TMPDIR/second.config.sig" "$TMPDIR/bad-config.sig"
cp "$TMPDIR/kernel.sig" "$TMPDIR/bad-kernel.sig"
chmod u+w "$TMPDIR"/bad-*.sig
for signature in "$TMPDIR"/bad-*.sig; do
  printf X | dd of="$signature" bs=1 seek=40 conv=notrunc status=none
done

send "$TMPDIR/first" "$TMPDIR/first.config" "$TMPDIR/first/nix.erofs.sig" \
  "$TMPDIR/first.config.sig" | receive
test "$(readlink "$TMPDIR/root/active")" = "generations/$first"
reject_unchanged() {
  if receive; then exit 1; fi
  test "$(readlink "$TMPDIR/root/active")" = "generations/$first"
  test "$(cat "$TMPDIR/root/active/config.toml")" = 'first config'
}
send "$TMPDIR/second" "$TMPDIR/second.config" "$TMPDIR/bad-image.sig" \
  "$TMPDIR/second.config.sig" | reject_unchanged
send "$TMPDIR/second" "$TMPDIR/second.config" "$TMPDIR/second/nix.erofs.sig" \
  "$TMPDIR/bad-config.sig" | reject_unchanged
send "$TMPDIR/second" "$TMPDIR/second.config" "$TMPDIR/second/nix.erofs.sig" \
  "$TMPDIR/second.config.sig" "$TMPDIR/bad-kernel.sig" | reject_unchanged
send "$TMPDIR/second" "$TMPDIR/second.config" "$TMPDIR/second/nix.erofs.sig" \
  "$TMPDIR/second.config.sig" | receive
test "$(readlink "$TMPDIR/root/active")" = "generations/$second"
for artifact in config.toml kernel.sig initrd.sig rescue.sfs rescue.sfs.sig; do
  test -s "$TMPDIR/root/active/$artifact"
done
python3 @scanner@ "$TMPDIR/keys/private" "$TMPDIR/keys/marker" "$TMPDIR/root" @receive@
touch "$out"
