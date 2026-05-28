# Single-disk btrfs layout. vda1 = BIOS-boot, vda2 = ESP/boot (vfat),
# vda3 = btrfs root with a single "@" subvolume mounted at /.
{ ... }:
{
  disko.devices.disk.main = {
    device = "/dev/vda";
    type = "disk";
    imageSize = "4G";
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
          content = {
            type = "btrfs";
            extraArgs = [
              "-L"
              "nixos-root"
              "-f"
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
}
