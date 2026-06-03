# Self-registering import hub for NMBL's security/staged-boot NixOS modules
# (FIX-60). Auto-imports every `*.nix` under `./security/`, so each later
# vertical slice (#6 signing, #7 tpm, #8 driver-image, #9 staged-boot,
# #10 secure-boot) only has to DROP a file into `./security/` — no shared
# `imports +=` line is edited, so the parallel slices never textually
# conflict.
#
# Wired into the NMBL module tree by ONE line in lib/options.nix:
#     imports = [ … ./modules/default.nix ];
#
# Scope: ONLY the `./security/` subdir is auto-imported. The other files in
# this `modules/` directory are a mix of NixOS-module-style files already
# imported explicitly by lib/options.nix (activation.nix, log-import.nix,
# stateful.nix) and plain `{ lib, config }` helper functions imported
# positionally by lib/config.nix (fs-modules.nix, nic-modules.nix,
# assertions.nix, …). Globbing all of `modules/*.nix` would double-import the
# former and mis-call the latter, so the auto-import dir is deliberately the
# dedicated `./security/` namespace.

{ lib, ... }:

let
  securityDir = ./security;

  # Every `*.nix` in ./security/ EXCEPT a possible `default.nix`, sorted for
  # deterministic eval order. `readDir` on a directory with only a `.keep`
  # placeholder yields no `.nix` files, so the skeleton (#5p) imports nothing
  # and stays a no-op until the slices drop their files.
  securityModules =
    if builtins.pathExists securityDir then
      let
        entries = builtins.readDir securityDir;
        isNixModule =
          name: type:
          (type == "regular") && (lib.hasSuffix ".nix" name) && (name != "default.nix");
        names = lib.filter (n: isNixModule n entries.${n}) (builtins.attrNames entries);
      in
      map (n: securityDir + "/${n}") names
    else
      [ ];
in
{
  imports = securityModules;
}
