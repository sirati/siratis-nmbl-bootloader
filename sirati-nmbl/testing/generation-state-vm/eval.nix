{ source, publicKeyPath, publicKeyHash }:

let
  flake = builtins.getFlake "path:${source}";
  publicKey = builtins.path {
    path = publicKeyPath;
    name = "nmbl-generation-state-vm-public.key";
    sha256 = publicKeyHash;
  };
  system = import ./configuration.nix {
    nixpkgs = flake.inputs.nixpkgs;
    nmblModule = flake.nixosModules.default;
    inherit publicKey;
  };
  build = system.config.system.build;
  pkgs = flake.inputs.nixpkgs.legacyPackages.x86_64-linux;
in
pkgs.linkFarm "nmbl-generation-state-vm-artifacts" [
  { name = "kernel"; path = "${build.nmblKernel}/bzImage"; }
  { name = "initrd"; path = "${build.nmblInitramfs}/initrd"; }
  { name = "config.toml"; path = build.nmblConfigToml; }
  { name = "rescue.sfs"; path = build.nmblRescueSquashfs; }
  { name = "generation.erofs"; path = build.nmblGenerationImage; }
  { name = "toplevel"; path = build.toplevel; }
]
