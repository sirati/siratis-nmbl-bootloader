({
  nmblModule,
  publicKey,
  diskEnvironment,
  withVm ? true,
}:

{ config, lib, pkgs, ... }:

{
  imports = [ nmblModule ];

  boot.initrd.systemd.enable = true;
  boot.initrd.availableKernelModules = [ "virtio_blk" "ext4" "loop" "erofs" ];
  boot.nmbl = {
    enable = true;
    bootstrapper.bootMode = "qemu_kernel_invoke";
    generationImage = {
      enable = true;
      signaturePath = "/boot/nmbl-generations/active/nix.erofs.sig";
    };
    signing = {
      enable = true;
      enforce = true;
      algorithm = "ml-dsa-65";
      publicKeys = [ publicKey ];
      generationKeyFile = "/run/operator-only/private.key";
      deferInstallSigning = true;
    };
    secureBoot = {
      enable = true;
      enforce = true;
    };
  };

  fileSystems = {
    "/" = {
      device = "/dev/disk/by-label/NMBLROOT";
      fsType = "ext4";
    };
    "/boot" = {
      device = "/dev/disk/by-label/NMBLBOOT";
      fsType = "ext4";
      neededForBoot = true;
    };
    "/nix" = {
      device = "/boot/nmbl-generations/active/nix.erofs";
      fsType = "erofs";
      neededForBoot = true;
      options = [ "loop" "ro" ];
    };
  };

  environment.systemPackages = [ config.system.build.nmblErofsCtl pkgs.util-linux ];
  nix.enable = false;
  system.stateVersion = "26.05";

} // lib.optionalAttrs withVm {
  virtualisation = {
    memorySize = 1536;
    mountHostNixStore = false;
    writableStore = false;
    fileSystems = {
      "/boot" = {
        device = "/dev/disk/by-label/NMBLBOOT";
        fsType = "ext4";
        neededForBoot = true;
      };
      "/nix" = {
        device = "/boot/nmbl-generations/active/nix.erofs";
        fsType = "erofs";
        neededForBoot = true;
        options = [ "loop" "ro" ];
      };
    };
    qemu.drives = [
      {
        name = "nmbl-generation";
        file = "$" + diskEnvironment;
        driveExtraOpts.format = "raw";
        deviceExtraOpts.serial = "nmbl-generation";
      }
    ];
  };
})
