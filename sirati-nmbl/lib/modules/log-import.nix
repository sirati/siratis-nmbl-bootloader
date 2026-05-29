# NMBL pre-kexec log import module
#
# Adds a stage-1 oneshot to the BOOTED system's initramfs that drains the
# log file NMBL stashed at /nmbl-log/nmbl.log before the kexec handover
# into the booted journal. The file itself crosses the kexec boundary via
# the same passthrough mechanism that LUKS keyfile injection uses (wired
# up elsewhere); this module just trusts the file is there and imports it.
#
# Each line lands in the journal tagged `nmbl-init` at info priority. If
# systemd-cat is unavailable for any reason we fall back to /dev/kmsg so
# the lines still make it somewhere operators can read. After a successful
# import the file is unlinked and /nmbl-log is rmdir'd to leave no
# pre-boot artifacts in the booted system's root.

{ config, lib, pkgs, ... }:

let
  systemdBin = "${config.boot.initrd.systemd.package}/bin";

  # Inline stage-1 script. `read -r` preserves backslashes; the kmsg
  # fallback prefix matches the systemd-cat tag so log consumers see one
  # consistent identifier regardless of which path the line took.
  importScript = ''
    set -u
    src=/nmbl-log/nmbl.log
    if [ ! -f "$src" ]; then
      exit 0
    fi
    while IFS= read -r line || [ -n "$line" ]; do
      if ! printf '%s\n' "$line" | ${systemdBin}/systemd-cat -t nmbl-init -p info; then
        printf 'nmbl-init: %s\n' "$line" > /dev/kmsg || true
      fi
    done < "$src"
    rm -f "$src"
    rmdir /nmbl-log 2>/dev/null || true
  '';
in
{
  config = lib.mkIf config.boot.nmbl.enable {
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
      script = importScript;
    };
  };
}
