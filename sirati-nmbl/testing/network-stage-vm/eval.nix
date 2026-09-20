{
  source,
  publicKeyPath,
  publicKeyHash,
}:

let
  flake = builtins.getFlake "path:${source}";
  publicKey = builtins.path {
    path = publicKeyPath;
    name = "nmbl-network-stage-vm-public.key";
    sha256 = publicKeyHash;
  };
  config = flake.lib.mkNetworkStageVmConfig { inherit publicKey; };
  build = config.config.system.build;
  pkgs = flake.inputs.nixpkgs.legacyPackages.x86_64-linux;
in
pkgs.linkFarm "nmbl-network-stage-vm-artifacts" [
  { name = "kernel"; path = "${build.nmblKernel}/bzImage"; }
  { name = "initrd"; path = "${build.nmblInitramfs}/initrd"; }
  { name = "config.toml"; path = build.nmblConfigToml; }
  { name = "rescue.sfs"; path = build.nmblRescueSquashfs; }
  { name = "network.erofs"; path = build.nmblNetworkStage; }
  { name = "rescue-installer"; path = build.nmblRescueStageInstaller; }
]
