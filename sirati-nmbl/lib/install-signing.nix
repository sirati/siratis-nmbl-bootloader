# NMBL efi-stub UKI install + NVRAM registration (carved out of
# lib/install-bootloader.nix to keep that file under 400 lines and to give the
# secure/staged-boot work a single seam for UKI signing).
#
# Returns the `lib.optionalString (...) ''...''` shell-script fragment spliced
# back into the installer script for `actualLoader == "efi-stub"`. This is a
# pure extraction: the produced shell is byte-identical to the block that used
# to live inline in install-bootloader.nix.
#
# F5: generation+UKI signing here. This UKI install / NVRAM registration is the
# natural home for sbsign'ing the UKI PE (and signing generation/own-kernel
# sidecars) before it is copied onto the ESP; add it inside the fragment below.

{
  lib,
  pkgs,
  bootstrapper,
  actualLoader,
  actualLoaderExtraArgs,
  nmblUki,
}:

let
  # efi-stub install target. Defaults to the firmware removable/fallback
  # path (auto-booted, no NVRAM entry); an own path installs alongside
  # another bootloader (GRUB) and gets a NVRAM entry instead. Read with
  # `or` defaults so a null loader_extra_args (actualLoaderExtraArgs = {})
  # keeps the historical fallback-path behaviour.
  efiStubInstallPath = actualLoaderExtraArgs.efiStubInstallPath or "EFI/BOOT/BOOTX64.EFI";
  efiStubIsFallback = efiStubInstallPath == "EFI/BOOT/BOOTX64.EFI";
  efiStubCanTouchEfi = actualLoaderExtraArgs.canTouchEfiVariables or false;
  efiStubDir = builtins.dirOf efiStubInstallPath;
  # UEFI device-path form of the loader (backslash-separated, leading \).
  efiStubLoaderBackslash = "\\" + (lib.replaceStrings [ "/" ] [ "\\" ] efiStubInstallPath);
in

lib.optionalString (bootstrapper.bootMode == "uefi" && actualLoader == "efi-stub") ''
    # UEFI direct boot. The ESP holds a single NMBL UKI PE (kernel + initrd
    # embedded; systemd-stub passes the .initrd section to the kernel). No
    # separate nmbl-kernel/nmbl-initrd files (those copies are skipped above
    # in this mode).
    #
    # Install target = loader_extra_args.efiStubInstallPath:
    #   * default EFI/BOOT/BOOTX64.EFI — the firmware removable/fallback path,
    #     auto-booted with no NVRAM entry (dedicated NMBL disk or a manually
    #     uploaded image; this is what stardust/live-usb use).
    #   * an own path e.g. EFI/nmbl/nmbl.efi — installs ALONGSIDE another
    #     bootloader (GRUB) without touching its fallback binary, and a UEFI
    #     NVRAM entry "NMBL" (first in BootOrder) is registered so firmware
    #     boots it. GRUB's own NVRAM entry is left intact.
    echo "Installing NMBL UKI (UEFI efi-stub mode) to /boot/${efiStubInstallPath}..."
    mkdir -p /boot/${efiStubDir}
    cp -f ${nmblUki} /boot/${efiStubInstallPath}
    echo "✓ NMBL UKI installed at /boot/${efiStubInstallPath}"

    ${lib.optionalString (!efiStubIsFallback) (
      if efiStubCanTouchEfi then ''
        # Own (non-fallback) path: firmware won't auto-boot it, so register a
        # NVRAM boot entry. Derive the ESP disk + partition number from the
        # mounted /boot, drop any stale "NMBL" entries (idempotent re-install),
        # then create a fresh one — efibootmgr puts new entries first in
        # BootOrder, leaving GRUB's entry as the fallback choice.
        echo "Registering UEFI NVRAM boot entry for NMBL..."
        ESP_DEV=$(${pkgs.util-linux}/bin/findmnt -n -o SOURCE --target /boot)
        ESP_DISK=/dev/$(${pkgs.util-linux}/bin/lsblk -no PKNAME "$ESP_DEV")
        ESP_PART=$(cat /sys/class/block/$(basename "$ESP_DEV")/partition 2>/dev/null || echo "")
        if [ -b "$ESP_DISK" ] && [ -n "$ESP_PART" ]; then
          for n in $(${pkgs.efibootmgr}/bin/efibootmgr | ${pkgs.gnused}/bin/sed -nE 's/^Boot([0-9A-Fa-f]{4})\*? NMBL$/\1/p'); do
            ${pkgs.efibootmgr}/bin/efibootmgr -b "$n" -B || true
          done
          ${pkgs.efibootmgr}/bin/efibootmgr --create --disk "$ESP_DISK" --part "$ESP_PART" \
            --label NMBL --loader '${efiStubLoaderBackslash}' --unicode \
            || echo "WARNING: efibootmgr failed to create the NMBL boot entry; add it manually."
          echo "✓ NVRAM boot entry 'NMBL' -> ${efiStubLoaderBackslash} ($ESP_DISK part $ESP_PART)"
        else
          echo "WARNING: could not resolve ESP disk/partition from /boot (source: $ESP_DEV)."
          echo "         Add the NMBL boot entry manually:"
          echo "           efibootmgr -c -d <ESP-disk> -p <part#> -L NMBL -l '${efiStubLoaderBackslash}'"
        fi
      '' else ''
        # Own path but canTouchEfiVariables = false: NVRAM is left untouched.
        # The UKI exists but firmware will NOT auto-boot it (only the fallback
        # path is auto-booted). Add a UEFI boot entry by hand.
        echo "NOTE: NMBL UKI installed at an own path but canTouchEfiVariables = false."
        echo "      Firmware will NOT auto-boot it. Add a UEFI boot entry manually:"
        echo "        efibootmgr -c -d <ESP-disk> -p <ESP-part#> -L NMBL -l '${efiStubLoaderBackslash}'"
      ''
    )}
  ''
