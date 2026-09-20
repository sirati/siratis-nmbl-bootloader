{
  pkgs,
  lib,
  cfg,
  nmblRescueSquashfs,
  nmblNetworkStage,
  nmblSign,
}:

let
  stage = cfg.rescue.fullSystem.networkStage;
  configuredKey = cfg.signing.imageKeyFile;
  configuredKeyString = if configuredKey == null then "" else toString configuredKey;
  rescueDestination = lib.escapeShellArg cfg.rescue.sfsPath;
  networkDestination = lib.escapeShellArg stage.imagePath;
  suffix = lib.escapeShellArg cfg.signing.sigPathSuffix;
in
pkgs.writeShellApplication {
  name = "nmbl-install-rescue-stage";
  runtimeInputs = [ nmblSign ];
  text = ''
    set -euo pipefail

    boot_root="''${NMBL_BOOT_ROOT:-/boot}"
    key_file="''${NMBL_IMAGE_KEY_FILE:-${configuredKeyString}}"
    if [[ -z "$key_file" || ! -f "$key_file" ]]; then
      echo "NMBL network-stage signing key is missing: $key_file" >&2
      exit 1
    fi
    if [[ "$key_file" == /nix/store/* ]]; then
      echo "NMBL network-stage signing key must remain outside /nix/store" >&2
      exit 1
    fi

    ${import ./install-file-shell.nix { inherit pkgs; }}
    install_signed_image() {
      source_file="$1"
      relative_destination="$2"
      domain="$3"
      destination="$boot_root/$relative_destination"
      signature="$destination"${suffix}
      install_nmbl_file_if_changed "$source_file" "$destination" 0644
      temporary_signature="$signature.new.$$"
      trap 'rm -f "$temporary_signature"' RETURN
      nmbl-sign sign \
        --key "$key_file" \
        --domain "$domain" \
        --out "$temporary_signature" \
        "$destination"
      install_nmbl_file_if_changed "$temporary_signature" "$signature" 0644
    }

    install_signed_image ${nmblRescueSquashfs} ${rescueDestination} rescue-sfs
    ${lib.optionalString stage.enable ''
      install_signed_image ${nmblNetworkStage} ${networkDestination} network-stage
    ''}
  '';
}
