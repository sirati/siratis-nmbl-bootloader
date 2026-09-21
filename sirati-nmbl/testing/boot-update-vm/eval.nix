{ source, publicKeyPath, publicKeyHash }:

let
  flake = builtins.getFlake "path:${source}";
  publicKey = builtins.path {
    path = publicKeyPath;
    name = "nmbl-boot-update-vm-public.key";
    sha256 = publicKeyHash;
  };
  config = import ./configuration.nix {
    inherit publicKey;
    inherit (flake.inputs) nixpkgs;
    nmblModule = flake.nixosModules.default;
  };
  build = config.config.system.build;
  pkgs = flake.inputs.nixpkgs.legacyPackages.x86_64-linux;
in pkgs.linkFarm "nmbl-boot-update-vm-artifacts" [
  { name = "source-A"; path = build.nmblBootSetSources.A; }
  { name = "source-B"; path = build.nmblBootSetSources.B; }
  { name = "tool"; path = build.nmblBootSetTool; }
  { name = "update"; path = flake.packages.x86_64-linux.nmbl-boot-update; }
  { name = "grub.cfg"; path = build.nmblGrubConfig; }
]
