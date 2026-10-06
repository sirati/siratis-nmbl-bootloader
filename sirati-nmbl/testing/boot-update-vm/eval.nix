{ source, publicKeyPath, publicKeyHash, previousKeyPath ? null, previousKeyHash ? null }:

let
  flake = builtins.getFlake "path:${source}";
  publicKey = builtins.path {
    path = publicKeyPath;
    name = "nmbl-boot-update-vm-public.key";
    sha256 = publicKeyHash;
  };
  previousKeys = if previousKeyPath == null then [ ] else [ (builtins.path {
    path = previousKeyPath;
    name = "nmbl-boot-update-vm-previous.key";
    sha256 = previousKeyHash;
  }) ];
  config = import ./configuration.nix {
    inherit publicKey previousKeys;
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
  { name = "toplevel"; path = build.toplevel; }
]
