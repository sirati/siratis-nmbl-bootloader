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
#                              copyOnLaunch ? true; readOnly ? false }
#                   `path` may be null for runtime-supplied disks; in
#                   that case the renderer fills it in from CLI args.
#                   (`iface` not `if` — Nix keyword)
#     kernelArgs  — string appended to QEMU `-append`
#     bootMode    — "direct-kernel" | "bios" | "uefi"
#     startMode   — "kvm-kexec" | "nix-build-vm"
#                 | "nixos-anywhere-install" | "kvm-kexec-installed"
#                 | "kexec-no-disk"
#                   Renderer-visible discriminator for which START MODE
#                   produced this artefact. Mostly affects defaults
#                   (e.g. snapshot vs. copy of disks, how disks paths
#                   are supplied at runtime).
#     memoryMb    — VM RAM (defaults to 2048)
#     cores       — VM vCPUs (defaults to 4)
#     ovmfCode    — UEFI firmware code (uefi bootMode only)
#     ovmfVars    — UEFI firmware vars template (uefi bootMode only)
#     runtimeDisks — bool; true if `disks` includes entries whose
#                    `path = null`, signalling the renderer to expose a
#                    `--disks PATH,PATH` CLI flag.
#     diskAccess  — "copy" (default) | "snapshot" | "readonly"
#                    Hints the renderer about how to expose the disks
#                    to QEMU. Most start modes default to `copy` (cp
#                    the qcow2 into the workdir); kvm-kexec-installed
#                    defaults to `snapshot` (qemu -snapshot, RAM-only).
#                    Per-disk entries may override via `disk.copyOnLaunch`
#                    or `disk.readOnly`.
#   }
#
# Renderers (interactions/{qemu-serial-rs,vnc,tmux}.nix, …) consume
# this shape and don't care which test variant produced it. That keeps
# the three orthogonal axes (start mode × target × interaction) at
# N+M+K complexity instead of N×M×K duplication.

{ nixpkgs, system ? "x86_64-linux" }:

let
  lib = nixpkgs.lib;
in
rec {
  validBootModes = [
    "direct-kernel"
    "bios"
    "uefi"
  ];

  validStartModes = [
    "kvm-kexec"
    "nix-build-vm"
    "nixos-anywhere-install"
    "kvm-kexec-installed"
    "kexec-no-disk"
  ];

  validDiskAccess = [
    "copy"
    "snapshot"
    "readonly"
  ];

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
      startMode ? "kvm-kexec",
      memoryMb ? 2048,
      cores ? 4,
      ovmfCode ? null,
      ovmfVars ? null,
      diskAccess ? "copy",
    }:
    let
      runtimeDisks = builtins.any (d: (d.path or null) == null) disks;
    in
    assert lib.assertMsg (builtins.elem bootMode validBootModes)
      "artefact: bootMode must be one of ${toString validBootModes}, got ${toString bootMode}";
    assert lib.assertMsg (builtins.elem startMode validStartModes)
      "artefact: startMode must be one of ${toString validStartModes}, got ${toString startMode}";
    assert lib.assertMsg (builtins.elem diskAccess validDiskAccess)
      "artefact: diskAccess must be one of ${toString validDiskAccess}, got ${toString diskAccess}";
    assert lib.assertMsg (
      bootMode != "direct-kernel" || (kernel != null && initrd != null)
    ) "artefact: direct-kernel bootMode requires both kernel and initrd";
    assert lib.assertMsg (
      bootMode != "uefi" || (ovmfCode != null)
    ) "artefact: uefi bootMode requires ovmfCode (and ovmfVars is auto-staged)";
    {
      inherit
        name
        kernel
        initrd
        disks
        kernelArgs
        bootMode
        startMode
        memoryMb
        cores
        ovmfCode
        ovmfVars
        diskAccess
        runtimeDisks
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
      startMode ? "kvm-kexec",
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
        startMode
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
    "name=${a.name} startMode=${a.startMode} bootMode=${a.bootMode} "
    + "kernel=${if hasKernel then "yes" else "no"} initrd=${if hasInitrd then "yes" else "no"} "
    + "disks=${toString nDisks} memoryMb=${toString a.memoryMb} cores=${toString a.cores}";
}
