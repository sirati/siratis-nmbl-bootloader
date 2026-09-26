# Hetzner-shape stateful host: BIOS/GRUB NMBL, a vfat /boot (ESP) holding
# the NMBL kernel/initrd, external config and rescue image, and a Btrfs root
# with the normal writable Nix store and system profiles. Stateful boot
# tracking rolls back to a known-good generation; `boot.nmbl.rescue.automatic`
# decides what happens once no known-good generation is left.
#
# `variant` makes three distinct toplevels (system-1/2/3) from one module.
# `automatic` is the only knob the VM test flips between its two scenarios.
{
  nixpkgs,
  nmblModule,
  system ? "x86_64-linux",
  variant ? 1,
  automatic ? true,
  sshPublicKey ? "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFixtureOnlyNotARealKeyxxxxxxxxxxxxxxxxxxx fixture",
}:

nixpkgs.lib.nixosSystem {
  inherit system;
  modules = [
    nmblModule
    ({ config, lib, pkgs, ... }: {
      boot.nmbl = {
        enable = true;
        bootstrapper = {
          partition_table = "gpt";
          bootMode = "bios";
          loader = "grub";
          loader_extra_args = {
            timeout = 0;
            extraConfig = ''
              serial --unit=0 --speed=115200
              terminal_input serial
              terminal_output serial
            '';
          };
        };
        configLocation = "external";
        bootstrap = {
          configPath = "/nmbl/config.toml";
          bootFs = {
            device = "/dev/disk/by-partlabel/disk-main-ESP";
            fstype = "vfat";
            options = "ro";
            mountpoint = "/mnt/boot";
          };
          kernelModules.explicit = [ "vfat" "nls_cp437" "nls_iso8859_1" "virtio_pci" "virtio_blk" ];
        };
        stateful = {
          enable = true;
          maxRecoveryAttempts = 2;
          successTarget = "multi-user.target";
          stateDir = "/boot/nmbl";
          rwMountpoint = "/mnt/boot-state";
        };
        rescue = {
          mode = "external";
          inherit automatic;
          fullSystem = {
            enable = true;
            minimal = true;
            sshdPort = 22222;
            rootAuthorizedKeys = [ sshPublicKey ];
          };
        };
        kernelPackage = pkgs.linuxPackages_latest.kernel;
        kernelModules = [ "crc32c_generic" "libcrc32c" ];
        kernelParams = [ "console=ttyS0,115200" ];
        serialConsole = "ttyS0,115200";
        timeoutMillis = 300;
        emergencyTimeoutSecs = 600;
        refuseInvalidHardwareOnInstall = false;
        ignoreMissingDiskModules = true;
        paths.shell = "/bin/sh";
      };

      boot.kernelParams = [ "console=ttyS0,115200" ];
      boot.initrd.kernelModules = [ "virtio_pci" "virtio_blk" "btrfs" ];
      boot.initrd.availableKernelModules = [ "crc32c" ];
      boot.supportedFilesystems = [ "btrfs" "vfat" ];
      boot.loader.grub.devices = [ "/dev/vda" ];
      fileSystems = {
        "/" = {
          device = "/dev/disk/by-partlabel/disk-main-root";
          fsType = "btrfs";
          options = [ "subvol=@root" ];
        };
        "/nix" = {
          device = "/dev/disk/by-partlabel/disk-main-root";
          fsType = "btrfs";
          neededForBoot = true;
          options = [ "subvol=@nix" ];
        };
        "/boot" = {
          device = "/dev/disk/by-partlabel/disk-main-ESP";
          fsType = "vfat";
          options = [ "umask=0077" ];
        };
      };

      # Deterministic outcome per boot, chosen by a marker on /boot:
      # `fail-N` makes generation N power off before multi-user.target (so
      # nmbl-boot-succeeded never runs); otherwise the boot succeeds.
      systemd.services.nmbl-stateful-step = {
        wantedBy = [ "sysinit.target" ];
        after = [ "local-fs.target" ];
        before = [ "nmbl-boot-succeeded.service" ];
        unitConfig.DefaultDependencies = false;
        serviceConfig = {
          Type = "oneshot";
          StandardOutput = "tty";
          TTYPath = "/dev/ttyS0";
        };
        script = ''
          if [ -e /boot/nmbl-test/fail-${toString variant} ]; then
            echo NMBL_STATEFUL_GEN${toString variant}_FAILING
            ${config.systemd.package}/bin/systemctl poweroff --no-block
            exit 0
          fi
          echo NMBL_STATEFUL_GEN${toString variant}_BOOTED
        '';
      };
      systemd.services.nmbl-stateful-done = {
        wantedBy = [ "nmbl-boot-succeeded.service" ];
        after = [ "nmbl-boot-succeeded.service" ];
        serviceConfig = {
          Type = "oneshot";
          StandardOutput = "tty";
          TTYPath = "/dev/ttyS0";
        };
        script = ''
          echo NMBL_STATEFUL_GEN${toString variant}_SUCCEEDED
          ${config.systemd.package}/bin/systemctl poweroff --no-block
        '';
      };
      environment.etc."nmbl-stateful-variant".text = toString variant;
      networking.useDHCP = false;
      networking.dhcpcd.enable = false;
      nix.enable = false;
      users.users.root.initialHashedPassword = "";
      system.stateVersion = "26.05";
    })
  ];
}
