# Target: two-disk btrfs RAID1. Mirrors data + metadata at the
# filesystem layer (no mdadm superblock), sidesteps the
# kernel-6.18 / mdadm pad3-zero regression entirely.
{ pkgs, lib, ... }:
{
  id = "btrfs-raid1";
  description = "two-disk btrfs RAID1 (vda+vdb), /boot vfat on vda";
  diskoModule = ./disko/btrfs-raid1.nix;
  extraInitrdKernelModules = [ "btrfs" ];
  nmblKernelPackage = null;
  diskCount = 2;
  extraModules = [
    ({ lib, ... }: {
      boot.initrd.kernelModules = [ "btrfs" ];
    })
  ];
}
