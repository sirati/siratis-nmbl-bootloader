{ nixpkgs, nmblModule, publicKey, system ? "x86_64-linux" }:

nixpkgs.lib.nixosSystem {
  inherit system;
  modules = [
    nmblModule
    "${nixpkgs}/nixos/modules/profiles/qemu-guest.nix"
    ({ pkgs, ... }: {
      users.users.nmbl-update = {
        isSystemUser = true;
        uid = 991;
        group = "nmbl-update";
      };
      users.groups.nmbl-update.gid = 991;
      boot.nmbl = {
        enable = true;
        configLocation = "external";
        bootstrapper = {
          partition_table = "gpt";
          bootMode = "uefi";
          loader = "grub";
          loader_extra_args.timeout = 0;
        };
        bootstrap.bootFs = {
          device = "/dev/vdb";
          fstype = "ext4";
          options = "ro,nosuid,nodev,noexec";
          mountpoint = "/mnt/boot";
        };
        bootstrap.kernelModules.explicit = [ "virtio_pci" "virtio_blk" "ext4" ];
        kernelPackage = pkgs.linuxPackages_latest.kernel;
        kernelParams = [ "console=ttyS0,115200" ];
        serialConsole = "ttyS0,115200";
        refuseInvalidHardwareOnInstall = false;
        signing = {
          enable = true;
          enforce = true;
          algorithm = "ml-dsa-65";
          publicKeys = [ publicKey ];
          generationKeyFile = "/run/operator/offline.key";
          deferInstallSigning = true;
        };
        rescue.mode = "external";
        bootUpdate = {
          enable = true;
          inherit publicKey;
        };
      };
      boot.initrd.kernelModules = [ "virtio_pci" "virtio_blk" ];
      fileSystems."/" = { device = "/dev/vdc"; fsType = "ext4"; };
      # The install-time GRUB module requires an ESP declaration. The VM's
      # NMBL bootstrap store is the separate ext4 vdb above; the target NixOS
      # system is never entered in this selector-path test.
      fileSystems."/boot" = { device = "/dev/vda"; fsType = "vfat"; };
      boot.loader.grub.enable = false;
      boot.loader.systemd-boot.enable = false;
      system.stateVersion = "24.05";
    })
  ];
}
