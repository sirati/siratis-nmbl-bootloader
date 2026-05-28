# Target: single-disk btrfs root (no raid). /boot stays vfat because
# firmware can't read btrfs.
{ pkgs, lib, ... }:
{
  id = "btrfs";
  description = "single GPT disk, btrfs root, /boot vfat";
  diskoModule = ./disko/btrfs.nix;
  extraInitrdKernelModules = [ "btrfs" ];
  nmblKernelPackage = null;
  diskCount = 1;
  extraModules = [
    ({ lib, ... }: {
      boot.initrd.kernelModules = [ "btrfs" ];
    })
  ];
}
