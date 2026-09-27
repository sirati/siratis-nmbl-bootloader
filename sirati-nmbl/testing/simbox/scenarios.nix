# Scenarios for nmbl-simbox: the REAL production NMBL initramfs (nmbl-init as
# /init, embedded config.toml, modules, busybox) built from a NixOS config,
# plus the disk trees the simulator presents as block devices.
#
#   normal: /dev/vda2 (ext4, PARTLABEL disk-main-root) holds the root with
#           three system profiles; NMBL boots generation 3.
#   luks:   /dev/vda3 is LUKS (PARTLABEL disk-main-luks, test passphrase
#           "simbox-test-passphrase"); /dev/mapper/cryptroot holds the root,
#           and the typed passphrase is handed to stage 1 (passToStage1).
{ nixpkgs, nmblModule, system ? "x86_64-linux" }:

let
  pkgs = nixpkgs.legacyPackages.${system};
  lib = nixpkgs.lib;

  mkNmbl = { luks, splash ? false }: (lib.nixosSystem {
    inherit system;
    modules = [
      nmblModule
      ({ ... }: {
        boot.nmbl = {
          enable = true;
          bootstrapper = { partition_table = "gpt"; bootMode = "qemu_kernel_invoke"; };
          kernelPackage = pkgs.linuxPackages_latest.kernel;
          serialConsole = "ttyS0,115200";
          timeoutMillis = if splash then 20000 else 1500;
          splash.enable = splash;
          deviceTimeoutSeconds = 5;
          ignoreMissingDiskModules = true;
          refuseInvalidHardwareOnInstall = false;
          activation.luks = lib.optionals luks [
            { name = "cryptroot"; device = "/dev/disk/by-partlabel/disk-main-luks"; unlock = "password"; }
          ];
        };
        fileSystems."/" =
          if luks then { device = "/dev/mapper/cryptroot"; fsType = "ext4"; }
          else { device = "/dev/disk/by-partlabel/disk-main-root"; fsType = "ext4"; };
        # NMBL marks /boot needed-for-boot; give it a device (a vfat ESP).
        fileSystems."/boot" = { device = "/dev/disk/by-partlabel/disk-main-ESP"; fsType = "vfat"; };
        boot.loader.grub.enable = false;
        system.stateVersion = "26.05";
      })
    ];
  }).config.system.build;

  # A fake NixOS root: three system-N-link profiles, each with kernel,
  # initrd, init, kernel-params and nixos-version. The kernel/initrd are
  # small marker files — nothing boots them; the simulator only records them.
  fakeRoot = pkgs.runCommand "simbox-fake-root" { } ''
    mkdir -p $out/nix/var/nix/profiles $out/nix/store
    for n in 1 2 3; do
      g=$out/nix/store/00000000000000000000000000000000-nixos-system-simbox-gen$n
      mkdir -p $g
      printf 'SIMBOX-KERNEL-GEN%s\n' $n > $g/kernel
      # A gzip "system initrd" so the NMBL cpio fragment follows opaque bytes.
      printf 'SIMBOX-SYSTEM-INITRD-GEN%s\n' $n | ${pkgs.gzip}/bin/gzip -n > $g/initrd
      printf '#!/bin/sh\n' > $g/init; chmod +x $g/init
      printf 'console=ttyS0 loglevel=4 simbox.gen=%s' $n > $g/kernel-params
      printf '26.05.2026092%s (Simbox)' $n > $g/nixos-version
      ln -s /nix/store/$(basename $g) $out/nix/var/nix/profiles/system-$n-link
    done
    ln -s system-3-link $out/nix/var/nix/profiles/system
  '';

  # Unpack the real NMBL initramfs into a tree for the container root.
  initramfsTree = build: pkgs.runCommand "simbox-initramfs-tree" {
    nativeBuildInputs = [ pkgs.cpio pkgs.gzip ];
  } ''
    mkdir -p $out; cd $out
    gzip -dc ${build.nmblInitramfs}/initrd | cpio -idm --quiet
    # makeInitrd ships the store paths it references under ./nix/store and
    # points /init, /bin/sh, /bin/blkid at them with absolute symlinks, which
    # resolve inside the container root as-is. blkid is replaced by the
    # simbox stand-in at run time.
    chmod -R u+w .
  '';

  mkScenario = { name, luks, splash ? false }: let b = mkNmbl { inherit luks splash; }; in
    pkgs.runCommand "simbox-scenario-${name}" { } ''
      mkdir -p $out
      ln -s ${initramfsTree b} $out/initrd
      ln -s ${fakeRoot} $out/root
      mkdir -p $out/boot
      t=${initramfsTree b}; release=$(ls "$t$(readlink $t/lib/modules)")
      cat > $out/scenario.toml <<TOML
      name = "${name}"
      initramfs = "initrd"
      kernel_release = "$release"
      cmdline = "console=ttyS0,115200"
      [[block]]
      name = "vda1"
      major = 254
      minor = 1
      blkid = { TYPE = "vfat", PARTLABEL = "disk-main-ESP", UUID = "5A1B-0001" }
      tree = "boot"
      ${if luks then ''
      [[block]]
      name = "vda3"
      major = 254
      minor = 3
      blkid = { TYPE = "crypto_LUKS", PARTLABEL = "disk-main-luks", UUID = "5a1b0b0c-0000-4000-8000-00000000000c" }
      [[luks]]
      device = "/dev/vda3"
      name = "cryptroot"
      passphrase = "simbox-test-passphrase"
      mapper_tree = "root"
      [[keys]]
      after = "Enter passphrase"
      delay_ms = 300
      text = "simbox-test-passphrase\\r"
      '' else ''
      [[block]]
      name = "vda2"
      major = 254
      minor = 2
      blkid = { TYPE = "ext4", PARTLABEL = "disk-main-root", UUID = "5a1b0b0c-0000-4000-8000-000000000002" }
      tree = "root"
      ''}
      TOML
      sed -i 's/^      //' $out/scenario.toml
    '';
in {
  normal = mkScenario { name = "normal"; luks = false; };
  luks = mkScenario { name = "luks"; luks = true; };
  # The graphical splash (DRM framebuffer) build, for --graphical runs.
  splash = mkScenario { name = "splash"; luks = false; splash = true; };
}
