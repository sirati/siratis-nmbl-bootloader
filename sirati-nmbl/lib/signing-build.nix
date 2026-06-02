# UKI build wiring + /init binary selection, extracted out of
# lib/config.nix as a pure function.
#
# Returns two attrs consumed by lib/config.nix:
#   - selectedNmblInit : the resolved /init binary used by the initramfs
#                        builder and downstream tooling (identity-equal to
#                        the prebuilt `nmblInit` / `nmblInitSplash` in the
#                        single-feature cases so Nix's store-path dedup keeps
#                        the existing CI cache hot).
#   - nmblUki          : the NMBL UKI (Unified Kernel Image) derivation, the
#                        NMBL kernel + initrd spliced into ONE EFI-stub PE via
#                        systemd's `ukify`.
#
# Used as a pure function: lib/config.nix `import`s this file and applies it
# with `{ pkgs, lib, config, cfg, nmblInit, nmblInitSplash, mkNmblInit }`.
# This is a pure extraction — the publicKeys / signing logic itself lands in
# later phases (F2/F5); nothing here changes behaviour.

{
  pkgs,
  lib,
  config,
  cfg,
  nmblInit,
  nmblInitSplash,
  # Builder form supplied by flake.nix. When extra Cargo features are
  # requested (e.g. `network-rescue` when `boot.nmbl.rescue.network`
  # is enabled, combined with `image-splash` when both are on) we
  # re-build the binary with those features. Defaults to ignoring
  # features and returning the prebuilt `nmblInit` if the host flake
  # is older.
  mkNmblInit ? (_: nmblInit),
  ...
}:

let
  # Cargo features to enable in the /init binary. Gated on splash and
  # rescue options so feature-free builds (default) stay byte-identical
  # to today's binary. When only `image-splash` is requested we prefer
  # the prebuilt `nmblInitSplash` to keep the existing CI cache hot;
  # when only `network-rescue` is requested we use `mkNmblInit`. When
  # both are requested we build a combined binary via `mkNmblInit`.
  nmblFeatures =
    lib.optional cfg.splash.enable "image-splash"
    ++ lib.optional cfg.rescue.network "network-rescue"
    ++ lib.optional cfg.stateful.enable "stateful";

  # Resolved /init binary used by the initramfs builder. Identity-equal
  # to the prebuilt `nmblInit` / `nmblInitSplash` in the single-feature
  # cases so Nix's store-path dedup keeps the existing CI cache hot.
  selectedNmblInit =
    if nmblFeatures == [ ] then
      nmblInit
    else if nmblFeatures == [ "image-splash" ] then
      nmblInitSplash
    else if nmblFeatures == [ "stateful" ] then
      mkNmblInit { features = [ "stateful" ]; }
    else
      mkNmblInit { features = nmblFeatures; };

  # Build the NMBL UKI (Unified Kernel Image): the NMBL kernel + initrd
  # spliced into ONE EFI-stub PE via systemd's `ukify`. This is what the
  # `loader = "efi-stub"` install path drops at EFI/BOOT/BOOTX64.EFI so
  # the ESP holds ONLY NMBL (no GRUB/systemd-boot binary, no separate
  # kernel/initrd files — both live inside the PE's `.linux`/`.initrd`
  # sections, which systemd-stub hands to the kernel at boot).
  #
  # The cmdline matches `nmblBootConfig` (kernelParams + the optional
  # serial console=). x86_64 bzImage is already an EFI-stub-capable PE;
  # systemd-stub reliably passes the embedded `.initrd` section, so no
  # on-disk initrd is needed. Always evaluable (cheap when unreferenced);
  # only built when the efi-stub install path consumes it.
  nmblUki =
    let
      kernel = config.system.build.nmblKernel;
      initrd = config.system.build.nmblInitramfs;
      cmdline = lib.concatStringsSep " " (
        cfg.kernelParams ++ lib.optional (cfg.serialConsole != null) "console=${cfg.serialConsole}"
      );
    in
    pkgs.runCommand "nmbl-uki.efi"
      {
        nativeBuildInputs = [ pkgs.systemdUkify ];
      }
      ''
        # ukify defaults to reading /usr/lib/os-release for the .osrel
        # section, which does not exist in the Nix sandbox. Pass an
        # explicit minimal os-release so the build is hermetic.
        printf 'NAME=NMBL\nID=nmbl\nPRETTY_NAME="NMBL Bootloader"\n' > os-release
        ukify build \
          --linux=${kernel}/bzImage \
          --initrd=${initrd}/initrd \
          --cmdline=${lib.escapeShellArg cmdline} \
          --os-release=@os-release \
          --output=$out
      '';

in
{
  inherit selectedNmblInit nmblUki;
}
