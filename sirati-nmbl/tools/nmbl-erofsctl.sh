set -euo pipefail

die() { echo "nmbl-erofsctl: $*" >&2; exit 1; }
usage() {
  cat >&2 <<'EOF'
usage:
  nmbl-erofsctl prepare IMAGE PRIVATE_KEY OUT_DIR [SYSTEM]
  nmbl-erofsctl install BUNDLE IMAGE_ROOT
  nmbl-erofsctl activate GENERATION_ID IMAGE_ROOT
  nmbl-erofsctl rollback IMAGE_ROOT
  nmbl-erofsctl gc KEEP IMAGE_ROOT
  nmbl-erofsctl status IMAGE_ROOT

PRIVATE_KEY is read only at runtime. Pass `-` to obtain the key instead from
NMBL_SIGN_KEY_COMMAND, a shell command line whose stdout is the key; it runs
once per signature and is piped into `nmbl-sign sign --key-stdin`, so the key
never touches disk. OUT_DIR must be outside /nix/store.
IMAGE_ROOT conventionally is /.nix-image. install never changes `active`;
activate atomically replaces it after validating the complete generation.
EOF
  exit 2
}

valid_id() { [[ "$1" =~ ^[0-9a-f]{128}$ ]]; }
require_root() {
  [[ "$1" = /* ]] || die "IMAGE_ROOT must be absolute"
  [[ "$1" != /nix/store && "$1" != /nix/store/* ]] || die "IMAGE_ROOT cannot be in /nix/store"
}
target_id() {
  local link=$1 root=$2 target id
  [[ -L "$root/$link" ]] || return 1
  target=$(readlink "$root/$link")
  [[ "$target" =~ ^generations/([0-9a-f]{128})$ ]] || return 1
  id=${BASH_REMATCH[1]}
  [[ -d "$root/generations/$id" ]] || return 1
  printf '%s\n' "$id"
}
validate_generation() {
  local id=$1 root=$2 actual
  valid_id "$id" || die "invalid generation id: $id"
  [[ -f "$root/generations/$id/nix.erofs" ]] || die "generation $id has no image"
  [[ -f "$root/generations/$id/nix.erofs.sig" ]] || die "generation $id has no signature"
  actual=$(sha512sum "$root/generations/$id/nix.erofs" | cut -d' ' -f1)
  [[ "$actual" = "$id" ]] || die "generation $id content hash is $actual"
}
# Sign with a key file, or with NMBL_SIGN_KEY_COMMAND when the key is `-`.
# The command runs under the caller's PATH (NMBL_CALLER_PATH, recorded by the
# outer wrapper), since this script's own PATH is restricted to its inputs.
sign_with() {
  local key=$1; shift
  if [[ "$key" = - ]]; then
    [[ -n ${NMBL_SIGN_KEY_COMMAND:-} ]] || die "key '-' requires NMBL_SIGN_KEY_COMMAND"
    ( set -o pipefail
      PATH="${NMBL_CALLER_PATH:-$PATH}" "$BASH" -c "$NMBL_SIGN_KEY_COMMAND" \
        | @nmblSign@/bin/nmbl-sign sign --key-stdin "$@" )
  else
    @nmblSign@/bin/nmbl-sign sign --key "$key" "$@"
  fi
}
replace_link() {
  local name=$1 target=$2 root=$3 tmp
  tmp="$root/.${name}.new.$$"
  ln -s "$target" "$tmp"
  mv -Tf "$tmp" "$root/$name"
  sync -f "$root"
}

cmd=${1:-}
case "$cmd" in
  prepare)
    [[ $# -ge 4 && $# -le 5 ]] || usage
    image=$2; key=$3; out=$4; system=${5:-}
    [[ -f "$image" ]] || die "image is not a regular file: $image"
    [[ "$key" = - || -f "$key" ]] || die "private key is not a regular file: $key"
    [[ "$out" = /* ]] || die "OUT_DIR must be absolute"
    [[ "$out" != /nix/store && "$out" != /nix/store/* ]] || die "OUT_DIR cannot be in /nix/store"
    id=$(sha512sum "$image" | cut -d' ' -f1)
    tmp="${out}.tmp.$$"; rm -rf "$tmp"; install -d -m 0700 "$tmp"
    install -m 0444 "$image" "$tmp/nix.erofs"
    sign_with "$key" --domain generation-image \
      --out "$tmp/nix.erofs.sig" "$tmp/nix.erofs" >&2
    chmod 0444 "$tmp/nix.erofs.sig"
    printf '%s\n' "$id" > "$tmp/generation"
    if [[ -n "$system" ]]; then printf '%s\n' "$system" > "$tmp/system"; fi
    chmod 0444 "$tmp/generation" "$tmp/system" 2>/dev/null || true
    sync -f "$tmp/nix.erofs"; sync -f "$tmp/nix.erofs.sig"; sync -f "$tmp"
    [[ ! -e "$out" ]] || die "output already exists: $out"
    mv "$tmp" "$out"; printf '%s\n' "$id"
    ;;
  install)
    [[ $# -eq 3 ]] || usage
    bundle=$2; root=$3; require_root "$root"
    [[ -d "$bundle" ]] || die "bundle is not a directory: $bundle"
    id=$(cat "$bundle/generation"); valid_id "$id" || die "bundle has invalid generation id"
    actual=$(sha512sum "$bundle/nix.erofs" | cut -d' ' -f1)
    [[ "$actual" = "$id" ]] || die "bundle image hash does not match generation id"
    [[ -s "$bundle/nix.erofs.sig" ]] || die "bundle signature is absent or empty"
    install -d -m 0700 "$root" "$root/generations"
    if [[ ! -d "$root/generations/$id" ]]; then
      tmp="$root/generations/.incoming-$id.$$"; install -d -m 0700 "$tmp"
      install -m 0444 "$bundle/nix.erofs" "$tmp/nix.erofs"
      install -m 0444 "$bundle/nix.erofs.sig" "$tmp/nix.erofs.sig"
      [[ ! -f "$bundle/system" ]] || install -m 0444 "$bundle/system" "$tmp/system"
      [[ ! -f "$bundle/config.toml" ]] || install -m 0444 "$bundle/config.toml" "$tmp/config.toml"
      [[ ! -f "$bundle/config.toml.sig" ]] || install -m 0444 "$bundle/config.toml.sig" "$tmp/config.toml.sig"
      for extra in kernel.sig initrd.sig rescue.sfs rescue.sfs.sig network.erofs network.erofs.sig; do
        [[ ! -f "$bundle/$extra" ]] || install -m 0444 "$bundle/$extra" "$tmp/$extra"
      done
      printf '%s\n' "$id" > "$tmp/generation"; chmod 0444 "$tmp/generation"
      sync -f "$tmp/nix.erofs"; sync -f "$tmp/nix.erofs.sig"
      [[ ! -f "$tmp/config.toml" ]] || sync -f "$tmp/config.toml"
      [[ ! -f "$tmp/config.toml.sig" ]] || sync -f "$tmp/config.toml.sig"
      sync -f "$tmp"
      mv "$tmp" "$root/generations/$id"; sync -f "$root/generations"
    fi
    validate_generation "$id" "$root"; printf '%s\n' "$id"
    ;;
  activate)
    [[ $# -eq 3 ]] || usage
    id=$2; root=$3; require_root "$root"; validate_generation "$id" "$root"
    [[ ! -L "$root/attempted" ]] || die "cannot activate while a boot attempt is unresolved"
    old=$(target_id active "$root" || true)
    tested=$(target_id tested "$root" || true)
    if [[ -z "$tested" && -n "$old" ]]; then
      replace_link tested "generations/$old" "$root"; tested=$old
    fi
    if [[ -n "$old" && "$old" != "$id" ]]; then replace_link previous "generations/$old" "$root"; fi
    replace_link active "generations/$id" "$root"
    if [[ "$tested" = "$id" ]]; then
      rm -f -- "$root/pending"
    else
      replace_link pending "generations/$id" "$root"
    fi
    rm -f -- "$root/rollback-event"; sync -f "$root"
    ;;
  rollback)
    [[ $# -eq 2 ]] || usage
    root=$2; require_root "$root"
    [[ ! -L "$root/attempted" ]] || die "cannot roll back while a boot attempt is unresolved"
    old=$(target_id active "$root" || true); prior=$(target_id previous "$root" || true)
    [[ -n "$prior" ]] || die "no valid previous generation"
    validate_generation "$prior" "$root"
    [[ -z "$old" ]] || replace_link previous "generations/$old" "$root"
    replace_link active "generations/$prior" "$root"
    tested=$(target_id tested "$root" || true)
    if [[ "$tested" = "$prior" ]]; then
      rm -f -- "$root/pending"
    else
      replace_link pending "generations/$prior" "$root"
    fi
    sync -f "$root"
    ;;
  gc)
    [[ $# -eq 3 ]] || usage
    keep=$2; root=$3; require_root "$root"
    [[ "$keep" =~ ^[0-9]+$ ]] || die "KEEP must be a non-negative integer"
    active=$(target_id active "$root" || true); previous=$(target_id previous "$root" || true)
    tested=$(target_id tested "$root" || true); pending=$(target_id pending "$root" || true)
    attempted=$(target_id attempted "$root" || true); kept=0
    while read -r _ id; do
      valid_id "$id" || continue
      if [[ "$id" = "$active" || "$id" = "$previous" || "$id" = "$tested" \
        || "$id" = "$pending" || "$id" = "$attempted" || $kept -lt $keep ]]; then
        kept=$((kept + 1)); continue
      fi
      rm -rf -- "$root/generations/$id"
    done < <(find "$root/generations" -mindepth 1 -maxdepth 1 -type d \
      -name '[0-9a-f]*' -printf '%T@ %f\n' | sort -rn)
    sync -f "$root/generations"
    ;;
  status)
    [[ $# -eq 2 ]] || usage
    root=$2; require_root "$root"
    printf 'active=%s\n' "$(target_id active "$root" || echo invalid)"
    printf 'previous=%s\n' "$(target_id previous "$root" || echo none)"
    printf 'tested=%s\n' "$(target_id tested "$root" || echo none)"
    printf 'pending=%s\n' "$(target_id pending "$root" || echo none)"
    printf 'attempted=%s\n' "$(target_id attempted "$root" || echo none)"
    ;;
  *) usage ;;
esac
