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
  lib = flake.inputs.nixpkgs.lib;
  invalid = profile: config.extendModules {
    modules = [ {
      boot.nmbl.rescue.fullSystem.networkStage.staticProfiles = lib.mkForce [ profile ];
    } ];
  };
  evaluates = systemConfig:
    (builtins.tryEval systemConfig.config.system.build.toplevel.drvPath).success;
  bothSelectors = invalid {
    interfaceName = "eth0";
    macAddress = "52:54:00:12:34:56";
    ipv4.addresses = [ "10.0.2.15/24" ];
  };
  noAddress = invalid { interfaceName = "eth0"; };
  wrongFamily = (invalid {
    interfaceName = "eth0";
    ipv6.addresses = [ "fec0::15/64" ];
  }).extendModules {
    modules = [ {
      boot.nmbl.rescue.fullSystem.networkStage.addressFamily = lib.mkForce "ipv4-only";
    } ];
  };
in
assert !evaluates bothSelectors;
assert !evaluates noAddress;
assert !evaluates wrongFamily;
pkgs.linkFarm "nmbl-network-stage-vm-artifacts" [
  { name = "kernel"; path = "${build.nmblKernel}/bzImage"; }
  { name = "initrd"; path = "${build.nmblInitramfs}/initrd"; }
  { name = "config.toml"; path = build.nmblConfigToml; }
  { name = "rescue.sfs"; path = build.nmblRescueSquashfs; }
  { name = "network.erofs"; path = build.nmblNetworkStage; }
  { name = "rescue-installer"; path = build.nmblRescueStageInstaller; }
]
