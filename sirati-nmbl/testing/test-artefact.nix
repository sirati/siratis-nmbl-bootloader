# Common test-artefact value type and helpers.
#
# Every test variant — direct-kernel kexec, BIOS, UEFI, LUKS install,
# Btrfs raid, etc. — eventually produces the same shape of "stuff a
# QEMU needs to boot the system under test":
#
#   { name        — short identifier (used as workdir / session name)
#     kernel      — store path containing a bzImage (null for full-disk
#                   boots that go through the in-disk bootloader)
#     initrd      — store path containing an initrd (null for full-disk
#                   boots)
#     disks       — list of { path; format ? "qcow2"; iface ? "virtio";
#                              copyOnLaunch ? true }
#                   (`iface` not `if` — Nix keyword)
#     kernelArgs  — string appended to QEMU `-append`
#     bootMode    — "direct-kernel" | "bios" | "uefi"
#     memoryMb    — VM RAM (defaults to 2048)
#     cores       — VM vCPUs (defaults to 4)
#   }
#
# Renderers (`testing/serial-tmux-harness.nix`,
# `testing/test-runners.nix:mkRunner`, future SDL/VNC ones, …) consume
# this shape and don't care which test variant produced it. That keeps
# the "what to test" / "how to display it" axes orthogonal so each
# axis can grow without N×M duplication.
#
# Conversion helpers below adapt the existing NMBL VM configs
# (`mkTestVM` from `vm-config.nix`) and the nixos-anywhere installer
# outputs into the same artefact shape, so a single renderer can run
# any of them.

{ nixpkgs, system ? "x86_64-linux" }:

let
  lib = nixpkgs.lib;
in
rec {
  # Construct an artefact from raw fields. Validates only what cheap
  # `lib.assertMsg` can — the rest is renderer-side.
  mkArtefact =
    {
      name,
      kernel ? null,
      initrd ? null,
      disks ? [ ],
      kernelArgs ? "console=ttyS0,115200 earlyprintk=serial,ttyS0,115200",
      bootMode ? "direct-kernel",
      memoryMb ? 2048,
      cores ? 4,
      ovmfCode ? null,
      ovmfVars ? null,
    }:
    let
      validBootModes = [
        "direct-kernel"
        "bios"
        "uefi"
      ];
    in
    assert lib.assertMsg (builtins.elem bootMode validBootModes)
      "test-artefact: bootMode must be one of ${toString validBootModes}, got ${toString bootMode}";
    assert lib.assertMsg (
      bootMode != "direct-kernel" || (kernel != null && initrd != null)
    ) "test-artefact: direct-kernel bootMode requires both kernel and initrd";
    assert lib.assertMsg (
      bootMode != "uefi" || (ovmfCode != null)
    ) "test-artefact: uefi bootMode requires ovmfCode (and ovmfVars is auto-staged)";
    {
      inherit
        name
        kernel
        initrd
        disks
        kernelArgs
        bootMode
        memoryMb
        cores
        ovmfCode
        ovmfVars
        ;
    };

  # Build an artefact from an existing NMBL `mkTestVM` config (i.e.
  # one of the entries in `build_configurations.nix:mkTestConfigurations`).
  # The config exposes `system.build.{nmblKernel, nmblInitramfs,
  # vmDiskImage}`; we pull each into the artefact and pick the
  # bootMode from `boot.nmbl.bootstrapper.bootMode`.
  artefactFromVmConfig =
    {
      name,
      config,
      kernelArgs ? "console=ttyS0,115200 earlyprintk=serial,ttyS0,115200",
      memoryMb ? 2048,
      cores ? 4,
    }:
    let
      cfg = config.config;
      nmblBs = cfg.boot.nmbl.bootstrapper or null;
      sysBootMode =
        if nmblBs != null then
          if nmblBs.bootMode == "qemu_kernel_invoke" then
            "direct-kernel"
          else if nmblBs.bootMode == "bios" then
            "bios"
          else if nmblBs.bootMode == "uefi" then
            "uefi"
          else
            throw "artefactFromVmConfig: unknown bootstrapper.bootMode ${toString nmblBs.bootMode}"
        else
          "direct-kernel";
      diskQcow = cfg.system.build.vmDiskImage + "/nixos.qcow2";
      kernel = if sysBootMode == "direct-kernel" then cfg.system.build.nmblKernel else null;
      initrd = if sysBootMode == "direct-kernel" then cfg.system.build.nmblInitramfs else null;
      ovmfCode =
        if sysBootMode == "uefi" then
          "${nixpkgs.legacyPackages.${system}.OVMF.fd}/FV/OVMF_CODE.fd"
        else
          null;
      ovmfVars =
        if sysBootMode == "uefi" then
          "${nixpkgs.legacyPackages.${system}.OVMF.fd}/FV/OVMF_VARS.fd"
        else
          null;
    in
    mkArtefact {
      inherit
        name
        kernel
        initrd
        kernelArgs
        memoryMb
        cores
        ovmfCode
        ovmfVars
        ;
      bootMode = sysBootMode;
      disks = [
        {
          path = diskQcow;
          format = "qcow2";
          copyOnLaunch = true;
        }
      ];
    };

  # Quick sanity helper for renderer authors: returns a tidy
  # description string suitable for the `[harness]` banner.
  describeArtefact =
    a:
    let
      hasKernel = a.kernel != null;
      hasInitrd = a.initrd != null;
      nDisks = builtins.length a.disks;
    in
    "name=${a.name} bootMode=${a.bootMode} kernel=${if hasKernel then "yes" else "no"} "
    + "initrd=${if hasInitrd then "yes" else "no"} disks=${toString nDisks} "
    + "memoryMb=${toString a.memoryMb} cores=${toString a.cores}";
}
