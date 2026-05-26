{ ... }:
# Two-disk btrfs RAID1 layout. Sidesteps the kernel-6.18 / mdadm pad3-zero
# regression entirely — btrfs has its own on-disk format and the kernel's
# acceptance check is over a completely different code path (super_load in
# fs/btrfs/disk-io.c, not super_1_load in drivers/md/md.c).
#
# Layout per disk (identical):
#   1M  EF02  bios_boot       (so grub-pc can install to either disk)
#   512M EF00 ESP / /boot     (vfat, only the vda copy is touched by bootctl)
#   rest      btrfs member
#
# /boot stays vfat (firmware can't read btrfs), so it's NOT mirrored — vda's
# ESP is what the bootloader writes to. Failing over to vdb's ESP is a
# separate problem (firmware boot-order tweak + manual sync) and is out of
# scope for this test variant.
#
# The btrfs filesystem is declared on the vdb disk (processed second by disko).
# This is the crucial ordering constraint: disko processes disks alphabetically,
# so vda is partitioned first. By the time the btrfs mkfs runs (declared on
# vdb's root partition), disk-vda-root already exists in /dev/disk/by-partlabel/.
# The final mkfs.btrfs cmdline is:
#   mkfs.btrfs ... -d raid1 -m raid1 -f /dev/disk/by-partlabel/disk-vda-root \
#                  /dev/disk/by-partlabel/disk-vdb-root
# (disko appends the primary device — disk-vdb-root — at the end).
# For btrfs, any member can be used to mount the filesystem, so declaring the
# filesystem on vdb works fine. NMBL's `btrfs device scan` finds both members.
#
# NMBL prerequisites (already wired up in sirati-nmbl):
#   1. `btrfs` kernel module in boot.initrd.kernelModules.
#   2. pkgs.btrfs-progs in the initrd's storePaths so `btrfs device scan`
#      can be called from mount-and-kernel.sh.
#   3. `btrfs device scan` invocation before the mount step — without it,
#      mounting via any single member fails with "open ctree failed" because
#      the kernel doesn't yet know the other member exists (there's no udev
#      to autoscan).
{
  disko.devices = {
    disk = {
      vda = {
        device = "/dev/vda";
        type = "disk";
        content = {
          type = "gpt";
          partitions = {
            bios_boot = {
              priority = 1;
              size = "1M";
              type = "EF02";
            };
            ESP = {
              priority = 2;
              size = "512M";
              type = "EF00";
              content = {
                type = "filesystem";
                format = "vfat";
                mountpoint = "/boot";
                mountOptions = [ "umask=0077" ];
              };
            };
            root = {
              priority = 3;
              size = "100%";
              # No content declared here. This partition is absorbed as a
              # btrfs member by the mkfs.btrfs invocation on vdb's root
              # partition (declared below). Disko processes vda before vdb,
              # so disk-vda-root will exist in /dev/disk/by-partlabel/ by
              # the time vdb's btrfs mkfs runs.
            };
          };
        };
      };
      vdb = {
        device = "/dev/vdb";
        type = "disk";
        content = {
          type = "gpt";
          partitions = {
            bios_boot = {
              priority = 1;
              size = "1M";
              type = "EF02";
            };
            # Placeholder so partition numbering matches vda
            # (vdb3 = btrfs member, just like vda3).
            esp_placeholder = {
              priority = 2;
              size = "512M";
              type = "8300";
            };
            root = {
              priority = 3;
              size = "100%";
              # The btrfs filesystem is declared here (on vdb, processed second).
              # By this point, vda has already been partitioned and
              # disk-vda-root exists in /dev/disk/by-partlabel/.
              content = {
                type = "btrfs";
                extraArgs = [
                  "-L"
                  "nixos-root"
                  "-d"
                  "raid1"
                  "-m"
                  "raid1"
                  "-f"
                  "/dev/disk/by-partlabel/disk-vda-root"
                ];
                subvolumes = {
                  "@" = {
                    mountpoint = "/";
                    mountOptions = [
                      "compress=zstd"
                      "noatime"
                    ];
                  };
                };
              };
            };
          };
        };
      };
    };
  };
}
