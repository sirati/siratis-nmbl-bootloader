{ ... }:
# Two-disk RAID1 layout (mdadm). Mirrors both /boot and / across vda + vdb.
#
# /boot is on metadata=1.0 so the mdadm superblock lives at the *end* of the
# member partition; firmware reading the partition sees plain FAT and can load
# the bootstrapper directly without needing mdraid support. Userspace mounts
# the /dev/md array and writes are mirrored on each disk in sync.
#
# / is on metadata=1.2 (default — superblock at start) because it's only ever
# touched after NMBL has assembled the array.
#
# NMBL prerequisites (these must be wired up in sirati-nmbl before this config
# will actually boot):
#   1. boot.initrd.availableKernelModules = [ "md_mod" "raid1" ];
#      (or kernelModules, so NMBL loads them before assembly).
#   2. boot.initrd.kernelModules pulls them eagerly via the existing NMBL path.
#   3. NMBL's mount-and-kernel stage must run `mdadm --assemble --scan` before
#      it tries to mount /dev/disk/by-partlabel/* — that requires `pkgs.mdadm`
#      in the initrd and a `mdadm.conf` (or `--scan` with no-conf, which works
#      for arrays with metadata=1.x).
#   4. For BIOS mode, grub-pc must be installed to BOTH /dev/vda and /dev/vdb
#      so either disk can boot the box; that's outside disko's scope.
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
              # Type EF00 keeps the partition tagged as an EFI System
              # Partition so UEFI firmware will scan it, and BIOS+grub-pc
              # treats it as a regular partition either way.
              type = "EF00";
              content = {
                type = "mdraid";
                name = "boot";
              };
            };
            root = {
              priority = 3;
              size = "100%";
              content = {
                type = "mdraid";
                name = "root";
              };
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
            ESP = {
              priority = 2;
              size = "512M";
              type = "EF00";
              content = {
                type = "mdraid";
                name = "boot";
              };
            };
            root = {
              priority = 3;
              size = "100%";
              content = {
                type = "mdraid";
                name = "root";
              };
            };
          };
        };
      };
    };

    mdadm = {
      boot = {
        type = "mdadm";
        level = 1;
        # metadata=1.0 → superblock at END of member, FAT signature at the
        # start, so firmware can read /boot directly without understanding md.
        metadata = "1.0";
        # Disable internal bitmap: the bitmap feature (Feature Map 0x1) causes mdadm
        # --assemble --scan to look at the wrong superblock position for v1.0 arrays
        # (the bitmap+BBL push the superblock back from the expected offset).
        # Without bitmap, the superblock stays at the canonical end-of-device offset.
        extraArgs = [ "--bitmap=none" ];
        content = {
          type = "filesystem";
          format = "vfat";
          mountpoint = "/boot";
          mountOptions = [ "umask=0077" ];
        };
      };
      root = {
        type = "mdadm";
        level = 1;
        # metadata=1.2 (default) — only touched after NMBL has assembled it.
        # Disable internal bitmap: same reason as above, and also because kernel 6.18
        # may have issues with the bitmap superblock format from mdadm 4.4.
        extraArgs = [ "--bitmap=none" ];
        content = {
          type = "filesystem";
          format = "ext4";
          mountpoint = "/";
        };
      };
    };
  };
}
