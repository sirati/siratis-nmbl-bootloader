# The DNS-VPS / Stardust topology: BIOS/GRUB NMBL, signed EROFS `/nix`
# generations on a persistent stage-1 store, automatic rollback and rescue, and
# a signed network stage with recovery SSH inside the generation directory.
# Signing keys come from key commands, never from files.
#
# Used twice: `nmbl-erofs-bios-host-eval` evaluates the production shape, and
# `test-erofs-bios-host-vm` boots it (`vmTest = true` adds the serial console,
# the passt-reachable static address and the state-machine step service).
# Disk layout mirrors the consumer's DNS VPS disko: GPT with a BIOS boot
# partition, a vfat `/boot` holding only GRUB + the NMBL kernel/initrd, an
# ext4 `/persistent` holding the generations, and a tmpfs `/`.
{
  nixpkgs,
  nmblModule,
  publicKey,
  system ? "x86_64-linux",
  sshPublicKey ? "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFixtureOnlyNotARealKeyxxxxxxxxxxxxxxxxxxx fixture",
  vmTest ? false,
  algorithm ? "ml-dsa-87",
  variant ? 1,
}:

nixpkgs.lib.nixosSystem {
  inherit system;
  modules = [
    nmblModule
    ({ config, lib, pkgs, ... }:
      let
        stateRoot = "/persistent/nmbl-generations";
        stepScript = pkgs.writeShellScript "nmbl-bios-host-step" ''
          set -eu
          exec > /dev/ttyS0 2>&1
          trap 'systemctl --failed --no-pager || true; echo NMBL_BIOS_STEP_FAILED' ERR
          root=${stateRoot}
          step=$(cat /persistent/nmbl-test/step 2>/dev/null || echo 0)
          pointer() { basename "$(readlink "$root/$1")"; }
          wait_unit() {
            for _ in $(seq 1 600); do
              if systemctl "$1" -q "$2"; then return; fi
              sleep .2
            done
            systemctl list-jobs --no-pager || true
            systemctl status "$2" boot-complete.target systemd-boot-check-no-failures.service --no-pager || true
            return 1
          }
          finish() {
            echo "$1"
            echo "$2" > /persistent/nmbl-test/step
            sync
            systemctl poweroff
          }
          case "$step" in
            0)
              wait_unit is-active nmbl-generation-success.service
              test "$(pointer tested)" = "$(cat /persistent/nmbl-test/first)"
              ${pkgs.util-linux}/bin/findmnt -n -t erofs /nix
              test "$(${pkgs.util-linux}/bin/findmnt -n -o FSTYPE /)" = tmpfs
              ${config.system.build.nmblErofsCtl}/bin/nmbl-erofsctl activate \
                "$(cat /persistent/nmbl-test/second)" "$root"
              touch /persistent/nmbl-test/degraded
              finish NMBL_BIOS_FIRST_BLESSED 1
              ;;
            1)
              wait_unit is-failed systemd-boot-check-no-failures.service
              test "$(pointer attempted)" = "$(cat /persistent/nmbl-test/second)"
              rm /persistent/nmbl-test/degraded
              finish NMBL_BIOS_PENDING_FAILED 2
              ;;
            2)
              wait_unit is-active nmbl-generation-success.service
              grep -qw nmbl.rollback-after-untested-new-generation-failed /proc/cmdline
              test "$(pointer active)" = "$(cat /persistent/nmbl-test/first)"
              touch /persistent/nmbl-test/degraded
              finish NMBL_BIOS_ROLLBACK_BLESSED 3
              ;;
            3)
              wait_unit is-failed systemd-boot-check-no-failures.service
              rm /persistent/nmbl-test/degraded
              finish NMBL_BIOS_TESTED_FAILED 4
              ;;
            *) exit 1 ;;
          esac
        '';
      in
      lib.mkMerge [ {
        boot.nmbl = {
          enable = true;
          # loader_extra_args carries only the serial console for the VM; the
          # production eval leaves it unset to cover the option defaults.
          bootstrapper = {
            partition_table = "gpt";
            bootMode = "bios";
            loader = "grub";
          } // lib.optionalAttrs vmTest {
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
            algorithm = algorithm;
            publicKeys = [ publicKey ];
            generationKeyCommand = [ "nix-secrets" "pipe-secret" "nmbl-generation" ];
            imageKeyCommand = [ "nix-secrets" "pipe-secret" "nmbl-image" ];
            deferInstallSigning = true;
          };
          generationImage = {
            enable = true;
            stateRoot = stateRoot;
            signaturePath = "${stateRoot}/active/nix.erofs.sig";
            automaticRollback = true;
            automaticRescue = true;
            successDelaySec = 2;
            stage1Store.targetMountPoint = "/persistent";
          };
          secureBoot = { enable = false; enforce = false; requireTpm = false; };
          tpm = { measure = false; requireTpm = false; };
          ignoreMissingDiskModules = true;
          rescue = {
            mode = "external";
            nicDrivers = [ "virtio_net" ];
            fullSystem = {
              enable = true;
              minimal = true;
              sshdPort = 22222;
              rootAuthorizedKeys = [ sshPublicKey ];
              hostKeyPath = "/mnt/boot/rescue-host-ed25519";
              networkStage = {
                enable = true;
                addressFamily = "ipv4-only";
                dnsServers = [ "10.0.2.3" ];
                staticProfiles = [ {
                  macAddress = "52:54:00:12:34:56";
                  ipv4 = {
                    addresses = [ "10.0.2.15/32" ];
                    gateway = "10.0.2.2";
                    gatewayOnLink = true;
                  };
                } ];
              };
            };
          };
        };
      }
      (lib.mkIf vmTest {
        boot.nmbl = {
          kernelPackage = pkgs.linuxPackages_latest.kernel;
          kernelParams = [ "console=ttyS0,115200" ];
          serialConsole = "ttyS0,115200";
          timeoutMillis = 250 + variant;
          refuseInvalidHardwareOnInstall = false;
        };
      })
      {
        boot.initrd.systemd.enable = true;
        boot.initrd.kernelModules = [ "virtio_pci" "virtio_blk" "ext4" "loop" "erofs" ];
        boot.loader.grub.devices = [ "/dev/vda" ];
        fileSystems = {
          "/" = {
            device = "none";
            fsType = "tmpfs";
            options = [ "mode=0755" "nodev" "nosuid" "size=512M" ];
          };
          "/boot" = {
            device = "/dev/disk/by-partlabel/disk-main-boot";
            fsType = "vfat";
            options = [ "nodev" "noexec" "nosuid" ];
          };
          "/persistent" = {
            device = "/dev/disk/by-partlabel/disk-main-persistent";
            fsType = "ext4";
            neededForBoot = true;
            options = [ "nodev" "noexec" "nosuid" ];
          };
          "/nix" = {
            device = "${stateRoot}/active/nix.erofs";
            fsType = "erofs";
            neededForBoot = true;
            options = [ "loop" "ro" ];
          };
          "/nix/var" = {
            device = "/nix/var";
            fsType = "none";
            neededForBoot = true;
            options = [ "bind" "ro" "noexec" "nosuid" "nodev" ];
          };
        };
        system.stateVersion = "26.05";
      }
      (lib.mkIf vmTest {
        boot.kernelParams = [ "console=ttyS0,115200" ];
        boot.initrd.availableKernelModules = [ "virtio_pci" "virtio_blk" "ext4" "loop" "erofs" ];
        networking.useDHCP = false;
        networking.dhcpcd.enable = false;
        systemd.services.nmbl-test-degraded = {
          wantedBy = [ "multi-user.target" ];
          unitConfig.ConditionPathExists = "/persistent/nmbl-test/degraded";
          serviceConfig = { Type = "oneshot"; ExecStart = "/bin/false"; };
        };
        systemd.services.nmbl-bios-host-step.serviceConfig = {
          Type = "oneshot";
          ExecStart = stepScript;
        };
        systemd.timers.nmbl-bios-host-step = {
          wantedBy = [ "timers.target" ];
          timerConfig = { OnBootSec = "8s"; Unit = "nmbl-bios-host-step.service"; };
        };
        environment.systemPackages = [
          config.system.build.nmblErofsCtl
          (pkgs.writeTextFile {
            name = "nmbl-bios-host-variant-${toString variant}";
            destination = "/share/nmbl-bios-host-variant";
            text = toString variant;
          })
        ];
        nix.enable = false;
        users.users.root.initialHashedPassword = "";
      }) ])
  ];
}
