{ pkgs, nmblSign ? null }:

let
  signer = if nmblSign == null then "/nmbl-sign-unavailable" else toString nmblSign;
  inner = pkgs.writeShellApplication {
    name = "nmbl-erofsctl";
    runtimeInputs = [ pkgs.coreutils pkgs.findutils pkgs.gnugrep pkgs.gnused ];
    text = builtins.replaceStrings [ "@nmblSign@" ] [ signer ] (
      builtins.readFile ../tools/nmbl-erofsctl.sh
    );
    inheritPath = false;
  };
in
# The wrapper records the caller's PATH for NMBL_SIGN_KEY_COMMAND.
import ./caller-path-wrapper.nix { inherit pkgs; } "nmbl-erofsctl" inner
