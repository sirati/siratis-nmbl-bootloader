# NMBL pre-kexec log import module
#
# NMBL stashes its boot transcript at /nmbl-log/nmbl.log and splices it
# into the kexec'd kernel's initramfs (see boot.rs stage_log_for_kexec /
# the cpio fragment). This module replays that transcript into the booted
# system's journal tagged `nmbl-init` so operators can read what NMBL did
# before the handover.
#
# The transcript carries kernel and device strings, so it is read by the
# compiled `nmbl-log-import` (nmbl-host-tools), never by a shell: it bounds
# the size, escapes control bytes and invalid UTF-8, caps each line, and
# sends every line over journald's native socket (falling back to
# /dev/kmsg with the same `nmbl-init` prefix).
#
# It supports BOTH NixOS initrd styles:
#
#   * systemd initrd  — a stage-1 oneshot imports the file into the
#     initrd journal, which is carried into the booted system.
#
#   * scripted initrd — the initramfs has no journald, so a
#     postMountCommands hook copies the file across the switch-root
#     boundary onto the booted root, and a stage-2 oneshot imports it
#     once journald is up.
#
# Either way the file is removed after a successful import so no pre-boot
# artifact lingers.

{ config, lib, pkgs, nmblSign ? null, ... }:

let
  initrdSystemd = config.boot.initrd.systemd.enable;

  # Where the scripted-initrd hook stashes the transcript inside the
  # booted root so it survives switch-root; the stage-2 unit reads it back
  # and deletes it. Must live on the persistent root — `/run` is a fresh
  # tmpfs in stage 2, so anything dropped under /mnt-root/run pre-pivot is
  # wiped before the unit could read it.
  stage2Src = "/var/lib/nmbl/nmbl-log.txt";

  # Just the importer binary, so the initrd does not carry the signer. It
  # has no RUNPATH: ld.so finds libc and libgcc_s in its own built-in glibc
  # directory, but the initrd builder copies only libraries a RUNPATH names.
  # That only worked while the system's glibc was the same derivation. When
  # nmbl-host-tools comes from another nixpkgs (a consumer re-locking
  # nmbl-init-rs), the initrd got that glibc's ld.so without its libc.so.6
  # and the importer exited 127 before main. Name the interpreter's own
  # directory, and refuse a binary with a library no RUNPATH entry holds.
  logImport = pkgs.runCommand "nmbl-log-import" {
    nativeBuildInputs = [ pkgs.patchelf ];
    meta.mainProgram = "nmbl-log-import";
  } ''
    bin=$out/bin/nmbl-log-import
    install -Dm755 ${nmblSign}/bin/nmbl-log-import $bin
    glibc=$(dirname "$(patchelf --print-interpreter $bin)")
    patchelf --add-rpath "$glibc:${lib.getLib pkgs.stdenv.cc.cc}/lib" $bin
    rpath=$(patchelf --print-rpath $bin)
    for needed in $(patchelf --print-needed $bin); do
      found=
      for dir in ''${rpath//:/ }; do
        if [ -e "$dir/$needed" ]; then found=1; fi
      done
      if [ -z "$found" ]; then
        echo "nmbl-log-import: $needed is not in RUNPATH $rpath" >&2
        exit 1
      fi
    done
  '';
  importBin = "${logImport}/bin/nmbl-log-import";
in
{
  config = lib.mkIf config.boot.nmbl.enable (lib.mkMerge [
    {
      assertions = [ {
        assertion = nmblSign != null;
        message = "boot.nmbl: the NMBL log import needs nmbl-host-tools (_module.args.nmblSign).";
      } ];
    }

    # --- systemd initrd: import straight from the initramfs. ---
    (lib.mkIf initrdSystemd {
      boot.initrd.systemd.storePaths = [ importBin ];
      boot.initrd.systemd.services.nmbl-log-import = {
        description = "Import NMBL pre-kexec log into the booted journal";
        wantedBy = [ "initrd.target" ];
        wants = [ "systemd-journald.socket" ];
        after = [ "cryptsetup.target" "systemd-journald.socket" ];
        before = [ "initrd-switch-root.target" "sysroot.mount" ];
        unitConfig = {
          DefaultDependencies = false;
          ConditionPathExists = "/nmbl-log/nmbl.log";
        };
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
          # The importer reports its own errors and exits 0; `-` also keeps a
          # loader failure from failing the boot and its NMBL blessing.
          ExecStart = "-${importBin} /nmbl-log/nmbl.log";
        };
      };
    })

    # --- scripted initrd: carry the file across switch-root, then import
    # in stage 2 once journald exists. ---
    (lib.mkIf (!initrdSystemd) {
      # Runs after the root fs is mounted at /mnt-root, before switch-root,
      # while the initramfs /nmbl-log/nmbl.log is still reachable. The file
      # is only copied here, never read.
      boot.initrd.postMountCommands = ''
        if [ -f /nmbl-log/nmbl.log ]; then
          mkdir -p /mnt-root${builtins.dirOf stage2Src}
          cp /nmbl-log/nmbl.log /mnt-root${stage2Src}
        fi
      '';

      systemd.services.nmbl-log-import = {
        description = "Import NMBL pre-kexec log into the booted journal";
        wantedBy = [ "multi-user.target" ];
        after = [ "systemd-journald.service" ];
        unitConfig.ConditionPathExists = stage2Src;
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
          ExecStart = "-${importBin} ${stage2Src}";
        };
      };
    })
  ]);
}
