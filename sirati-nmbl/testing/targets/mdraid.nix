# Target: two-disk mdraid1 across vda + vdb. /boot is metadata=1.0
# (superblock at END so firmware can read the FAT signature at the
# start); / is metadata=1.2 because it is only touched after NMBL
# has assembled the array.
{ pkgs, lib, ... }:
{
  id = "mdraid";
  description = "two-disk mdraid1 (vda+vdb), GPT, ext4 root";
  diskoModule = ./disko/mdraid1.nix;
  extraInitrdKernelModules = [
    "md_mod"
    "raid1"
  ];
  # linuxPackages_latest.kernel — md_mod in NMBL's initrd must match
  # what the rescue installer's mdadm wrote into the pad3 region.
  # 6.18's pad3-zero check rejects superblocks whose pad3 carries the
  # logical_block_size field introduced in 6.19.
  nmblKernelPackage = pkgs.linuxPackages_latest.kernel;
  diskCount = 2;
  extraModules = [
    ({ lib, ... }: {
      # md_mod + raid1 must be in boot.initrd.kernelModules (not just
      # availableKernelModules): NMBL has no udev, so storage drivers
      # have to be explicitly loaded.
      boot.initrd.kernelModules = [ "md_mod" "raid1" ];
    })
  ];
}
