# Two-disk RAID1 layout (mdadm). Mirrors both /boot and / across vda + vdb.
# Lifted from nixos-anywhere-test/disko-raid1.nix; see that file for the
# detailed metadata-1.0 / 1.2 reasoning.
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
        metadata = "1.0";
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
