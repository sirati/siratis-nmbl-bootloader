set -euo pipefail

usage() {
  cat >&2 <<'EOF'
usage:
  nmbl-erofs-deploy INSTALLABLE PRIVATE_KEY IMAGE_ROOT
  nmbl-erofs-deploy remote INSTALLABLE PRIVATE_KEY SSH_TARGET [--reboot]

INSTALLABLE is a NixOS configuration, for example:
  .#nixosConfigurations.host

The command builds only the unsigned system.build.nmblGenerationImage, signs
outside Nix with PRIVATE_KEY, then atomically installs and selects it. Remote
mode streams the bundle to a forced nmbl-erofs-receive command over SSH.
PRIVATE_KEY and local IMAGE_ROOT paths must be outside /nix/store.
EOF
  exit 2
}

mode=local
if [[ ${1:-} = remote ]]; then mode=remote; shift; fi
if [[ "$mode" = local ]]; then
  [[ $# -eq 3 ]] || usage
else
  [[ $# -eq 3 || ( $# -eq 4 && $4 = --reboot ) ]] || usage
fi
installable=$1; private_key=$2; destination=$3
[[ -f "$private_key" ]] || { echo "private key is not a file" >&2; exit 1; }
[[ "$private_key" != /nix/store/* ]] || { echo "private key is in /nix/store" >&2; exit 1; }
if [[ "$mode" = local && ( "$destination" != /* || "$destination" = /nix/store* ) ]]; then
  echo "IMAGE_ROOT must be absolute and outside /nix/store" >&2
  exit 1
fi

nix_args=()
if [[ ${NMBL_EROFS_DEPLOY_IMPURE:-0} = 1 ]]; then nix_args+=(--impure); fi
image=$(nix build "${nix_args[@]}" --no-link --print-out-paths \
  "$installable.config.system.build.nmblGenerationImage")
ctl=$(nix build "${nix_args[@]}" --no-link --print-out-paths \
  "$installable.config.system.build.nmblErofsCtl")
bundle=$(mktemp -d --tmpdir nmbl-erofs-bundle.XXXXXXXX)
trap 'chmod -R u+w "$bundle" 2>/dev/null || true; find "$bundle" -delete' EXIT
generation=$("$ctl/bin/nmbl-erofsctl" prepare \
  "$image" "$private_key" "$bundle/generation")
if [[ "$mode" = local ]]; then
  "$ctl/bin/nmbl-erofsctl" install "$bundle/generation" "$destination"
  "$ctl/bin/nmbl-erofsctl" activate "$generation" "$destination"
else
  remote_command=${NMBL_EROFS_REMOTE_COMMAND:-nmbl-erofs-receive}
  ssh_command=${NMBL_EROFS_SSH:-ssh}
  [[ "$remote_command" =~ ^[A-Za-z0-9_./-]+$ ]] || {
    echo "invalid NMBL_EROFS_REMOTE_COMMAND" >&2; exit 1;
  }
  reboot=0; [[ ${4:-} != --reboot ]] || reboot=1
  payload="$bundle/generation"
  image_size=$(stat -c %s "$payload/nix.erofs")
  signature_size=$(stat -c %s "$payload/nix.erofs.sig")
  system_size=0
  [[ ! -f "$payload/system" ]] || system_size=$(stat -c %s "$payload/system")
  {
    printf 'NMBL-EROFS-BUNDLE-1\n%s\n%s\n%s\n%s\n%s\n' \
      "$generation" "$image_size" "$signature_size" "$system_size" "$reboot"
    cat "$payload/nix.erofs" "$payload/nix.erofs.sig"
    [[ ! -f "$payload/system" ]] || cat "$payload/system"
  } | "$ssh_command" -- "$destination" "$remote_command"
fi
printf '%s\n' "$generation"
