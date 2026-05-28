# Start mode: full NixOS-built VM image, system boots through its
# own in-disk bootloader chain rather than via -kernel/-initrd.
#
# Same NixOS configuration as kvm-kexec, but the artefact omits the
# kernel/initrd pair so the renderer doesn't pass `-kernel`/`-initrd`
# to QEMU. The bootloader chain inside the disk takes over.
#
# Useful for verifying the post-NMBL boot chain on its own: NMBL is
# installed onto /boot during make-disk-image, and the firmware /
# loader / kexec sequence runs end-to-end without needing a freshly
# rebuilt kernel-initrd pair on the host.
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
      vmName = "nix-build-vm-${configName}";
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
      cfgCfg = cfg.config;
      diskQcow = cfgCfg.system.build.vmDiskImage + "/nixos.qcow2";
      nmblBs = cfgCfg.boot.nmbl.bootstrapper or null;
      bootMode =
        if nmblBs != null && nmblBs.bootMode == "uefi" then "uefi"
        else if nmblBs != null && nmblBs.bootMode == "bios" then "bios"
        else "direct-kernel";
      ovmfCode =
        if bootMode == "uefi" then
          "${pkgs.OVMF.fd}/FV/OVMF_CODE.fd"
        else
          null;
      ovmfVars =
        if bootMode == "uefi" then
          "${pkgs.OVMF.fd}/FV/OVMF_VARS.fd"
        else
          null;
    in
    artefactLib.mkArtefact {
      name = vmName;
      kernel = null;
      initrd = null;
      inherit kernelArgs memoryMb cores ovmfCode ovmfVars;
      inherit bootMode;
      startMode = "nix-build-vm";
      disks = [
        {
          path = diskQcow;
          format = "qcow2";
          copyOnLaunch = true;
        }
      ];
    };
in
{
  inherit mkArtefact;
}
