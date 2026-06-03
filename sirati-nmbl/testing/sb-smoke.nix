# Secure-Boot enforcement smoke test (#55 / R-10): the DEMONSTRABLY-ENFORCING
# unsigned-UKI harness that is the literal precondition for #29 closing.
#
# It builds a GPT disk whose EFI System Partition carries an UNSIGNED NMBL UKI
# at the firmware-default path `/EFI/BOOT/BOOTX64.EFI`, then exposes a runner
# that boots it under a Secure-Boot-ENFORCING OVMFFull (db = Microsoft KEK/db,
# `smm=on`). Because the UKI is unsigned and not covered by `db`, an enforcing
# firmware REFUSES to launch it — it never reaches NMBL at all. The companion
# assertion script (testing/assertions/sb-unsigned-uki.sh) proves exactly that:
# a Secure-Boot violation / no NMBL output. This is what distinguishes "firmware
# refused" (what we want here) from "NMBL refused" (a different test).
#
# BUILD-ONLY here: this module produces the disk image + the runner; the actual
# VM run is #57's job.

{
  nixpkgs,
  system ? "x86_64-linux",
  testRunners,
  vmSerialMan,
  # A NixOS config whose `system.build.nmblUki` is the unsigned EFI-stub PE.
  config,
}:

let
  pkgs = nixpkgs.legacyPackages.${system};

  # The unsigned NMBL UKI (ukify output): kernel+initrd in one EFI-stub PE. The
  # efi-stub install path drops this at EFI/BOOT/BOOTX64.EFI; we do the same on a
  # throwaway ESP, but DELIBERATELY DO NOT SIGN it.
  unsignedUki = config.config.system.build.nmblUki;

  # A GPT disk with a single FAT32 ESP holding the unsigned UKI at the
  # firmware-default boot path. mtools writes the FAT image without root; sgdisk
  # wraps it in a GPT with an EF00 (ESP) partition so OVMF's boot manager finds
  # `\EFI\BOOT\BOOTX64.EFI`.
  unsignedUkiDisk =
    pkgs.runCommand "sb-unsigned-uki-disk.qcow2"
      {
        nativeBuildInputs = [
          pkgs.mtools
          pkgs.gptfdisk
          pkgs.qemu
          pkgs.dosfstools
        ];
      }
      ''
        set -euo pipefail

        # Size the FAT to comfortably hold the UKI (~50 MiB) + slack.
        uki_bytes=$(stat -c %s ${unsignedUki})
        esp_mib=$(( (uki_bytes / 1048576) + 32 ))

        # Build the FAT32 ESP and drop the UNSIGNED UKI at the default path.
        truncate -s "''${esp_mib}M" esp.img
        mkfs.vfat -F 32 -n NMBLESP esp.img
        mmd -i esp.img ::/EFI ::/EFI/BOOT
        mcopy -i esp.img ${unsignedUki} ::/EFI/BOOT/BOOTX64.EFI

        # Wrap the ESP in a GPT (1 MiB alignment gap before the partition).
        gap=1
        total_mib=$(( esp_mib + gap + 1 ))
        truncate -s "''${total_mib}M" disk.raw
        dd if=esp.img of=disk.raw bs=1M seek="$gap" conv=notrunc
        sgdisk \
          --new=1:''${gap}MiB:+''${esp_mib}MiB \
          --typecode=1:EF00 \
          --change-name=1:"EFI System Partition" \
          disk.raw

        mkdir -p "$out"
        qemu-img convert -f raw -O qcow2 disk.raw "$out/nixos.qcow2"
      '';

  # A config-shaped shim so mkRunner's `config.config.system.build.*` accessors
  # resolve to our throwaway disk. mkRunner takes `config` and reads
  # `config.config.system.build.*`, so the shim needs exactly ONE `.config`
  # under it (mkRunner supplies the outer one by naming the parameter `config`).
  diskShim = {
    config.boot.nmbl.bootstrapper.bootMode = "uefi";
    config.system.build.vmDiskImage = unsignedUkiDisk;
    config.system.build.testArtifacts = unsignedUki;
    config.system.build.nmblKernel = null;
    config.system.build.nmblInitramfs = null;
  };

  # The Secure-Boot-ENFORCING runner: UEFI boot of the unsigned-UKI ESP under
  # OVMFFull with smm=on and the db-enrolled VARS. Unsigned ⇒ firmware refuses.
  runner = testRunners.mkRunner {
    name = "sb-unsigned-uki";
    config = diskShim;
    inherit vmSerialMan;
    bootMode = "gpt-uefi";
    secureBoot = true;
  };
in
{
  inherit unsignedUki unsignedUkiDisk runner;
}
