# Two-disk btrfs RAID1. Lifted from nixos-anywhere-test/disko-btrfs-raid1.nix;
# see that file for the detailed processing-order reasoning.
{ ... }:
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
              # No content declared here. This partition is absorbed
              # as a btrfs member by the mkfs.btrfs invocation on
              # vdb's root partition (declared below). Disko processes
              # vda before vdb, so disk-vda-root will exist in
              # /dev/disk/by-partlabel/ by the time vdb's btrfs mkfs runs.
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
            esp_placeholder = {
              priority = 2;
              size = "512M";
              type = "8300";
            };
            root = {
              priority = 3;
              size = "100%";
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
