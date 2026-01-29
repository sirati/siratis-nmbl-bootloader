# Testing configurations for NMBL bootloader
# This file provides utilities to generate test VMs with various configurations
# Uses NixOS's native VM building capabilities

{ self, nixpkgs }:

let
  # Helper function to create a test VM configuration
  mkTestVM =
    {
      name,
      system ? "x86_64-linux",
      bootMode ? "mbr",
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
      extraModules ? [ ],
    }:
    nixpkgs.lib.nixosSystem {
      inherit system;
      modules = [
        self.nixosModules.default

        # Basic VM configuration
        {
          # Minimal system configuration
          boot.loader.grub.enable = false;
          boot.loader.systemd-boot.enable = false;

          # Use NMBL bootloader
          boot.nmbl = {
            enable = true;
            bootMode = bootMode;
            kernelPackage = nixpkgs.legacyPackages.${system}.linux_6_6;

            kernelModules = [
              "ext4"
              "vfat"
              "virtio_blk"
              "virtio_pci"
              "virtio_net"
              "ata_piix"
              "ahci"
              "sd_mod"
            ];

            fileSystems =
              if bootMode == "mbr" then
                {
                  "/mnt-boot" = {
                    device = "/dev/vda1";
                    fsType = diskLayout.boot.fsType;
                    options = [ "ro" ];
                  };
                  "/mnt-root" = {
                    device = "/dev/vda2";
                    fsType = diskLayout.root.fsType;
                    options = [ "ro" ];
                  };
                }
              else
                {
                  "/mnt-boot" = {
                    device = "/dev/vda1";
                    fsType = "vfat";
                    options = [ "ro" ];
                  };
                  "/mnt-root" = {
                    device = "/dev/vda2";
                    fsType = diskLayout.root.fsType;
                    options = [ "ro" ];
                  };
                };

            # Serial console for headless testing
            kernelParams = [
              "console=ttyS0,115200"
              "earlyprintk=serial,ttyS0,115200"
            ];

            timeoutSeconds = 5;
            serialConsole = "ttyS0,115200";
          };

          # Actual system configuration with serial console
          boot.kernelParams = [
            "console=ttyS0,115200"
            "earlyprintk=serial,ttyS0,115200"
          ];

          # Minimal filesystem configuration for the actual NixOS system
          fileSystems."/" = {
            device = "/dev/vda2";
            fsType = diskLayout.root.fsType;
          };

          fileSystems."/boot" = {
            device = "/dev/vda1";
            fsType = diskLayout.boot.fsType;
          };

          # Remove default packages to keep VM minimal
          environment.defaultPackages = [ ];

          # Add only essential packages
          environment.systemPackages = with nixpkgs.legacyPackages.${system}; [
            vim
            htop
          ];

          # Enable SSH for remote access
          services.openssh = {
            enable = true;
            settings.PermitRootLogin = "yes";
          };

          # Set root password for testing (insecure, only for testing!)
          users.users.root.password = "test";

          # Networking
          networking.hostName = name;
          networking.useDHCP = true;

          system.stateVersion = "24.05";

          # VM-specific configuration for easy testing
          virtualisation.vmVariant = {
            # VM settings
            virtualisation = {
              memorySize = 1024; # 1GB RAM
              cores = 4;

              # Disk configuration
              diskSize = 10240; # 10GB

              # Use serial console
              graphics = false;

              # QEMU options
              qemu = {
                options = [
                  "-nographic"
                  "-serial mon:stdio"
                ];
                networkingOptions = [
                  "-netdev user,id=net0,hostfwd=tcp::${
                    toString (
                      2222
                      + (
                        if bootMode == "mbr" then
                          0
                        else if bootMode == "gpt-bios" then
                          1
                        else
                          2
                      )
                    )
                  }-:22"
                  "-device virtio-net-pci,netdev=net0"
                ];
              };
            };
          };
        }
      ]
      ++ extraModules;
    };

in
{
  # Function to generate test configurations
  # Returns an attrset suitable for nixosConfigurations
  mkTestConfigurations = {
    # MBR test VM with FAT32 boot and ext4 root
    test-mbr-serial = mkTestVM {
      name = "test-mbr-serial";
      bootMode = "mbr";
      diskLayout = {
        boot = {
          size = "200M";
          fsType = "vfat";
        };
        root = {
          size = "5G";
          fsType = "ext4";
        };
      };
    };

    # GPT-BIOS test VM
    test-gpt-bios = mkTestVM {
      name = "test-gpt-bios";
      bootMode = "gpt-bios";
      diskLayout = {
        boot = {
          size = "200M";
          fsType = "ext4";
        };
        root = {
          size = "5G";
          fsType = "ext4";
        };
      };
    };

    # GPT-UEFI test VM
    test-gpt-uefi = mkTestVM {
      name = "test-gpt-uefi";
      bootMode = "gpt-uefi";
      diskLayout = {
        boot = {
          size = "512M";
          fsType = "vfat";
        };
        root = {
          size = "5G";
          fsType = "ext4";
        };
      };
    };
  };
}
