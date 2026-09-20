{ pkgs, nmblErofsCtl, nmblSign }:

pkgs.writeShellApplication {
  name = "nmbl-erofs-receive";
  runtimeInputs = [ pkgs.coreutils pkgs.diffutils ];
  text = builtins.replaceStrings
    [ "@ctl@" "@nmblSign@" "@systemctl@" ]
    [ (toString nmblErofsCtl) (toString nmblSign) "${pkgs.systemd}/bin/systemctl" ]
    (builtins.readFile ../tools/nmbl-erofs-receive.sh);
  inheritPath = false;
}
