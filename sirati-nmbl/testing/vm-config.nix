# VM configuration builder for NMBL testing
# Creates NixOS system configurations for different boot modes

{
  self,
  nixpkgs,
  system ? "x86_64-linux",
}:

let
  pkgs = nixpkgs.legacyPackages.${system};

  # Create a test VM configuration
  mkTestVM =
    {
      name,
      bootMode,
      diskLayout ? {
        boot = {
          size = "200M";
          fsType = "vfat";
        };
        root = {
          size = "5G";
          fsType = "ext4";
        };
      },
    }:
    let
      # Base NixOS system configuration
      nixosSystem = nixpkgs.lib.nixosSystem {
        inherit system;
        modules = [
          self.nixosModules.default
          {
            # Use NMBL bootloader
            boot.nmbl = {
              enable = true;
              inherit bootMode;
              kernelPackage = pkgs.linux_6_6;

              kernelModules = [
                "ext4"
                "vfat"
                "virtio_blk"
                "virtio_pci"
                "virtio_net"
                "ata_piix"
                "ahci"
                "sd_mod"
                "crc32c"
                "crc32c_generic"
                "crc32c_intel"
              ];

              mountPrefix = "/mnt";
              kernelParams = [
                "console=ttyS0,115200"
                "earlyprintk=serial,ttyS0,115200"
              ];
              timeoutSeconds = 5;
              serialConsole = "ttyS0,115200";
            };

            # System configuration
            boot.kernelParams = [
              "console=ttyS0,115200"
              "earlyprintk=serial,ttyS0,115200"
              "loglevel=7"
            ];

            boot.loader.grub.enable = false;
            boot.loader.systemd-boot.enable = false;

            fileSystems."/" = {
              device = "/dev/vda1";
              fsType = diskLayout.root.fsType;
            };

            environment.defaultPackages = [ ];
            environment.systemPackages = with pkgs; [
              vim
              htop
            ];

            services.openssh.enable = true;
            services.openssh.settings.PermitRootLogin = "yes";
            users.users.root.password = "test";
            services.getty.autologinUser = "root";

            networking.hostName = name;
            networking.useDHCP = true;

            system.stateVersion = "24.05";
          }
        ];
      };

      # Build VM disk image with the full NixOS system installed
      vmDiskImage = import "${nixpkgs}/nixos/lib/make-disk-image.nix" {
        inherit pkgs;
        lib = nixpkgs.lib;
        config = nixosSystem.config;

        diskSize = "auto";
        format = "qcow2";
        name = "${name}-disk-image";

        # Partition layout for VirtIO disk (/dev/vda)
        partitionTableType = if bootMode == "mbr" then "legacy" else "hybrid";

        # Install the bootloader and system
        installBootLoader = true;
      };

    in
    nixosSystem
    // {
      # Add the VM disk image and all test artifacts to build outputs
      config = nixosSystem.config // {
        system = nixosSystem.config.system // {
          build = nixosSystem.config.system.build // {
            vmDiskImage = vmDiskImage;

            # Convenience package with all test artifacts in one place
            testArtifacts = pkgs.runCommand "${name}-test-artifacts" { } ''
              mkdir -p $out/bin

              # Link kernel and initrd
              ln -s ${nixosSystem.config.system.build.nmblKernel}/bzImage $out/kernel
              ln -s ${nixosSystem.config.system.build.nmblInitramfs}/initrd $out/initrd

              # Link VM disk image
              ln -s ${vmDiskImage}/nixos.qcow2 $out/disk.qcow2

              # Expose vm-serial-man binary location for convenience
              # Note: vm-serial-man is passed separately in flake.nix
              # This just documents where to find it
              echo "${name} test artifacts:" > $out/README.txt
              echo "  kernel: $out/kernel" >> $out/README.txt
              echo "  initrd: $out/initrd" >> $out/README.txt
              echo "  disk: $out/disk.qcow2" >> $out/README.txt
            '';
          };
        };
      };
    };

in
{
  inherit mkTestVM;
}
