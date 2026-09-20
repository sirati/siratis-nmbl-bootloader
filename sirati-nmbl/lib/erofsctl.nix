{ pkgs, nmblSign ? null }:

let
  signer = if nmblSign == null then "/nmbl-sign-unavailable" else toString nmblSign;
in pkgs.writeShellApplication {
  name = "nmbl-erofsctl";
  runtimeInputs = [ pkgs.coreutils pkgs.findutils pkgs.gnugrep pkgs.gnused ];
  text = builtins.replaceStrings [ "@nmblSign@" ] [ signer ] (
    builtins.readFile ../tools/nmbl-erofsctl.sh
  );
  inheritPath = false;
}
