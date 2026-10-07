# Stage-1 log import on a production-shaped host: systemd initrd, stateful
# boot tracking with the servers' success target, no generation image, and
# nmbl-host-tools built by a different nixpkgs than the system. That last
# part is what consumers get when nmbl-init-rs is locked on its own (as the
# sirati-nixos fleet does): the importer's ld.so is not the system glibc, so
# the initrd only has its libc when the module names it. The transcript is
# put where NMBL's kexec fragment puts it.
{ pkgs, nmblModule, nmblSign, foreignGlibc }:
let
  inherit (pkgs) lib;
  # The same importer, loaded by another glibc derivation of the same release.
  foreignSign = pkgs.runCommand "nmbl-sign-foreign-glibc" {
    nativeBuildInputs = [ pkgs.patchelf ];
  } ''
    cp -r ${nmblSign} $out
    chmod -R u+w $out
    patchelf --set-interpreter ${foreignGlibc}/lib/ld-linux-x86-64.so.2 \
      $out/bin/nmbl-log-import
  '';
  transcript = ''
    nmbl-init starting
    phase 1: mount pseudo-filesystems
    ${lib.concatStrings (lib.genList (n: "phase 3b: waiting for /dev/disk/by-partlabel/disk-${toString n}\n") 600)}phase 4: scanning generations
    ${lib.fixedWidthString 2000 "w" ""}
    kexec: handing off to new kernel
  '';
in
assert lib.assertMsg (foreignGlibc.outPath != pkgs.glibc.outPath)
  "log-import-production: the foreign glibc must be another derivation than the system's";
pkgs.testers.runNixOSTest {
  name = "nmbl-log-import-production";
  nodes.machine = { config, ... }: {
    imports = [ nmblModule ];
    _module.args.nmblSign = lib.mkForce foreignSign;
    boot.initrd.systemd.enable = true;
    boot.nmbl = {
      enable = true;
      bootstrapper.bootMode = "qemu_kernel_invoke";
      stateful = {
        enable = true;
        maxRecoveryAttempts = 5;
        successTarget = "boot-complete.target";
      };
    };
    # NMBL hands the transcript over as a cpio fragment appended to the
    # initrd it kexecs, a regular root-only file in the initramfs.
    virtualisation.directBoot.initrd = "${pkgs.runCommand "initrd-with-nmbl-log" {
      nativeBuildInputs = [ pkgs.cpio ];
      inherit transcript;
      passAsFile = [ "transcript" ];
    } ''
      mkdir -p fragment/nmbl-log
      install -m 0400 "$transcriptPath" fragment/nmbl-log/nmbl.log
      cp ${config.system.build.initialRamdisk}/${config.system.boot.loader.initrdFile} $out
      chmod u+w $out
      truncate -s %4 $out
      (cd fragment && find nmbl-log | cpio -o -H newc -R +0:+0 --reproducible --quiet) >> $out
    ''}";
    system.stateVersion = "26.05";
  };

  testScript = ''
    machine.wait_for_unit("multi-user.target")
    unit = machine.succeed("journalctl -b -u nmbl-log-import.service -o cat --no-pager")
    assert "error while loading" not in unit, unit
    assert "status=" not in unit, unit
    native = machine.succeed(
        "journalctl -b -t nmbl-init _TRANSPORT=journal -o cat --no-pager"
    ).splitlines()
    assert native[0] == "nmbl-init starting", native[:3]
    assert native.count("phase 4: scanning generations") == 1, len(native)
    assert len(native) == 605, len(native)
    assert native[-1] == "kexec: handing off to new kernel", native[-2:]
    assert native[-2].startswith("w" * 2000), native[-2][:80]
  '';
}
