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
  echo "Staging external NMBL config to ${lib.escapeShellArg destination}..."
  install -D -m 0644 ${nmblConfigToml} ${lib.escapeShellArg destination}
  ${lib.optionalString (signingEnabled && !deferInstallSigning) ''
    echo "Signing external NMBL config..."
    ${signer.sign {
      domain = "boot-config";
      out = lib.escapeShellArg signature;
      input = lib.escapeShellArg destination;
    }}
    chmod 0644 ${lib.escapeShellArg signature}
  ''}
  ${lib.optionalString (signingEnabled && deferInstallSigning) ''
    echo "WARNING: external config signature deferred; sign ${destination} out of band." >&2
  ''}
  echo "✓ External config installed: ${destination}"
''
