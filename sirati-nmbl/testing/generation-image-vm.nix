{
  pkgs,
  nmblModule,
  publicKey,
}:

let
  node = diskEnvironment: import ./generation-image-node.nix {
    inherit nmblModule publicKey diskEnvironment;
  };
in
pkgs.testers.runNixOSTest {
  name = "nmbl-generation-image-vm";
  nodes = {
    happy = node "NMBL_DISK_HAPPY";
    tampered = node "NMBL_DISK_TAMPERED";
    unsigned = node "NMBL_DISK_UNSIGNED";
  };

  testScript = ''
    happy.start(allow_reboot=True)
    happy.wait_for_unit("multi-user.target", timeout=180)
    happy.succeed("findmnt -n -t erofs /nix")
    happy.succeed("test $(findmnt -no SOURCE -T /nix) = $(readlink -f /dev/nmbl-verified-generation)")
    first = happy.succeed("readlink /boot/nmbl-generations/active").strip()
    happy.succeed("old=$(basename $(readlink /boot/nmbl-generations/active)); other=$(find /boot/nmbl-generations/generations -mindepth 1 -maxdepth 1 -type d | grep -v /$old$); nmbl-erofsctl activate $(basename $other) /boot/nmbl-generations")
    happy.reboot()
    happy.wait_for_unit("multi-user.target", timeout=180)
    second = happy.succeed("readlink /boot/nmbl-generations/active").strip()
    assert second != first, "second signed generation was not selected"
    happy.succeed("findmnt -n -t erofs /nix")
    happy.succeed("nmbl-erofsctl rollback /boot/nmbl-generations")
    happy.reboot()
    happy.wait_for_unit("multi-user.target", timeout=180)
    assert happy.succeed("readlink /boot/nmbl-generations/active").strip() == first

    tampered.start()
    tampered.wait_for_shutdown()
    tampered_log = tampered.get_console_log()
    assert "nmbl-generation-mount:" in tampered_log
    assert "signature verification failed" in tampered_log

    unsigned.start()
    unsigned.wait_for_shutdown()
    unsigned_log = unsigned.get_console_log()
    assert "nmbl-generation-mount:" in unsigned_log
    assert "read sidecar" in unsigned_log
    assert "nix.erofs.sig" in unsigned_log
    assert "No such file or directory" in unsigned_log
  '';
}
