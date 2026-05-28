# Start mode: KVM with `-kernel`/`-initrd` pointing at freshly-built
# NMBL artefacts and a freshly-built disk image.
#
# This is the original `mkTestVM` path. Fastest to set up (no full
# install), simplest contract (one ~4 GiB qcow2, no installer), and
# the default for iterating on NMBL itself.
#
# Inputs:
#   - target          (from testing/targets/<id>.nix)
#   - bootstrapper    (one of the bootstrapper attrsets in
#                      build-configurations.nix)
#   - configName      app name suffix (e.g. "luks-password",
#                      "btrfs-raid1")
#
# Output: an artefact value as defined in artefact.nix.
{
  self,
  nixpkgs,
  disko,
  system ? "x86_64-linux",
}:
let
  pkgs = nixpkgs.legacyPackages.${system};
  lib = nixpkgs.lib;
  vmConfig = import ../vm-config.nix { inherit self nixpkgs disko system; };
  artefactLib = import ../artefact.nix { inherit nixpkgs system; };

  mkArtefact =
    {
      target,
      bootstrapper,
      configName,
      kernelArgs ? "console=ttyS0,115200 earlyprintk=serial,ttyS0,115200",
      memoryMb ? 2048,
      cores ? 4,
      extraModules ? [ ],
    }:
    let
      vmName = "kvm-kexec-${configName}";
      cfg = vmConfig.mkTestVM {
        name = vmName;
        inherit bootstrapper;
        extraModules = target.extraModules ++ extraModules;
        diskoModule = target.diskoModule;
        nmblKernelPackage =
          if target.nmblKernelPackage != null
          then target.nmblKernelPackage
          else pkgs.linux_6_6;
      };
    in
    artefactLib.artefactFromVmConfig {
      name = vmName;
      config = cfg;
      inherit kernelArgs memoryMb cores;
      startMode = "kvm-kexec";
    };
in
{
  inherit mkArtefact;
}
