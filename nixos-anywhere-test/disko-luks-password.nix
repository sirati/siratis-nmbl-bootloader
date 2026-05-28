# Disko layout for the LUKS-password NMBL test VM.
#
# Layout mirrors the plain test images (vda1=BIOS-boot, vda2=ESP, vda3=root),
# but vda3 is wrapped in a LUKS container unlocked by the fixed passphrase
# "test". The passphrase lives in a writeText store path so the image build
# is reproducible inside the Nix sandbox.
{ pkgs, ... }:
{
  disko.devices.disk.main = {
    device = "/dev/vda";
    type = "disk";
    imageSize = "4G";
    content = {
      type = "gpt";
      partitions = {
        boot = {
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
        luks = {
          priority = 3;
          size = "100%";
          content = {
            type = "luks";
            name = "cryptroot";
            passwordFile = "${pkgs.writeText "nmbl-test-luks-password" "test"}";
            settings.allowDiscards = true;
            content = {
              type = "filesystem";
              format = "ext4";
              mountpoint = "/";
            };
          };
        };
      };
    };
  };
}
