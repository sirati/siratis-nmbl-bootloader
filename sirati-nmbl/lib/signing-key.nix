# One private-key source for install-time `nmbl-sign sign` calls.
#
# A signing key reaches `nmbl-sign` in exactly one of two ways:
#   * `keyFile`    — a STRING path to an on-disk `NMBLSK01` container, passed
#                    as `nmbl-sign sign --key <file>` (the historical form);
#   * `keyCommand` — an argv list whose STDOUT is that container. Every signing
#                    operation runs it afresh and pipes it straight into
#                    `nmbl-sign sign --key-stdin`; the key is never written to
#                    disk or cached between signatures.
#
# Usage:
#   signer = import ./signing-key.nix { inherit lib; } {
#     nmblSignBin = "${nmblSign}/bin/nmbl-sign";
#     keyFile = cfg.signing.generationKeyFile;
#     keyCommand = cfg.signing.generationKeyCommand;
#   };
#   signer.sign { domain = "gen-kernel"; out = "\"$dir/kernel.sig\""; input = "\"$k\""; }
#
# `out` and `input` are inserted verbatim, so callers pass them already
# shell-quoted (`lib.escapeShellArg …` or a double-quoted `"$var"`).
{ lib }:
{
  nmblSignBin,
  keyFile ? null,
  keyCommand ? null,
}:

let
  keyFileStr = if keyFile == null then null else toString keyFile;
in
{
  inherit keyFileStr;
  # True when either source is set; callers assert this where signing is needed.
  configured = keyFile != null || keyCommand != null;
  # A private key imported into the store would leak into the closure.
  keyInStore = keyFileStr != null && lib.hasPrefix builtins.storeDir keyFileStr;
  # Both set is a configuration error (the module asserts it too).
  ambiguous = keyFile != null && keyCommand != null;

  sign =
    { domain, out, input }:
    if keyCommand != null then
      # Subshell with pipefail: a failing key command must fail the step even
      # in callers running plain `set -e` (nmbl-sign would reject the empty
      # or truncated key anyway; pipefail makes the cause explicit).
      ''
        ( set -o pipefail
          ${lib.escapeShellArgs keyCommand} \
            | ${nmblSignBin} sign --key-stdin --domain ${domain} --out ${out} ${input} )''
    else
      ''
        ${nmblSignBin} sign --key ${lib.escapeShellArg keyFileStr} --domain ${domain} --out ${out} ${input}'';
}
