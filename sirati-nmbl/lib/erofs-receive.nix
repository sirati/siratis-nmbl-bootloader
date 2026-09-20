{ pkgs, nmblErofsCtl }:

pkgs.writeShellApplication {
  name = "nmbl-erofs-receive";
  runtimeInputs = [ pkgs.coreutils ];
  text = builtins.replaceStrings
    [ "@ctl@" "@systemctl@" ]
    [ (toString nmblErofsCtl) "${pkgs.systemd}/bin/systemctl" ]
    (builtins.readFile ../tools/nmbl-erofs-receive.sh);
  inheritPath = false;
}
