{ pkgs }:

pkgs.writeShellApplication {
  name = "nmbl-erofs-deploy";
  runtimeInputs = [ pkgs.coreutils pkgs.findutils pkgs.nix pkgs.openssh ];
  text = builtins.readFile ../tools/nmbl-erofs-deploy.sh;
  inheritPath = false;
}
