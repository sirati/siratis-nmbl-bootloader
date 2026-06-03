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
  # Single source of the `secureBootActive` IMPLICATION boolean (FIX-16).
  # Imported here so the `nmblFeatures` derive and (later) the per-group
  # emit gates in lib/config-toml.nix share ONE definition. The contract
  # is "any security table enabled ⇒ `secure-boot` ∈ nmblFeatures".
  securityConsts = import ./security-consts.nix { inherit lib; };
  secureBootActive = securityConsts.mkSecureBootActive config;
  stagedBootActive = securityConsts.mkStagedBootActive config;

  # Cargo features to enable in the /init binary. Gated on splash and
  # rescue options so feature-free builds (default) stay byte-identical
  # to today's binary. When only `image-splash` is requested we prefer
  # the prebuilt `nmblInitSplash` to keep the existing CI cache hot;
  # when only `network-rescue` is requested we use `mkNmblInit`. When
  # both are requested we build a combined binary via `mkNmblInit`.
  nmblFeatures =
    lib.optional cfg.splash.enable "image-splash"
    ++ lib.optional cfg.rescue.network "network-rescue"
    ++ lib.optional cfg.stateful.enable "stateful"
    # IMPLICATION (FIX-16): enabling any security table pulls the
    # `secure-boot` feature into the built /init. `secureBootActive` is
    # `false` in the skeleton (the options don't exist yet), so the
    # default build is unchanged.
    ++ lib.optional secureBootActive "secure-boot"
    # Staged boot (#9): `boot.nmbl.staged.enable` pulls the `staged-boot`
    # Cargo feature (which implies `secure-boot`). The SAME boolean gates
    # the `[staged]`/`[bootstrap.staged]` emit in the toml helpers, so the
    # built binary always carries the cfg that parses what Nix emits
    # (FIX-40). `false` in the skeleton, so the default build is unchanged.
    ++ lib.optional stagedBootActive "staged-boot";

  # Baked trust-anchor keys threaded into the /init binary (R-5/FIX-17).
  # Each `boot.nmbl.signing.publicKeys` path is paired with the single
  # configured `algorithm` (all baked keys share the algorithm; the
  # per-key length is asserted Rust-side). Empty unless the operator
  # configured keys — measure-only builds legitimately bake none.
  signingCfg = config.boot.nmbl.signing or { enable = false; enforce = false; publicKeys = [ ]; algorithm = "ml-dsa-65"; };
  algName =
    if (signingCfg.algorithm or "ml-dsa-65") == "ml-dsa-87" then "MlDsa87" else "MlDsa65";
  publicKeys = map (p: { path = p; alg = algName; }) (signingCfg.publicKeys or [ ]);

  # Signature ENFORCEMENT is active ⇒ the baked-key set is MANDATORY, so a
  # zero-key build is rejected (FIX-24). Measure-only / audit-only builds keep
  # this false so they can build with no keys.
  requireKeys = (signingCfg.enable or false) && (signingCfg.enforce or false);

  # Resolved /init binary used by the initramfs builder. Identity-equal
  # to the prebuilt `nmblInit` / `nmblInitSplash` in the single-feature,
  # keyless cases so Nix's store-path dedup keeps the existing CI cache
  # hot. STRUCTURED selection (FIX-17): whenever baked keys are configured
  # we ALWAYS route through `mkNmblInit { publicKeys }`, regardless of the
  # splash/stateful feature mix, so a splash or stateful build never silently
  # drops the trust anchor.
  selectedNmblInit =
    if publicKeys != [ ] then
      mkNmblInit { features = nmblFeatures; inherit publicKeys requireKeys; }
    else if nmblFeatures == [ ] then
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
