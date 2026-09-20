set -euo pipefail

usage() {
  cat >&2 <<'EOF'
usage:
  nmbl-erofs-deploy INSTALLABLE PRIVATE_KEY IMAGE_ROOT
  nmbl-erofs-deploy remote INSTALLABLE PRIVATE_KEY SSH_TARGET [--reboot]

INSTALLABLE is a NixOS configuration, for example:
  .#nixosConfigurations.host

The command builds unsigned generation and runtime-config artifacts and signs
outside Nix with PRIVATE_KEY. Remote mode streams both signatures and payloads
to a forced nmbl-erofs-receive command over SSH for verification and activation.
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
  config=$(nix build "${nix_args[@]}" --no-link --print-out-paths \
    "$installable.config.system.build.nmblConfigToml")
  toplevel=$(nix build "${nix_args[@]}" --no-link --print-out-paths \
    "$installable.config.system.build.toplevel")
  rescue=$(nix build "${nix_args[@]}" --no-link --print-out-paths \
    "$installable.config.system.build.nmblRescueSquashfs")
  signer=$(nix build "${nix_args[@]}" --no-link --print-out-paths \
    "$installable.config.system.build.nmblSign")
  install -m 0444 "$config" "$bundle/config.toml"
  "$signer/bin/nmbl-sign" sign --key "$private_key" --domain boot-config \
    --out "$bundle/config.toml.sig" "$bundle/config.toml"
  install -m 0444 "$toplevel/kernel" "$bundle/kernel"
  install -m 0444 "$toplevel/initrd" "$bundle/initrd"
  "$signer/bin/nmbl-sign" sign --key "$private_key" --domain gen-kernel \
    --out "$bundle/kernel.sig" "$bundle/kernel"
  "$signer/bin/nmbl-sign" sign --key "$private_key" --domain gen-initrd \
    --out "$bundle/initrd.sig" "$bundle/initrd"
  install -m 0444 "$rescue" "$bundle/rescue.sfs"
  "$signer/bin/nmbl-sign" sign --key "$private_key" --domain rescue-sfs \
    --out "$bundle/rescue.sfs.sig" "$bundle/rescue.sfs"
  network_enabled=$(nix eval "${nix_args[@]}" --json \
    "$installable.config.boot.nmbl.rescue.fullSystem.networkStage.enable")
  network_size=0 network_signature_size=0
  if [[ "$network_enabled" = true ]]; then
    network=$(nix build "${nix_args[@]}" --no-link --print-out-paths \
      "$installable.config.system.build.nmblNetworkStage")
    install -m 0444 "$network" "$bundle/network.erofs"
    "$signer/bin/nmbl-sign" sign --key "$private_key" --domain network-stage \
      --out "$bundle/network.erofs.sig" "$bundle/network.erofs"
    network_size=$(stat -c %s "$bundle/network.erofs")
    network_signature_size=$(stat -c %s "$bundle/network.erofs.sig")
  fi
  config_id=$(sha512sum "$bundle/config.toml" | cut -d' ' -f1)
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
  config_size=$(stat -c %s "$bundle/config.toml")
  config_signature_size=$(stat -c %s "$bundle/config.toml.sig")
  kernel_size=$(stat -c %s "$bundle/kernel")
  kernel_signature_size=$(stat -c %s "$bundle/kernel.sig")
  initrd_size=$(stat -c %s "$bundle/initrd")
  initrd_signature_size=$(stat -c %s "$bundle/initrd.sig")
  rescue_size=$(stat -c %s "$bundle/rescue.sfs")
  rescue_signature_size=$(stat -c %s "$bundle/rescue.sfs.sig")
  {
    printf 'NMBL-EROFS-BUNDLE-3\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n' \
      "$generation" "$image_size" "$signature_size" "$system_size" \
      "$config_id" "$config_size" "$config_signature_size" \
      "$kernel_size" "$kernel_signature_size" "$initrd_size" "$initrd_signature_size" \
      "$rescue_size" "$rescue_signature_size" "$network_size" "$network_signature_size" "$reboot"
    cat "$payload/nix.erofs" "$payload/nix.erofs.sig"
    [[ ! -f "$payload/system" ]] || cat "$payload/system"
    cat "$bundle/config.toml" "$bundle/config.toml.sig"
    cat "$bundle/kernel" "$bundle/kernel.sig" "$bundle/initrd" "$bundle/initrd.sig"
    cat "$bundle/rescue.sfs" "$bundle/rescue.sfs.sig"
    [[ "$network_enabled" != true ]] || cat "$bundle/network.erofs" "$bundle/network.erofs.sig"
  } | "$ssh_command" -- "$destination" "$remote_command"
fi
printf '%s\n' "$generation"
