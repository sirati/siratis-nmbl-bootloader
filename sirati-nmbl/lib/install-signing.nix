# NMBL efi-stub UKI install + NVRAM registration + install-time UKI
# Secure-Boot signing (carved out of lib/install-bootloader.nix to keep that
# file under 400 lines and to give the secure/staged-boot work a single seam
# for UKI signing).
#
# Returns the `lib.optionalString (...) ''...''` shell-script fragment spliced
# back into the installer script for `actualLoader == "efi-stub"`.
#
# F5 / R-9: install-time UKI Secure-Boot signing lives here. The pure
# `nmblUki` derivation (lib/signing-build.nix) stays UNSIGNED; when
# `boot.nmbl.signing.uki.enable` is set we `sbsign` the PE at INSTALL time
# using the operator's `db`-enrolled key/cert, then copy the SIGNED PE onto
# the ESP. The private `keyFile` is read IMPURELY from its on-disk path by
# the install shell and is NEVER imported into the Nix store (closure-leak
# assert below). When signing is disabled the unsigned PE is installed
# exactly as before (no behaviour change).
#
# NOTE (cross-ref): this is the INSTALL-TIME db-enrollment CHECK only. The
# RUNTIME PCR-7 / Secure-Boot-state read at the start of the measured path
# (R-9 / FIX-11 "NMBL reads PCR-7 and warns if SB not enforcing") is F4c's
# measured-path Rust work (#26) and is intentionally NOT done here.

{
  lib,
  pkgs,
  config,
  bootstrapper,
  actualLoader,
  actualLoaderExtraArgs,
  nmblUki,
  # Install-time UKI Secure-Boot signing policy (config.boot.nmbl.signing.uki).
  # Defaults keep the module evaluable / behaviour unchanged if a caller does
  # not thread it (e.g. older host flake).
  ukiSigning ? {
    enable = false;
    keyFile = null;
    certFile = null;
    refuseInstallIfNotEnforcing = false;
  },
  # Install-time per-generation ML-DSA signing policy
  # (config.boot.nmbl.signing.{enable,generationKeyFile,sigPathSuffix}). The
  # host-platform `nmbl-sign` signer (threaded from the flake) signs each
  # bootable generation's kernel + initrd so the in-initramfs verify guard has
  # sidecars to check. Defaults keep this a no-op for non-secure-boot configs.
  genSigning ? {
    enable = false;
    keyFile = null;
    sigPathSuffix = ".sig";
  },
  # The host-platform `nmbl-sign` derivation (flake `_module.args.nmblSign`).
  # `null` on an older host flake; only dereferenced when generation signing is
  # enabled (the eval-time assert below requires it then).
  nmblSign ? null,
  # BUILD-TIME-ONLY: when true the install SKIPS every signing step (UNSIGNED
  # UKI installed, NO generation sidecars written) so a sealed image builder
  # that cannot read the impure keys still completes. Runtime enforcement is
  # untouched; the boot partition must be signed out of band afterwards. See
  # `boot.nmbl.signing.deferInstallSigning`.
  deferInstallSigning ? false,
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

  # ---- install-time UKI Secure-Boot signing (R-9 / #52) --------------------
  ukiSignEnable = ukiSigning.enable or false;
  ukiKeyFile = ukiSigning.keyFile or null;
  ukiCertFile = ukiSigning.certFile or null;
  ukiRefuseIfNotEnforcing = ukiSigning.refuseInstallIfNotEnforcing or false;

  # CLOSURE-LEAK ASSERT (CRITICAL). The Secure-Boot PRIVATE key must never
  # enter the Nix store / the system closure — it is read at INSTALL runtime
  # from its path, never embedded in a derivation. `lib.types.path` happily
  # accepts a Nix *path literal* (e.g. `./db.key`), which Nix imports into the
  # store at eval; a *string* path (e.g. "/run/secrets/db.key") stays out of
  # the store. We therefore `toString` the key/cert (interpolating the bare
  # filesystem path, NOT a store import) and FAIL the eval if either resolves
  # under `builtins.storeDir`. This mirrors the `publicKeys` posture (trust
  # material never written to a writable-boot artifact / never store-leaked).
  storeDir = builtins.storeDir;
  keyFileStr = if ukiKeyFile == null then null else toString ukiKeyFile;
  certFileStr = if ukiCertFile == null then null else toString ukiCertFile;
  keyIsStorePath = keyFileStr != null && lib.hasPrefix storeDir keyFileStr;
  certIsStorePath = certFileStr != null && lib.hasPrefix storeDir certFileStr;

  # Eval-time guards. Only meaningful when signing is enabled. `assertMsg`
  # throws (aborts eval) on a violation, so a misconfigured store-path key
  # can never produce an install script.
  closureLeakChecked =
    assert lib.assertMsg (!(ukiSignEnable && keyIsStorePath)) ''
      boot.nmbl.signing.uki.keyFile resolves to a Nix store path:
        ${toString keyFileStr}
      The Secure-Boot PRIVATE key must NEVER enter the store / system closure.
      Pass it as a STRING path to an on-disk secret read at install time, e.g.
        boot.nmbl.signing.uki.keyFile = "/run/secrets/nmbl-db.key";
      not a Nix path literal like ./db.key (which Nix imports into the store).
    '';
    assert lib.assertMsg (!(ukiSignEnable && certIsStorePath)) ''
      boot.nmbl.signing.uki.certFile resolves to a Nix store path:
        ${toString certFileStr}
      The Secure-Boot signing certificate is read at install time from its
      on-disk path; pass it as a STRING path (e.g. "/run/secrets/nmbl-db.crt"),
      not a Nix path literal that imports it into the store.
    '';
    true;

  # The on-ESP destination of the UKI PE (signed or unsigned). All shell
  # refs go through this so the signed/unsigned branch only changes WHAT is
  # copied, not WHERE.
  ukiEspDest = "/boot/${efiStubInstallPath}";

  # Escaped install-time-impure key/cert paths for the sbsign invocation.
  # These never appear in a derivation — only as literal arguments to the
  # imperative install command.
  keyArg = lib.escapeShellArg (toString keyFileStr);
  certArg = lib.escapeShellArg (toString certFileStr);

  # sbsign + db-enrollment install-check (#52 / FIX-11). Emitted only on the
  # efi-stub path with `signing.uki.enable`; force `closureLeakChecked` so the
  # eval-time guards run whenever this fragment is built.
  ukiSignShell =
    assert closureLeakChecked;
    ''
      echo "Signing NMBL UKI for Secure Boot (sbsign, install-time)..."

      # ---- db-enrollment install-check (FIX-11) ----------------------------
      # Detect whether the running firmware would actually REFUSE an unsigned
      # UKI: Secure Boot enabled AND enforcing AND our cert enrolled in `db`.
      # This is INSTALL-TIME advisory only; the RUNTIME PCR-7/SB-state read is
      # F4c's measured-path work (#26). Degrade gracefully (warn, never block)
      # when a detection tool is absent.
      sb_enforcing="unknown"
      if [ -x ${pkgs.systemd}/bin/bootctl ]; then
        # bootctl status prints e.g. "Secure Boot: enabled (user)" /
        # "disabled" / "enabled (setup)". setup-mode does NOT enforce.
        sb_line=$(${pkgs.systemd}/bin/bootctl status 2>/dev/null \
          | ${pkgs.gnugrep}/bin/grep -i "Secure Boot:" || true)
        case "$sb_line" in
          *enabled*setup*|*Setup*) sb_enforcing="no" ;;
          *enabled*)               sb_enforcing="yes" ;;
          *disabled*)              sb_enforcing="no" ;;
        esac
      fi
      if [ "$sb_enforcing" = "unknown" ] && [ -x ${pkgs.mokutil}/bin/mokutil ]; then
        # mokutil --sb-state prints "SecureBoot enabled" / "SecureBoot disabled".
        mok_line=$(${pkgs.mokutil}/bin/mokutil --sb-state 2>/dev/null || true)
        case "$mok_line" in
          *enabled*)  sb_enforcing="yes" ;;
          *disabled*) sb_enforcing="no" ;;
        esac
      fi
      if [ "$sb_enforcing" = "unknown" ] && [ -r /sys/firmware/efi/efivars ]; then
        # efivarfs fallback: SecureBoot-<guid> byte[4] == 1 ⇒ enabled.
        sb_var=$(echo /sys/firmware/efi/efivars/SecureBoot-* 2>/dev/null)
        if [ -r "$sb_var" ]; then
          sb_byte=$(${pkgs.coreutils}/bin/od -An -tu1 -j4 -N1 "$sb_var" 2>/dev/null \
            | ${pkgs.coreutils}/bin/tr -d ' ' || true)
          case "$sb_byte" in
            1) sb_enforcing="yes" ;;
            0) sb_enforcing="no" ;;
          esac
        fi
      fi

      # Is OUR cert enrolled in db? Best-effort positive detection.
      # The `db` efivar is a concatenation of EFI_SIGNATURE_LISTs, each
      # embedding the raw DER X.509 of an enrolled cert. Converting our cert
      # to DER and testing whether those exact bytes appear inside the db blob
      # is a reliable POSITIVE signal of enrollment (no fragile field parsing).
      # We hex-encode both (single text lines) so the substring test is plain
      # ASCII, sidestepping NUL/newline issues of a binary grep. Any missing
      # tool / unreadable db ⇒ "unknown" (warn, never block).
      cert_enrolled="unknown"
      db_var=$(echo /sys/firmware/efi/efivars/db-* 2>/dev/null)
      if [ -r "$db_var" ] && [ -x ${pkgs.openssl}/bin/openssl ]; then
        cert_hex=$(${pkgs.openssl}/bin/openssl x509 -in ${certArg} -outform DER 2>/dev/null \
          | ${pkgs.coreutils}/bin/od -An -v -tx1 | ${pkgs.coreutils}/bin/tr -d ' \n' || true)
        # Strip the 4-byte efivar attribute header off db, then hex-encode.
        db_hex=$(${pkgs.coreutils}/bin/dd if="$db_var" bs=1 skip=4 2>/dev/null \
          | ${pkgs.coreutils}/bin/od -An -v -tx1 | ${pkgs.coreutils}/bin/tr -d ' \n' || true)
        if [ -n "$cert_hex" ] && [ -n "$db_hex" ]; then
          case "$db_hex" in
            *"$cert_hex"*) cert_enrolled="yes" ;;
            *)             cert_enrolled="no" ;;
          esac
        fi
      fi

      # Would the running firmware actually REFUSE an unsigned UKI? Only if SB
      # is enforcing AND our cert is enrolled in db. Anything else (off, setup
      # mode, cert not in db, undetectable) means the chain is not yet
      # enforceable for THIS machine — warn loudly (or refuse, per policy).
      enforceable="no"
      if [ "$sb_enforcing" = "yes" ] && [ "$cert_enrolled" = "yes" ]; then
        enforceable="yes"
      fi

      if [ "$enforceable" = "yes" ]; then
        echo "✓ Secure Boot is enforcing and the UKI cert is enrolled in db."
      else
        echo "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!" >&2
        echo "WARNING: this firmware would NOT refuse an unsigned NMBL UKI."  >&2
        echo "         Secure Boot enforcing: $sb_enforcing; cert in db: $cert_enrolled." >&2
        echo "         The signed UKI is installed, but the firmware->NMBL"   >&2
        echo "         trust chain is NOT yet enforceable on this machine."   >&2
        echo "         Enroll ${certArg} into db out-of-band"    >&2
        echo "         and enable/enforce Secure Boot to close the chain."    >&2
        echo "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!" >&2
        ${lib.optionalString ukiRefuseIfNotEnforcing ''
          echo "refuseInstallIfNotEnforcing = true; aborting install." >&2
          exit 1
        ''}
      fi

      # ---- sbsign the UKI PE (reads keyFile/certFile IMPURELY) -------------
      uki_signed=$(mktemp)
      ${pkgs.sbsigntool}/bin/sbsign \
        --key ${keyArg} \
        --cert ${certArg} \
        --output "$uki_signed" \
        ${nmblUki}
      echo "✓ NMBL UKI signed; verifying signature against the cert..."
      ${pkgs.sbsigntool}/bin/sbverify --cert ${certArg} "$uki_signed" \
        || { echo "ERROR: sbverify of the signed UKI failed." >&2; rm -f "$uki_signed"; exit 1; }

      mkdir -p /boot/${efiStubDir}
      cp -f "$uki_signed" ${ukiEspDest}
      rm -f "$uki_signed"
      echo "✓ Signed NMBL UKI installed at ${ukiEspDest}"
    '';

  # Unsigned install (default / unchanged behaviour).
  ukiUnsignedShell = ''
    mkdir -p /boot/${efiStubDir}
    cp -f ${nmblUki} ${ukiEspDest}
    echo "✓ NMBL UKI installed at ${ukiEspDest}"
  '';

  # ---- install-time per-generation signing (#53 — boot-guard sidecars) ------
  # Sign EVERY bootable NixOS generation's kernel + initrd at install time so
  # NMBL's in-initramfs verify guard (#18/#20) has sidecars to check; without
  # them an ENFORCING install would refuse every generation. Factored into its
  # own module (loader-INDEPENDENT, and to keep this file under the size cap).
  # Returns "" when `signing.enable` is unset.
  # Deferred installs sign nothing in-place (the keys are unreadable in the
  # sealed builder); pass a disabled policy so install-gen-signing.nix emits the
  # empty fragment AND its impure-key asserts never fire for this build.
  effectiveGenSigning = if deferInstallSigning then genSigning // { enable = false; } else genSigning;
  genSignShell = import ./install-gen-signing.nix {
    inherit lib config nmblSign;
    genSigning = effectiveGenSigning;
  };

  # The efi-stub UKI install/sign fragment (loader-specific; UEFI direct boot
  # only). Bound here so the module can also emit the loader-INDEPENDENT
  # generation-signing fragment alongside it.
  ukiInstallShell = lib.optionalString (bootstrapper.bootMode == "uefi" && actualLoader == "efi-stub") ''
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
    #
    # When boot.nmbl.signing.uki.enable is set the PE is sbsign'd at install
    # time with the operator's db-enrolled key (R-9); otherwise the pure
    # unsigned PE is installed unchanged.
    echo "Installing NMBL UKI (UEFI efi-stub mode) to ${ukiEspDest}..."
    ${
      # Deferred build: install the UNSIGNED PE here; the host-side step
      # sbsigns it in place afterwards (the impure SB key is unreadable in a
      # sealed image builder).
      if ukiSignEnable && !deferInstallSigning then ukiSignShell else ukiUnsignedShell
    }

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
  '';
in

# Generation signing runs for EVERY loader (the sidecars live on the writable
# boot partition the runtime reads regardless of how NMBL itself is booted),
# so it is emitted independently of the efi-stub-only UKI fragment.
# `genSignShell` is already "" when signing is disabled (see the imported
# install-gen-signing.nix).
genSignShell + ukiInstallShell
