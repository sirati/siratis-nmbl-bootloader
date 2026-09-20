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
  keyFile = cfg.signing.generationKeyFile or null;
  keyString = if keyFile == null then null else toString keyFile;
  keyInStore = keyString != null && lib.hasPrefix builtins.storeDir keyString;
  checked =
    assert lib.assertMsg (!signingEnabled || deferInstallSigning || nmblSign != null) ''
      External NMBL config signing is enabled, but nmblSign is unavailable.
    '';
    assert lib.assertMsg (!signingEnabled || deferInstallSigning || keyFile != null) ''
      External NMBL config signing requires signing.generationKeyFile.
      Pass an install-time STRING path outside the Nix store.
    '';
    assert lib.assertMsg (!signingEnabled || !keyInStore) ''
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
    ${nmblSign}/bin/nmbl-sign sign \
      --key ${lib.escapeShellArg keyString} \
      --domain boot-config \
      --out ${lib.escapeShellArg signature} \
      ${lib.escapeShellArg destination}
    chmod 0644 ${lib.escapeShellArg signature}
  ''}
  ${lib.optionalString (signingEnabled && deferInstallSigning) ''
    echo "WARNING: external config signature deferred; sign ${destination} out of band." >&2
  ''}
  echo "✓ External config installed: ${destination}"
''
