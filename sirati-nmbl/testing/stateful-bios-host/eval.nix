# Artifacts for test-stateful-bios-host-vm: three generations sharing one NMBL
# boot chain, plus the closure the Btrfs root must carry. `automatic` is the
# only difference between the two scenarios the test boots.
{ source, automatic, sshPublicKeyPath, sshPublicKeyHash }:

let
  flake = builtins.getFlake "path:${source}";
  nixpkgs = flake.inputs.nixpkgs;
  pkgs = nixpkgs.legacyPackages.x86_64-linux;
  sshPublicKey = builtins.readFile (builtins.path {
    path = sshPublicKeyPath;
    name = "nmbl-stateful-vm-ssh.pub";
    sha256 = sshPublicKeyHash;
  });
  generation = variant: import ./configuration.nix {
    inherit nixpkgs variant automatic sshPublicKey;
    nmblModule = flake.nixosModules.default;
  };
  first = (generation 1).config.system.build;
  toplevels = map (v: (generation v).config.system.build.toplevel) [ 1 2 3 ];
  closure = pkgs.closureInfo { rootPaths = toplevels; };
in
pkgs.linkFarm "nmbl-stateful-bios-host-artifacts" ([
  { name = "nmbl-kernel"; path = "${first.nmblKernel}/bzImage"; }
  { name = "nmbl-initrd"; path = "${first.nmblInitramfs}/initrd"; }
  { name = "grub.cfg"; path = first.nmblGrubConfig; }
  { name = "config.toml"; path = first.nmblConfigToml; }
  { name = "rescue.sfs"; path = first.nmblRescueSquashfs; }
  { name = "nmbl-init"; path = first.nmblInit; }
  { name = "closure"; path = closure; }
] ++ pkgs.lib.imap1 (i: t: { name = "system-${toString i}"; path = t; }) toplevels)
