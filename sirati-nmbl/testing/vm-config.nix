# VM configuration builder for NMBL testing
# Creates NixOS system configurations for different boot modes

{
  self,
  nixpkgs,
  disko ? null,
  system ? "x86_64-linux",
}:

let
  pkgs = nixpkgs.legacyPackages.${system};
  lib = nixpkgs.lib;

  # Create a test VM configuration
  mkTestVM =
    {
      name,
      bootstrapper,
      # Optional extra NixOS modules layered onto the base test config.
      # Used by disko-backed variants to override fileSystems and inject
      # boot.nmbl.activation.luks entries.
      extraModules ? [ ],
      # When set to a disko config module, build the disk image via
      # disko.diskoImages instead of make-disk-image.nix. The disko module
      # is also added to extraModules automatically.
      diskoModule ? null,
      # Kernel used by NMBL itself (the bootloader, not the post-kexec
      # system). LUKS configs benefit from a newer kernel because the
      # 6.6 series has dm-crypt → trusted-keys → encrypted-keys → ecb(aes)
      # init-order issues that newer kernels handle differently.
      nmblKernelPackage ? pkgs.linux_6_6,
    }:
    let
      # Base NixOS system configuration
      nixosSystem = nixpkgs.lib.nixosSystem {
        inherit system;
        modules = [
          self.nixosModules.default
          # Use NixOS QEMU guest profile for automatic VirtIO module detection
          "${nixpkgs}/nixos/modules/profiles/qemu-guest.nix"
        ] ++ lib.optional (diskoModule != null) disko.nixosModules.disko
          ++ lib.optional (diskoModule != null) diskoModule
          ++ extraModules
          ++ [
          {
            # Use NMBL bootloader
            boot.nmbl = {
              enable = true;
              inherit bootstrapper;
              kernelPackage = nmblKernelPackage;

              # availableKernelModules defaults to ["crc32c"]
              # kernelModules can be added if needed for explicit loading
              # Don't manually specify modules - they are inherited from
              # boot.initrd.kernelModules automatically
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

            # VirtIO drivers must be explicitly loaded for NMBL bootloader
            # NMBL doesn't have udev, so storage drivers from availableKernelModules won't auto-load
            # qemu-guest.nix adds these to availableKernelModules, but we need them in kernelModules
            # so NMBL loads them before trying to mount /dev/vda* devices
            # virtio_pci is required for PCI bus, virtio_blk is the actual block device driver
            boot.initrd.kernelModules = [
              "virtio_pci"
              "virtio_blk"
            ];

            boot.loader.grub.enable = false;
            boot.loader.systemd-boot.enable = false;

            # Filesystem configuration
            # GPT+BIOS: vda1 = FAT32 boot, vda2 = BIOS boot partition, vda3 = ext4 root
            # GPT+UEFI: vda1 = FAT32 ESP, vda2 = BIOS boot partition, vda3 = ext4 root
            # When a diskoModule is supplied, disko owns the fileSystems
            # config and we leave it alone here.
            fileSystems = lib.mkIf (diskoModule == null) {
              "/boot" = {
                device = "/dev/vda1";
                fsType = "vfat";
              };
              "/" = {
                device = "/dev/vda3";
                fsType = "ext4";
              };
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

      # Build VM disk image. With make-disk-image.nix (default) we get a
      # qcow2 at $out/nixos.qcow2; with disko we get a raw image at
      # $out/main.raw that we wrap into the same on-disk layout so the
      # test runner can `cp $out/nixos.qcow2` either way.
      vmDiskImage =
        if diskoModule == null then
          import "${nixpkgs}/nixos/lib/make-disk-image.nix" {
            inherit pkgs;
            inherit lib;
            config = nixosSystem.config;

            diskSize = "auto";
            format = "qcow2";
            name = "${name}-disk-image";

            # Partition layout for VirtIO disk (/dev/vda)
            # Always use hybrid (GPT with BIOS compatibility)
            # This creates: FAT32 ESP + BIOS boot partition + ext4 root
            partitionTableType = "hybrid";

            # Size of the boot partition
            bootSize = "512M";

            # Install the bootloader and system
            installBootLoader = true;
          }
        else
          pkgs.runCommand "${name}-disk-image"
            {
              nativeBuildInputs = [ pkgs.qemu-utils ];
            }
            ''
              mkdir -p $out
              qemu-img convert -f raw -O qcow2 \
                ${nixosSystem.config.system.build.diskoImages}/main.raw \
                $out/nixos.qcow2
            '';

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
