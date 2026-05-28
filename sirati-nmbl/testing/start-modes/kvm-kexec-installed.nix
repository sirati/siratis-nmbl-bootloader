# Start mode: KVM with `-kernel`/`-initrd` pointing at freshly-built
# NMBL artefacts AND `-drive` pointing at PRE-EXISTING qcow2 disks
# from a prior `nixos-anywhere-install` run.
#
# The whole point: skip the 10-minute install and rapidly iterate on
# the NMBL Rust code, while still testing against realistically-shaped
# disks (LUKS headers, real partition tables, etc.).
#
# Inputs:
#   - target          (storage layout: LUKS, mdraid, btrfs, …)
#   - bootstrapper    bootMode + loader_extra_args
#   - configName      app name suffix
#   - diskCount       overrides target.diskCount; usually unset
#
# Output: artefact { startMode = "kvm-kexec-installed"; disks = [{ path = null; ... }]; }
#
# The artefact's `disks` carry `path = null` placeholders — the
# renderer is expected to accept a runtime `--disks` CLI flag (a
# comma-separated list of qcow2 paths) and splice the paths in.
#
# diskAccess defaults to "snapshot" so botched runs do not corrupt
# the installed disks. Override via a renderer flag for tests that
# need to mutate them.
{
  self,
  nixpkgs,
  disko,
  system ? "x86_64-linux",
}:
let
  pkgs = nixpkgs.legacyPackages.${system};
  lib = nixpkgs.lib;
  artefactLib = import ../artefact.nix { inherit nixpkgs system; };
  # We reuse kvm-kexec's mkTestVM-based config plumbing to get a
  # freshly-built kernel + initrd that matches the target's
  # activation modules (LUKS keyfile injection, mdadm assemble, …).
  vmConfig = import ../vm-config.nix { inherit self nixpkgs disko system; };

  mkArtefact =
    {
      target,
      bootstrapper,
      configName,
      kernelArgs ? "console=ttyS0,115200 earlyprintk=serial,ttyS0,115200",
      memoryMb ? 2048,
      cores ? 4,
      extraModules ? [ ],
      diskCount ? null,
    }:
    let
      vmName = "kvm-kexec-installed-${configName}";
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
      kernel = cfgCfg.system.build.nmblKernel;
      initrd = cfgCfg.system.build.nmblInitramfs;
      nmblBs = cfgCfg.boot.nmbl.bootstrapper or null;
      # We always feed -kernel + -initrd here regardless of the
      # bootMode the bootstrapper was set to: the whole point of
      # kvm-kexec-installed is to override the in-disk loader chain
      # and feed our freshly-rebuilt NMBL initramfs directly.
      bootMode = "direct-kernel";
      effectiveDiskCount =
        if diskCount != null then diskCount else target.diskCount;
      placeholderDisks = builtins.genList (idx: {
        path = null;
        format = "qcow2";
        iface = "virtio";
        copyOnLaunch = false;
        readOnly = false;
      }) effectiveDiskCount;
    in
    artefactLib.mkArtefact {
      name = vmName;
      inherit kernel initrd kernelArgs memoryMb cores;
      bootMode = bootMode;
      startMode = "kvm-kexec-installed";
      diskAccess = "snapshot";
      disks = placeholderDisks;
    };
in
{
  inherit mkArtefact;
}
