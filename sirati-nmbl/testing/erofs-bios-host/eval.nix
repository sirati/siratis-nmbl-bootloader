# Artifacts for test-erofs-bios-host-vm, evaluated with ONLY the ephemeral
# operator public key and rescue-SSH public key (content-hash pinned).
{ source, publicKeyPath, publicKeyHash, sshPublicKeyPath, sshPublicKeyHash }:

let
  flake = builtins.getFlake "path:${source}";
  nixpkgs = flake.inputs.nixpkgs;
  pkgs = nixpkgs.legacyPackages.x86_64-linux;
  publicKey = builtins.path {
    path = publicKeyPath;
    name = "nmbl-bios-host-vm-public.key";
    sha256 = publicKeyHash;
  };
  sshPublicKey = builtins.readFile (builtins.path {
    path = sshPublicKeyPath;
    name = "nmbl-bios-host-vm-ssh.pub";
    sha256 = sshPublicKeyHash;
  });
  system = import ./configuration.nix {
    inherit nixpkgs publicKey sshPublicKey;
    nmblModule = flake.nixosModules.default;
    vmTest = true;
    algorithm = "ml-dsa-65";
  };
  build = system.config.system.build;
in
pkgs.linkFarm "nmbl-bios-host-vm-artifacts" [
  { name = "nmbl-kernel"; path = "${build.nmblKernel}/bzImage"; }
  { name = "nmbl-initrd"; path = "${build.nmblInitramfs}/initrd"; }
  { name = "grub.cfg"; path = build.nmblGrubConfig; }
  { name = "install-bootloader"; path = build.installBootLoader; }
]
