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

# The rescue tools image (nmblctl) travels only when the config pins it.
tools_payload='' tools_signature='' magic=NMBL-EROFS-BUNDLE-4
send() {
  local bundle=$1 config=$2 image_sig=$3 config_sig=$4
  local kernel_sig=${5:-$TMPDIR/kernel.sig}
  local tools_size=0 tools_signature_size=0
  if [[ -n "$tools_payload" ]]; then
    tools_size=$(stat -c %s "$tools_payload")
    tools_signature_size=$(stat -c %s "$tools_signature")
  fi
  printf '%s\n%s\n%s\n%s\n0\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n0\n0\n%s\n%s\n0\n' \
    "$magic" \
    "$(cat "$bundle/generation")" "$(stat -c %s "$bundle/nix.erofs")" \
    "$(stat -c %s "$image_sig")" "$(sha512sum "$config" | cut -d' ' -f1)" \
    "$(stat -c %s "$config")" "$(stat -c %s "$config_sig")" \
    "$(stat -c %s "$TMPDIR/kernel")" "$(stat -c %s "$kernel_sig")" \
    "$(stat -c %s "$TMPDIR/initrd")" "$(stat -c %s "$TMPDIR/initrd.sig")" \
    "$(stat -c %s "$TMPDIR/rescue")" "$(stat -c %s "$TMPDIR/rescue.sig")" \
    "$tools_size" "$tools_signature_size"
  cat "$bundle/nix.erofs" "$image_sig" "$config" "$config_sig" \
    "$TMPDIR/kernel" "$kernel_sig" "$TMPDIR/initrd" "$TMPDIR/initrd.sig" \
    "$TMPDIR/rescue" "$TMPDIR/rescue.sig"
  [[ -z "$tools_payload" ]] || cat "$tools_payload" "$tools_signature"
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
test ! -e "$TMPDIR/root/active/rescue-tools.erofs"

# A config pinning the rescue tools image: it must arrive with exactly that
# image, signed under its own domain; anything else leaves `active` alone.
for name in third fourth fifth sixth seventh; do
  printf '%s' "$name" > "$TMPDIR/$name.erofs"
  @ctl@/bin/nmbl-erofsctl prepare \
    "$TMPDIR/$name.erofs" "$TMPDIR/keys/private" "$TMPDIR/$name" > "$TMPDIR/$name.id"
  printf '%s config\n\n[rescue.tools]\npath = "nmbl/rescue-tools.erofs"\n' "$name" > "$TMPDIR/$name.config"
  @sign@/bin/nmbl-sign sign --key "$TMPDIR/keys/private" --domain boot-config \
    --out "$TMPDIR/$name.config.sig" "$TMPDIR/$name.config" >/dev/null
done
printf 'nmblctl tools' > "$TMPDIR/tools"
@sign@/bin/nmbl-sign sign --key "$TMPDIR/keys/private" --domain rescue-tools \
  --out "$TMPDIR/tools.sig" "$TMPDIR/tools" >/dev/null
@sign@/bin/nmbl-sign sign --key "$TMPDIR/keys/private" --domain rescue-sfs \
  --out "$TMPDIR/tools.wrong-domain.sig" "$TMPDIR/tools" >/dev/null
reject_keeps_second() {
  if receive; then exit 1; fi
  test "$(readlink "$TMPDIR/root/active")" = "generations/$second"
}
send_named() {
  send "$TMPDIR/$1" "$TMPDIR/$1.config" "$TMPDIR/$1/nix.erofs.sig" "$TMPDIR/$1.config.sig"
}
# Pinned but missing.
send_named third | reject_keeps_second
# Signed under another role's domain.
tools_payload="$TMPDIR/tools" tools_signature="$TMPDIR/tools.wrong-domain.sig"
send_named fourth | reject_keeps_second
# Carried by a config that does not pin it.
tools_payload="$TMPDIR/tools" tools_signature="$TMPDIR/tools.sig"
send "$TMPDIR/fifth" "$TMPDIR/second.config" "$TMPDIR/fifth/nix.erofs.sig" \
  "$TMPDIR/second.config.sig" | reject_keeps_second
# The previous protocol cannot carry it. The receiver stops at the header, so
# hand it a file rather than a pipe the sender might still be writing.
magic=NMBL-EROFS-BUNDLE-3
send_named sixth > "$TMPDIR/bundle-3"
reject_keeps_second < "$TMPDIR/bundle-3"
magic=NMBL-EROFS-BUNDLE-4
send_named seventh | receive
test "$(readlink "$TMPDIR/root/active")" = "generations/$(cat "$TMPDIR/seventh.id")"
cmp "$TMPDIR/tools" "$TMPDIR/root/active/rescue-tools.erofs"
cmp "$TMPDIR/tools.sig" "$TMPDIR/root/active/rescue-tools.erofs.sig"
python3 @scanner@ "$TMPDIR/keys/private" "$TMPDIR/keys/marker" "$TMPDIR/root" @receive@
touch "$out"
