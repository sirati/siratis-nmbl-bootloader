{
  nixpkgs,
  nmblModule,
  publicKey,
  system ? "x86_64-linux",
  rootStore ? false,
  variant ? 1,
  signingAlgorithm ? "ml-dsa-65",
}:

nixpkgs.lib.nixosSystem {
  inherit system;
  modules = [
    nmblModule
    "${nixpkgs}/nixos/modules/profiles/qemu-guest.nix"
    ({ config, lib, pkgs, ... }:
      let
        generationStoreMount = if rootStore then "/" else "/persistent";
        generationStateRoot = if rootStore then "/nmbl-generations" else "/persistent/nmbl-generations";
        stateTest = pkgs.writeShellScript "nmbl-state-vm-step" ''
          set -eu
          exec > /dev/ttyS0 2>&1
          trap 'systemctl --failed --no-pager; systemctl status nmbl-generation-success.service boot-complete.target systemd-boot-check-no-failures.service --no-pager || true; echo NMBL_STATE_STEP_FAILED' ERR
          root=${generationStateRoot}
          step=$(cat /boot/nmbl-test-step 2>/dev/null || echo 0)
          first=$(cat /boot/nmbl-test-first)
          second=$(cat /boot/nmbl-test-second)
          third=$(cat /boot/nmbl-test-third)
          pointer() { basename "$(readlink "$root/$1")"; }
          wait_active() {
            for _ in $(seq 1 600); do
              if systemctl is-active -q "$1"; then return; fi
              sleep .2
            done
            return 1
          }
          wait_failed() {
            for _ in $(seq 1 600); do
              if systemctl is-failed -q "$1"; then return; fi
              sleep .2
            done
            return 1
          }
          assert_nix_var_flags() {
            options=$(${pkgs.util-linux}/bin/findmnt -n -o OPTIONS /nix/var)
            for option in ro noexec nosuid nodev; do
              printf '%s\n' "$options" | ${pkgs.gnugrep}/bin/grep -Eq "(^|,)$option(,|$)"
            done
          }
          finish() {
            echo "$1" > /dev/ttyS0
            echo "$2" > /boot/nmbl-test-step
            sync
            ${config.systemd.package}/bin/systemctl poweroff
          }
          case "$step" in
            0)
              wait_active nmbl-generation-success.service
              test "$(pointer tested)" = "$first"
              test ! -e "$root/attempted"
              ${pkgs.util-linux}/bin/findmnt -n -t erofs /nix
              assert_nix_var_flags
              ${config.system.build.nmblErofsCtl}/bin/nmbl-erofsctl activate "$second" "$root"
              finish NMBL_FIRST_BLESSED 1
              ;;
            1)
              wait_active nmbl-generation-success.service
              test "$(pointer tested)" = "$second"
              test ! -e "$root/pending"
              ${config.system.build.nmblErofsCtl}/bin/nmbl-erofsctl activate "$third" "$root"
              touch /boot/nmbl-force-degraded
              finish NMBL_SECOND_BLESSED 2
              ;;
            2)
              wait_failed systemd-boot-check-no-failures.service
              systemctl is-failed -q nmbl-test-degraded.service
              test "$(pointer attempted)" = "$third"
              test "$(pointer pending)" = "$third"
              test "$(pointer tested)" = "$second"
              rm /boot/nmbl-force-degraded
              finish NMBL_PENDING_FAILED 3
              ;;
            3)
              wait_active nmbl-generation-success.service
              grep -qw nmbl.rollback-after-untested-new-generation-failed /proc/cmdline
              systemctl is-active -q nmbl-rollback-after-untested-new-generation-failed.target
              test "$(pointer active)" = "$second"
              test ! -e "$root/rollback-event"
              test ! -e "$root/attempted"
              touch /boot/nmbl-force-degraded
              finish NMBL_ROLLBACK_BLESSED 4
              ;;
            4)
              wait_failed systemd-boot-check-no-failures.service
              systemctl is-failed -q nmbl-test-degraded.service
              test "$(pointer attempted)" = "$second"
              test ! -e "$root/pending"
              rm /boot/nmbl-force-degraded
              finish NMBL_TESTED_FAILED 5
              ;;
            *) exit 1 ;;
          esac
        '';
      in
      {
      boot.nmbl = {
        enable = true;
        configLocation = "external";
        bootstrapper.bootMode = "qemu_kernel_invoke";
        bootstrap.configPath = "/nmbl-generations/active/config.toml";
        bootstrap.bootFs = {
          device = if rootStore then "/dev/vdb" else "/dev/vdc";
          fstype = "ext4";
          options = "rw,nosuid,nodev,noexec";
          mountpoint = "/mnt/boot";
        };
        bootstrap.kernelModules.explicit = [ "virtio_pci" "virtio_blk" "ext4" ];
        generationImage = {
          enable = true;
          automaticRollback = true;
          automaticRescue = true;
          successDelaySec = 2;
          stateRoot = generationStateRoot;
          signaturePath = "${generationStateRoot}/active/nix.erofs.sig";
          stage1Store = {
            targetMountPoint = generationStoreMount;
            runtimeMountPoint = "/mnt/nmbl-generation-store";
          };
        };
        signing = {
          enable = true;
          enforce = true;
          algorithm = signingAlgorithm;
          publicKeys = [ publicKey ];
          generationKeyFile = "/run/operator-only/private.key";
          imageKeyFile = "/run/operator-only/private.key";
          deferInstallSigning = true;
        };
        secureBoot = {
          enable = false;
          enforce = false;
          requireTpm = false;
        };
        tpm = { measure = false; requireTpm = false; };
        rescue.mode = "external";
        timeoutMillis = 250 + variant;
        kernelPackage = pkgs.linuxPackages_latest.kernel;
        kernelParams = [ "console=ttyS0,115200" ];
        serialConsole = "ttyS0,115200";
      };

      boot.initrd.systemd.enable = true;
      boot.initrd.availableKernelModules = [ "virtio_pci" "virtio_blk" "ext4" "loop" "erofs" ];
      fileSystems = {
        "/" = {
          device = "/dev/disk/by-label/NMBLROOT";
          fsType = "ext4";
          neededForBoot = rootStore;
        };
        "/boot" = {
          device = "/dev/disk/by-label/NMBLBOOT";
          fsType = "ext4";
          neededForBoot = true;
          options = [ "nosuid" "nodev" "noexec" ];
        };
        "/persistent" = lib.mkIf (!rootStore) {
          device = "/dev/disk/by-label/NMBLSTORE";
          fsType = "ext4";
          neededForBoot = true;
          options = [ "nosuid" "nodev" "noexec" ];
        };
        "/nix" = {
          device = "${generationStateRoot}/active/nix.erofs";
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

      systemd.services.nmbl-test-degraded = {
        description = "Deterministic degraded-boot acceptance-test trigger";
        wantedBy = [ "multi-user.target" ];
        unitConfig.ConditionPathExists = "/boot/nmbl-force-degraded";
        serviceConfig = { Type = "oneshot"; ExecStart = "/bin/false"; };
      };
      systemd.services.nmbl-test-ready = {
        wantedBy = [ "multi-user.target" ];
        after = [ "getty@ttyS0.service" ];
        serviceConfig = {
          Type = "oneshot";
          ExecStart = "${pkgs.coreutils}/bin/echo NMBL_TARGET_READY";
          StandardOutput = "tty";
          TTYPath = "/dev/ttyS0";
        };
      };

      systemd.services.nmbl-state-vm-step = {
        serviceConfig = { Type = "oneshot"; ExecStart = stateTest; };
      };
      systemd.timers.nmbl-state-vm-step = {
        wantedBy = [ "timers.target" ];
        timerConfig = {
          OnBootSec = "8s";
          AccuracySec = "100ms";
          Unit = "nmbl-state-vm-step.service";
        };
      };

      services.getty.autologinUser = "root";
      users.users.root.initialHashedPassword = "";
      environment.systemPackages = [
        config.system.build.nmblErofsCtl
        pkgs.util-linux
        (pkgs.writeTextFile {
          name = "nmbl-generation-variant-${toString variant}";
          destination = "/share/nmbl-generation-variant";
          text = toString variant;
        })
      ];
      nix.enable = false;
      boot.loader.grub.enable = false;
      boot.loader.systemd-boot.enable = false;
      system.stateVersion = "26.05";
      })
  ];
}
