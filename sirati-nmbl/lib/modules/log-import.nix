# NMBL pre-kexec log import module
#
# NMBL stashes its boot transcript at /nmbl-log/nmbl.log and splices it
# into the kexec'd kernel's initramfs (see boot.rs stage_log_for_kexec /
# the cpio fragment). This module replays that transcript into the booted
# system's journal tagged `nmbl-init` so operators can read what NMBL did
# before the handover.
#
# It supports BOTH NixOS initrd styles:
#
#   * systemd initrd  — a stage-1 oneshot drains the file directly into
#     the journal via systemd-cat while still in the initramfs.
#
#   * scripted initrd — the initramfs has no journald, so a
#     postMountCommands hook copies the file across the switch-root
#     boundary into the booted root's /run, and a stage-2 oneshot then
#     replays it once journald is up.
#
# Either way the file is removed after a successful import so no pre-boot
# artifact lingers. Both paths fall back to /dev/kmsg if systemd-cat is
# unavailable, keeping the same `nmbl-init` prefix.

{ config, lib, pkgs, ... }:

let
  initrdSystemd = config.boot.initrd.systemd.enable;

  # Where the scripted-initrd hook stashes the transcript inside the
  # booted root so it survives switch-root; the stage-2 unit reads it back
  # and deletes it. Must live on the persistent root — `/run` is a fresh
  # tmpfs in stage 2, so anything dropped under /mnt-root/run pre-pivot is
  # wiped before the unit could read it.
  stage2Src = "/var/lib/nmbl/nmbl-log.txt";

  # Drain <src> into the journal, one line per entry, tagged `nmbl-init`.
  # `read -r` preserves backslashes; the kmsg fallback prefix matches the
  # systemd-cat tag so log consumers see one identifier either way.
  importScript = systemdCat: src: ''
    set -u
    src=${src}
    if [ ! -f "$src" ]; then
      exit 0
    fi
    while IFS= read -r line || [ -n "$line" ]; do
      if ! printf '%s\n' "$line" | ${systemdCat} -t nmbl-init -p info; then
        printf 'nmbl-init: %s\n' "$line" > /dev/kmsg || true
      fi
    done < "$src"
    rm -f "$src"
    rmdir "$(dirname "$src")" 2>/dev/null || true
  '';

  initrdSystemdBin = "${config.boot.initrd.systemd.package}/bin/systemd-cat";
  stage2SystemdCat = "${config.systemd.package}/bin/systemd-cat";
in
{
  config = lib.mkMerge [
    # --- systemd initrd: drain straight from the initramfs. ---
    (lib.mkIf initrdSystemd {
      boot.initrd.systemd.services.nmbl-log-import = {
        description = "Import NMBL pre-kexec log into the booted journal";
        wantedBy = [ "initrd.target" ];
        after = [ "cryptsetup.target" ];
        before = [ "initrd-switch-root.target" "sysroot.mount" ];
        unitConfig.DefaultDependencies = false;
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
        };
        script = importScript initrdSystemdBin "/nmbl-log/nmbl.log";
      };
    })

    # --- scripted initrd: carry the file across switch-root, then replay
    # in stage 2 once journald exists. ---
    (lib.mkIf (!initrdSystemd) {
      # Runs after the root fs is mounted at /mnt-root, before switch-root,
      # while the initramfs /nmbl-log/nmbl.log is still reachable.
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
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
        };
        script = importScript stage2SystemdCat stage2Src;
      };
    })
  ];
}
