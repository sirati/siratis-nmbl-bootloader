{ pkgs, nmblInit }:

let
  id = builtins.concatStringsSep "" (builtins.genList (_: "a") 128);
  root = "/boot/nmbl-generations";
in
pkgs.testers.runNixOSTest {
  name = "nmbl-generation-success-late-secrets";
  nodes.machine = { lib, ... }: {
    imports = [ ../lib/modules/security/generation-success.nix ];
    options.boot.nmbl = lib.mkOption { type = lib.types.attrs; };
    config = {
      boot.nmbl = {
        generationImage = {
          enable = true;
          automaticRollback = true;
          successDelaySec = 1;
          stateRoot = root;
        };
        rescue.automatic = false;
      };
      system.build.nmblInit = nmblInit;
      fileSystems."/boot" = { device = "tmpfs"; fsType = "tmpfs"; };
      virtualisation.fileSystems."/boot" = { device = "tmpfs"; fsType = "tmpfs"; };
      systemd.tmpfiles.rules = [
        "d ${root}/generations/${id} 0700 root root - -"
        "f ${root}/generations/${id}/nix.erofs 0600 root root - -"
        "f ${root}/generations/${id}/nix.erofs.sig 0600 root root - -"
        "L ${root}/active - - - - generations/${id}"
        "L ${root}/attempted - - - - generations/${id}"
        "L ${root}/pending - - - - generations/${id}"
      ];
      systemd.services.mock-secret-ready = {
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
          ExecStart = "${pkgs.coreutils}/bin/test -e /run/mock-secret";
        };
      };
      systemd.services.nmbl-generation-success = {
        requires = [ "mock-secret-ready.service" ];
        after = [ "mock-secret-ready.service" ];
      };
      system.stateVersion = "26.05";
    };
  };
  testScript = ''
    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.succeed("mountpoint -q /boot")
    machine.wait_until_succeeds("systemctl is-failed mock-secret-ready.service")
    machine.succeed("test ! -e ${root}/tested")
    machine.succeed("test -L ${root}/attempted; test -L ${root}/pending")
    # Supply the missing credential without starting or resetting any unit.
    machine.succeed("touch /run/mock-secret")
    machine.wait_until_succeeds("test -L ${root}/tested", timeout=120)
    machine.wait_for_unit("nmbl-generation-success.service")
    machine.succeed("test $(readlink ${root}/active) = $(readlink ${root}/tested)")
    machine.succeed("test ! -L ${root}/attempted; test ! -L ${root}/pending")
    machine.succeed("systemctl is-active --quiet boot-complete.target")
    assert not machine.succeed("systemctl --failed --no-legend --plain").strip()
  '';
}
