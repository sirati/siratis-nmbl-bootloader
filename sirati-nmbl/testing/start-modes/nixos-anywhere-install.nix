# Start mode: full nixos-anywhere install onto blank qcow2 disks.
#
# Wraps the existing orchestrator from nixos-anywhere-test/flake.nix.
# This start mode is special among the four: its artefact carries an
# explicit `installRunner` derivation (a writeShellApplication) that
# performs the rescue-VM boot, nixos-anywhere install, and stage-3
# verification end-to-end. The interaction renderers for this start
# mode are limited to `screen` (the legacy behaviour, drives the
# stage-3 VM serial through screen/socat) and `vnc-demo` (the existing
# noVNC bridge variant).
#
# The `disks` field of the produced artefact lists the qcow2 paths
# the orchestrator will leave at `$WORK_DIR/disk{1,2}.qcow2`. This is
# how kvm-kexec-installed gets the disk filenames it needs.
#
# Inputs are deliberately permissive: we accept the same orchestrator
# input set that lived in nixos-anywhere-test/install-configs.nix.
{
  self,
  nixpkgs,
  disko,
  nixos-anywhere,
  system ? "x86_64-linux",
}:
let
  pkgs = nixpkgs.legacyPackages.${system};
  lib = nixpkgs.lib;
  artefactLib = import ../artefact.nix { inherit nixpkgs system; };

  # Minimal NixOS-config builder. Same shape as the legacy
  # `install-configs.nix:mkInstall` but parameterised over a target +
  # bootstrapper instead of carrying its own disko branch.
  mkInstallConfig =
    {
      target,
      bootstrapper,
      configName,
      extraModules ? [ ],
    }:
    let
      hostName = "install-${configName}";
      hasMdRaid = builtins.any (
        m: m == "raid1" || m == "raid0" || m == "raid10" || m == "raid456"
      ) target.extraInitrdKernelModules;
      nmblKernelPackage =
        if target.nmblKernelPackage != null then target.nmblKernelPackage else pkgs.linux_6_6;
    in
    nixpkgs.lib.nixosSystem {
      inherit system;
      modules =
        [
          disko.nixosModules.disko
          target.diskoModule
          self.nixosModules.default
          "${nixpkgs}/nixos/modules/profiles/qemu-guest.nix"
          (
            { lib, ... }:
            {
              boot.nmbl = {
                enable = true;
                inherit bootstrapper;
                kernelPackage = nmblKernelPackage;
                kernelModules = [ ];
                mountPrefix = "/mnt";
                kernelParams = [
                  "console=ttyS0,115200"
                  "earlyprintk=serial,ttyS0,115200"
                  "dyndbg=file super1.c +p"
                ];
                timeoutSeconds = 5;
                serialConsole = "ttyS0,115200";
              };

              boot.kernelParams = [
                "console=ttyS0,115200"
                "earlyprintk=serial,ttyS0,115200"
                "loglevel=7"
              ];

              boot.initrd.availableKernelModules =
                [ "crc32c" ] ++ target.extraInitrdKernelModules;
              boot.initrd.kernelModules =
                [ "virtio_pci" "virtio_blk" ] ++ target.extraInitrdKernelModules;

              # Mdraid plumbing for the post-kexec NixOS scripted-stage-1.
              # See the equivalent block in the legacy install-configs.nix
              # for the rationale.
              boot.swraid.enable = lib.mkDefault hasMdRaid;

              boot.initrd.extraUtilsCommands = lib.mkIf hasMdRaid (lib.mkAfter ''
                copy_bin_and_libs ${pkgs.mdadm}/sbin/mdadm
              '');

              boot.initrd.preLVMCommands = lib.mkIf hasMdRaid (lib.mkBefore ''
                echo "NMBL-test: scripted-stage-1 calling mdadm --assemble --scan"
                mdadm --assemble --scan || true
              '');

              boot.kernelPackages = lib.mkIf hasMdRaid pkgs.linuxPackages_latest;

              boot.loader.grub.enable = false;
              boot.loader.systemd-boot.enable = false;

              networking.hostName = hostName;
              networking.useDHCP = true;
              networking.firewall.allowedTCPPorts = [ 22 ];

              services.openssh = {
                enable = true;
                settings = {
                  PermitRootLogin = "prohibit-password";
                  PasswordAuthentication = false;
                };
              };

              users.users.root.openssh.authorizedKeys.keys = [ ];

              services.getty.autologinUser = "root";

              environment.systemPackages = with pkgs; [
                vim
                htop
                kexec-tools
              ];

              system.stateVersion = "24.05";
            }
          )
        ]
        ++ target.extraModules
        ++ extraModules;
    };

  # Build the artefact: kernel/initrd absent (we boot from disk), and
  # the disks list carries the eventual qcow2 paths so kvm-kexec-installed
  # can find them.
  mkArtefact =
    {
      target,
      bootstrapper,
      configName,
      diskSizeGb ? 16,
      memoryMb ? 2048,
      cores ? 4,
      port ? 22001,
    }:
    let
      vmName = "nixos-anywhere-install-${configName}";
      cfg = mkInstallConfig { inherit target bootstrapper configName; };
      bootMode =
        if bootstrapper.bootMode == "uefi" then "uefi"
        else if bootstrapper.bootMode == "bios" then "bios"
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
      # Build a placeholder disks list. paths are null because the
      # actual disks are produced at runtime by the install orchestrator;
      # the count matches what the target needs.
      placeholderDisks = builtins.genList (idx: {
        path = null;
        format = "qcow2";
        iface = "virtio";
        copyOnLaunch = false;
        sizeGb = diskSizeGb;
      }) target.diskCount;
    in
    (artefactLib.mkArtefact {
      name = vmName;
      kernel = null;
      initrd = null;
      disks = placeholderDisks;
      bootMode = bootMode;
      startMode = "nixos-anywhere-install";
      inherit memoryMb cores ovmfCode ovmfVars;
    })
    // {
      # Auxiliary metadata for renderers that drive the install itself.
      installConfig = cfg;
      installPort = port;
    };
in
{
  inherit mkInstallConfig mkArtefact;
}
