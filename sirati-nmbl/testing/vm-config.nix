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
    }:
    let
      # Base NixOS system configuration
      nixosSystem = nixpkgs.lib.nixosSystem {
        inherit system;
        modules = [
          self.nixosModules.default
          # Use NixOS QEMU guest profile for automatic VirtIO module detection
          "${nixpkgs}/nixos/modules/profiles/qemu-guest.nix"
          {
            # Use NMBL bootloader
            boot.nmbl = {
              enable = true;
              inherit bootMode;
              kernelPackage = pkgs.linux_6_6;

              # Don't manually specify modules - they are inherited from
              # boot.initrd.availableKernelModules automatically based on
              # filesystem declarations
              kernelModules = [ ];

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

            # WORKAROUND: crc32c is required by ext4 but not automatically included by NixOS's ext.nix module
            # This is needed for the system's initrd (after kexec) to mount the root filesystem
            # See: https://github.com/NixOS/nixpkgs/blob/master/nixos/modules/tasks/filesystems/ext.nix
            # The module only adds "ext2" and "ext4" but ext4 depends on crc32c at runtime
            boot.initrd.availableKernelModules = [ "crc32c" ];

            boot.loader.grub.enable = false;
            boot.loader.systemd-boot.enable = false;

            # Filesystem configuration
            # MBR (legacy+boot): vda1 = FAT32 boot, vda2 = ext4 root
            # GPT (hybrid): vda1 = FAT32 ESP, vda2 = BIOS boot partition, vda3 = ext4 root
            fileSystems."/boot" = {
              device = "/dev/vda1";
              fsType = "vfat";
            };

            fileSystems."/" = {
              device = if bootMode == "mbr" then "/dev/vda2" else "/dev/vda3";
              fsType = "ext4";
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
        # legacy+boot: FAT32 boot partition + ext4 root partition (MBR)
        # hybrid: FAT32 ESP + ext4 root partition (GPT with BIOS compat)
        partitionTableType = if bootMode == "mbr" then "legacy+boot" else "hybrid";

        # Size of the boot partition
        bootSize = "512M";

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
