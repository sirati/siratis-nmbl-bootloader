# Evaluation fixture for the DNS-VPS topology: BIOS/GRUB NMBL, signed EROFS
# `/nix` generations on a stage-1 persistent store, automatic rollback and
# rescue, and a signed network stage with recovery SSH in the same generation
# directory. Signing keys come from key commands, never from files.
{
  nixpkgs,
  nmblModule,
  publicKey,
  system ? "x86_64-linux",
}:

nixpkgs.lib.nixosSystem {
  inherit system;
  modules = [
    nmblModule
    ({ ... }: {
      boot.nmbl = {
        enable = true;
        # loader_extra_args deliberately unset: bios/grub must evaluate with
        # the option defaults.
        bootstrapper = {
          partition_table = "gpt";
          bootMode = "bios";
          loader = "grub";
        };
        configLocation = "external";
        bootstrap = {
          configPath = "/nmbl-generations/active/config.toml";
          bootFs = {
            device = "/dev/disk/by-partlabel/disk-main-persistent";
            fstype = "ext4";
            options = "ro,nosuid,nodev,noexec";
            mountpoint = "/mnt/boot";
          };
          kernelModules.explicit = [ "virtio_pci" "virtio_blk" "ext4" ];
        };
        signing = {
          enable = true;
          enforce = true;
          algorithm = "ml-dsa-87";
          publicKeys = [ publicKey ];
          generationKeyCommand = [ "nix-secrets" "pipe-secret" "nmbl-generation" ];
          imageKeyCommand = [ "nix-secrets" "pipe-secret" "nmbl-image" ];
          deferInstallSigning = true;
        };
        generationImage = {
          enable = true;
          stateRoot = "/persistent/nmbl-generations";
          signaturePath = "/persistent/nmbl-generations/active/nix.erofs.sig";
          automaticRollback = true;
          automaticRescue = true;
          stage1Store.targetMountPoint = "/persistent";
        };
        ignoreMissingDiskModules = true;
        rescue.mode = "external";
        rescue.fullSystem = {
          enable = true;
          sshdPort = 22222;
          rootAuthorizedKeys = [ "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFixtureOnlyNotARealKeyxxxxxxxxxxxxxxxxxxx fixture" ];
          hostKeyPath = "/mnt/boot/rescue-host-ed25519";
          networkStage = {
            enable = true;
            interfaces = [ "eth0" ];
          };
        };
      };
      boot.initrd.systemd.enable = true;
      boot.initrd.kernelModules = [ "virtio_pci" "virtio_blk" "ext4" "loop" "erofs" ];
      boot.loader.grub.devices = [ "/dev/vda" ];
      fileSystems = {
        "/" = { device = "/dev/vda3"; fsType = "ext4"; };
        "/boot" = { device = "/dev/vda2"; fsType = "vfat"; };
        "/persistent" = {
          device = "/dev/disk/by-partlabel/disk-main-persistent";
          fsType = "ext4";
          neededForBoot = true;
        };
        "/nix" = {
          device = "/persistent/nmbl-generations/active/nix.erofs";
          fsType = "erofs";
          neededForBoot = true;
          options = [ "loop" "ro" ];
        };
      };
      system.stateVersion = "26.05";
    })
  ];
}
