{
  pkgs,
  source,
  nmblSign,
  nmblErofsCtl,
  nmblErofsDeploy,
}:

pkgs.writeShellApplication {
  name = "nmbl-generation-image-vm-test";
  runtimeInputs = [
    pkgs.coreutils
    pkgs.e2fsprogs
    pkgs.findutils
    pkgs.nix
    pkgs.python3
  ];
  text = builtins.replaceStrings
    [ "@source@" "@signer@" "@ctl@" "@deploy@" "@scanner@" ]
    [
      (toString source)
      (toString nmblSign)
      (toString nmblErofsCtl)
      (toString nmblErofsDeploy)
      (toString ../testing/scan-private-key.py)
    ]
    (builtins.readFile ../tools/generation-image-vm-test.sh);
  inheritPath = false;
}
