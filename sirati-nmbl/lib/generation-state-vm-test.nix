{ pkgs, source, nmblSign, nmblErofsCtl, nmblErofsReceive, nmblErofsDeploy }:

pkgs.writeShellApplication {
  name = "nmbl-generation-state-vm-test";
  runtimeInputs = [ pkgs.coreutils pkgs.e2fsprogs pkgs.nix pkgs.python3 ];
  text = builtins.replaceStrings
    [ "@source@" "@signer@" "@ctl@" "@receive@" "@deploy@" "@scanner@" "@eval@" "@harness@" "@qemu@" ]
    [
      (toString source)
      (toString nmblSign)
      (toString nmblErofsCtl)
      (toString nmblErofsReceive)
      (toString nmblErofsDeploy)
      (toString ../testing/scan-private-key.py)
      (toString ../testing/generation-state-vm/eval.nix)
      (toString ../testing/generation-state-vm/harness.py)
      "${pkgs.qemu_kvm}/bin/qemu-system-x86_64"
    ]
    (builtins.readFile ../tools/generation-state-vm-test.sh);
  inheritPath = false;
}
