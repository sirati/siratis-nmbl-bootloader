# Production nmblctl writes must survive an immediate VM power loss on VFAT.
{ pkgs, nmblCtl }:
pkgs.testers.runNixOSTest {
  name = "nmblctl-vfat-rename-durability";
  nodes.machine = { ... }: {
    virtualisation.emptyDiskImages = [ 128 ];
    environment.systemPackages = [ pkgs.dosfstools nmblCtl ];
  };
  testScript = ''
    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.succeed("mkfs.vfat /dev/vdb; mkdir -p /mnt/state; mount -t vfat -o umask=0077 /dev/vdb /mnt/state")
    # First ten writes create new entries; next ten replace those same names.
    # No sync/umount/shutdown may intervene between the CLI and hard power loss.
    for iteration in range(20):
        directory = f"/mnt/state/selection-{iteration % 10}"
        machine.succeed(f"mkdir -p {directory}; nmblctl --state-dir {directory} default --latest")
        machine.crash()
        machine.start()
        machine.wait_for_unit("multi-user.target")
        machine.succeed("mkdir -p /mnt/state; mount -t vfat -o umask=0077 /dev/vdb /mnt/state")
        actual = machine.succeed(f"od -An -tx1 {directory}/boot-default").strip()
        assert actual == "6c 61 74 65 73 74 0a", f"write {iteration} lost canonical bytes: {actual!r}"
  '';
}
