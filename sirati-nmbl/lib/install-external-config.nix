# Stage and, when verification is enabled, sign the external runtime config.
{
  lib,
  cfg,
  nmblConfigToml,
  nmblSign ? null,
  deferInstallSigning ? false,
}:

let
  signingEnabled = cfg.signing.enable or false;
  # generationKeyFile or generationKeyCommand (lib/signing-key.nix).
  signer = import ./signing-key.nix { inherit lib; } {
    nmblSignBin = "${toString nmblSign}/bin/nmbl-sign";
    keyFile = cfg.signing.generationKeyFile or null;
    keyCommand = cfg.signing.generationKeyCommand or null;
  };
  checked =
    assert lib.assertMsg (!signingEnabled || deferInstallSigning || nmblSign != null) ''
      External NMBL config signing is enabled, but nmblSign is unavailable.
    '';
    assert lib.assertMsg (!signingEnabled || deferInstallSigning || signer.configured) ''
      External NMBL config signing requires signing.generationKeyFile or
      signing.generationKeyCommand. A key file must be an install-time STRING
      path outside the Nix store.
    '';
    assert lib.assertMsg (!signingEnabled || !signer.keyInStore) ''
      signing.generationKeyFile resolves inside the Nix store. Private signing
      keys must be install-time STRING paths outside the store.
    '';
    true;
  relativePath =
    let path = cfg.bootstrap.configPath or "/nmbl/config.toml";
    in if lib.hasPrefix "/" path then lib.removePrefix "/" path else path;
  destination = "/boot/${relativePath}";
  signature = "${destination}${cfg.signing.sigPathSuffix}";
in
assert checked;
''
  # Each file reaches the ESP through a synced temporary and a rename
  # (install_nmbl_file_if_changed, install-file-shell.nix): a power loss
  # right after an update must not leave a truncated config behind.
  echo "Staging external NMBL config to ${lib.escapeShellArg destination}..."
  ${lib.optionalString (signingEnabled && !deferInstallSigning) ''
    echo "Signing external NMBL config..."
    mkdir -p "$(dirname ${lib.escapeShellArg signature})"
    temporary_signature=${lib.escapeShellArg signature}.new.$$
    ${signer.sign {
      domain = "boot-config";
      out = "\"$temporary_signature\"";
      input = toString nmblConfigToml;
    }}
  ''}
  install_nmbl_file_if_changed ${nmblConfigToml} ${lib.escapeShellArg destination} 0644
  ${lib.optionalString (signingEnabled && !deferInstallSigning) ''
    install_nmbl_file_if_changed "$temporary_signature" ${lib.escapeShellArg signature} 0644
    rm -f "$temporary_signature"
  ''}
  ${lib.optionalString (signingEnabled && deferInstallSigning) ''
    echo "WARNING: external config signature deferred; sign ${destination} out of band." >&2
  ''}
  echo "✓ External config installed: ${destination}"
''
