# Target: single GPT disk with vda3 wrapped in a LUKS container
# unlocked by passphrase "test".
#
# NMBL stage-0 unlocks via the TUI passphrase modal, passes the
# typed passphrase through to the kexec'd initrd at
# /etc/nmbl-luks/cryptroot, and stage-1's keyFile picks it up so
# the operator only types the passphrase once.
{ pkgs, lib, ... }:
{
  id = "luks-password";
  description = "single GPT disk, LUKS-on-vda3, passphrase unlock";
  diskoModule = ./disko/luks-password.nix;
  extraInitrdKernelModules = [
    "dm_mod"
    "dm-crypt"
    "aesni_intel"
  ];
  # Linux 6.6 trips a crypto-API init bug in dm-crypt; use latest so
  # NMBL stage-0 can actually open the volume.
  nmblKernelPackage = pkgs.linuxPackages_latest.kernel;
  diskCount = 1;
  extraModules = [
    ({ lib, ... }: {
      # NMBL has no udev; storage drivers have to be explicitly listed
      # in boot.initrd.kernelModules so the bootloader can open the
      # LUKS device.
      boot.initrd.kernelModules = [ "dm_mod" "dm-crypt" "aesni_intel" ];
      boot.nmbl.activation.luks = [
        {
          name = "cryptroot";
          device = "/dev/vda3";
          unlock = "password";
          promptLabel = "Enter LUKS passphrase for cryptroot";
          passToStage1 = "/etc/nmbl-luks/cryptroot";
        }
      ];
      # Tell the post-kexec NixOS initrd to read the injected
      # passphrase instead of prompting. fallbackToPassword keeps
      # the operator able to recover if injection ever fails.
      boot.initrd.luks.devices.cryptroot = lib.mkForce {
        device = "/dev/disk/by-partlabel/disk-main-luks";
        keyFile = "/etc/nmbl-luks/cryptroot";
        fallbackToPassword = true;
        allowDiscards = true;
      };
    })
  ];
}
