{ pkgs }:

let
  inner = pkgs.writeShellApplication {
    name = "nmbl-erofs-deploy";
    runtimeInputs = [ pkgs.coreutils pkgs.findutils pkgs.nix pkgs.openssh ];
    text = builtins.readFile ../tools/nmbl-erofs-deploy.sh;
    inheritPath = false;
  };
in
# The wrapper records the caller's PATH for NMBL_SIGN_KEY_COMMAND.
import ./caller-path-wrapper.nix { inherit pkgs; } "nmbl-erofs-deploy" inner
