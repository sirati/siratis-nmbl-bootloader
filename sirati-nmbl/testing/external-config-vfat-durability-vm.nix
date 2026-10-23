# A staged external config must survive a power loss right after the install
# on VFAT. A config truncated on the ESP boots NMBL with no modules and no
# filesystems: "nothing is mounted at /mnt/system".
{ pkgs, lib }:
let
  stage = text: pkgs.writeShellScript "stage-external-config" ''
    set -e
    ${import ../lib/install-file-shell.nix { inherit pkgs; }}
    ${import ../lib/install-external-config.nix {
      inherit lib;
      cfg = { bootstrap.configPath = "/nmbl/config.toml"; signing.enable = false; };
      nmblConfigToml = pkgs.writeText "config.toml" text;
    }}
  '';
  configs = map (n: lib.concatStrings (lib.genList (i: "# config ${toString n} line ${toString i}\n") 64)) [ 1 2 ];
  scripts = map stage configs;
  expected = map (text: pkgs.writeText "expected.toml" text) configs;
in
pkgs.testers.runNixOSTest {
  name = "external-config-vfat-durability";
  nodes.machine = { ... }: {
    virtualisation.emptyDiskImages = [ 128 ];
    environment.systemPackages = [ pkgs.dosfstools ];
  };
  testScript = ''
    scripts = ${builtins.toJSON (map toString scripts)}
    expected = ${builtins.toJSON (map toString expected)}
    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.succeed("mkfs.vfat /dev/vdb; mkdir -p /boot; mount -t vfat -o umask=0077 /dev/vdb /boot")
    # Every install replaces the previous config; no sync, umount or
    # shutdown may intervene between the install and the power loss.
    for iteration in range(6):
        machine.succeed(scripts[iteration % 2])
        machine.crash()
        machine.start()
        machine.wait_for_unit("multi-user.target")
        machine.succeed("mkdir -p /boot; mount -t vfat -o umask=0077 /dev/vdb /boot")
        machine.succeed(f"cmp {expected[iteration % 2]} /boot/nmbl/config.toml")
  '';
}
