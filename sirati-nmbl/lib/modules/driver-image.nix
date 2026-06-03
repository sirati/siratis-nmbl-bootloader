# Driver-image squashfs BUILD + ESP staging + install-time signing (#25a).
#
# The `boot.nmbl.driverImages.*` OPTIONS live in
# `lib/modules/security/driver-image.nix` (#8, which also enforces the
# enable⇒secure-boot rejection — FIX-05). This file is the BUILD side: for
# each configured image it produces a signed-on-install squashfs of the
# out-of-tree `.ko` (+ their firmware), staged onto the ESP at the
# boot-relative `path`, with a detached ML-DSA sidecar at `sigPath`.
#
# Signing model (mirrors the UKI `sbsign` posture in lib/install-signing.nix):
#   * The squashfs derivation is PURE — it contains only build inputs (kernel
#     modules + firmware), never a private key.
#   * The detached signature is produced AT INSTALL TIME by `nmbl-sign sign
#     --domain driver-image`, reading the ML-DSA private key IMPURELY from
#     `boot.nmbl.signing.imageKeyFile`. The key NEVER enters the store / the
#     system closure (closure-leak assertion below), exactly like the SB key.
#   * The runtime loader (#23) reads the squashfs at `path` and its sidecar at
#     `sigPath`, verifies the sidecar against the baked `publicKeys`
#     (`--domain driver-image`), then loop-mounts + `finit_module`s it.
#
# Used as a pure function:
#   import ./modules/driver-image.nix
#     { inherit pkgs lib config cfg nmblSign rescueModulesTree; }
#   -> { driverImageInstallShell = <shell fragment>; driverImages = [ … ]; }

{
  pkgs,
  lib,
  config,
  cfg,
  # The host-platform `nmbl-sign` signer derivation, threaded in from the
  # flake (`_module.args.nmblSign`). May be null on an older host flake that
  # predates the signer; in that case install-time signing is a hard eval
  # error WHEN driver images are enabled (it would otherwise silently ship
  # unsigned blobs the runtime would refuse).
  nmblSign ? null,
  # NMBL's exact kernel module tree (same derivation the rescue closure uses,
  # `pkgs.aggregateModules [ (lib.getOutput "modules" cfg.kernelPackage) ]`).
  rescueModulesTree,
}:

let
  driverCfg = cfg.driverImages or { enable = false; images = { }; };
  enabled = driverCfg.enable or false;

  # The ADDITIVE module-closure factor (FIX-36): an EXPLICIT per-image
  # `firmwareName` keeps each image's firmware env (and thus its closure) on a
  # distinct store path, and leaves the rescue closure (built inline in
  # lib/config.nix with the `nmbl-rescue-firmware` name) byte-identical.
  makeClosure = import ./module-closure.nix { inherit pkgs lib; };

  # ML-DSA image-signing key, read IMPURELY at install time. Reuses the same
  # closure-leak posture as the UKI key: a STRING path stays out of the store;
  # a Nix path literal would be imported and is rejected below.
  signingCfg = config.boot.nmbl.signing or { };
  imageKeyFile = signingCfg.imageKeyFile or null;
  storeDir = builtins.storeDir;
  imageKeyStr = if imageKeyFile == null then null else toString imageKeyFile;
  imageKeyIsStorePath = imageKeyStr != null && lib.hasPrefix storeDir imageKeyStr;

  # Eval-time guards. Only meaningful when driver images are enabled.
  signingChecked =
    assert lib.assertMsg (!enabled || nmblSign != null) ''
      boot.nmbl.driverImages.enable = true but the `nmbl-sign` signer is not
      available to the module (nmblSign == null). Driver images are signed at
      install time with `nmbl-sign --domain driver-image`; without the signer
      the runtime loader would refuse every (unsigned) image. Update the host
      flake so it threads `_module.args.nmblSign`.
    '';
    assert lib.assertMsg (!enabled || imageKeyFile != null) ''
      boot.nmbl.driverImages.enable = true but boot.nmbl.signing.imageKeyFile
      is null. Each driver image is signed at install time with the operator's
      ML-DSA private key (the pair of a baked boot.nmbl.signing.publicKeys
      entry). Set boot.nmbl.signing.imageKeyFile to the on-disk private key.
    '';
    assert lib.assertMsg (!(enabled && imageKeyIsStorePath)) ''
      boot.nmbl.signing.imageKeyFile resolves to a Nix store path:
        ${toString imageKeyStr}
      The ML-DSA signing PRIVATE key must NEVER enter the store / system
      closure. Pass it as a STRING path to an on-disk secret read at install
      time, e.g. boot.nmbl.signing.imageKeyFile = "/run/secrets/nmbl-img.key";
      not a Nix path literal like ./img.key (which Nix imports into the store).
    '';
    true;

  # Build one driver-image squashfs from a single `images.<name>` submodule.
  # Layout mirrors the rescue closure staging (full-system.nix): the closure's
  # /lib/modules/<kver> (+ /lib/firmware) is `cp -aL`'d into the squashfs root
  # so the running (NMBL) kernel's `uname -r` matches and `finit_module`
  # resolves the .ko. `cp -aL` dereferences the build-host store symlinks so
  # the blob is self-contained (the boot environment has no /nix/store).
  buildImage = name: img:
    let
      closure = makeClosure {
        rootModules = img.modules;
        kernel = rescueModulesTree;
        firmware = img.firmware;
        firmwareName = "nmbl-driver-${name}-firmware";
      };
      closurePath = if closure != null then "${closure}" else "";
    in
    pkgs.runCommand "nmbl-driver-${name}.sfs"
      {
        nativeBuildInputs = [ pkgs.squashfsTools ];
      }
      ''
        mkdir -p root/lib
        mc=${lib.escapeShellArg closurePath}
        if [ -n "$mc" ] && [ -d "$mc/lib/modules" ]; then
          echo "staging driver modules from $mc/lib/modules"
          cp -aL "$mc/lib/modules" root/lib/modules
          if [ -d "$mc/lib/firmware" ]; then
            echo "staging driver firmware from $mc/lib/firmware"
            cp -aL "$mc/lib/firmware" root/lib/firmware
          else
            echo "no driver firmware in closure (no module requested any)"
          fi
        else
          echo "WARNING: driver image '${name}' resolved no out-of-tree modules"
        fi
        chmod -R u+w root

        # Deterministic, best-ratio squashfs (same flags as the rescue blob):
        # zstd-19, -noappend for reproducibility, -all-root so no host uid leaks.
        mksquashfs root "$out" \
          -comp zstd -Xcompression-level 19 \
          -noappend \
          -all-root \
          -no-progress
      '';

  # Strip a leading slash so the boot-relative path joins cleanly under /boot.
  bootRel = p: if lib.hasPrefix "/" p then lib.removePrefix "/" p else p;

  # Per-image install record: the pure squashfs derivation plus the on-ESP
  # destinations for the blob and its detached sidecar.
  imageRecords = lib.mapAttrsToList
    (name: img: {
      inherit name;
      sfs = buildImage name img;
      destPath = "/boot/${bootRel img.path}";
      sigDest = "/boot/${bootRel img.sigPath}";
    })
    (driverCfg.images or { });

  # Install-time shell: copy each squashfs onto the ESP, then sign it in place
  # with `nmbl-sign --domain driver-image` reading the impure key, writing the
  # detached sidecar where the runtime loader (#23) looks. Force `signingChecked`
  # so the eval-time guards run whenever this fragment is built.
  signImageShell = record:
    let
      destArg = lib.escapeShellArg record.destPath;
      sigArg = lib.escapeShellArg record.sigDest;
      keyArg = lib.escapeShellArg (toString imageKeyStr);
      sigDir = builtins.dirOf record.sigDest;
    in ''
      echo "Staging NMBL driver image '${record.name}' to ${destArg}..."
      install -D -m 0644 ${record.sfs} ${destArg}
      mkdir -p ${lib.escapeShellArg sigDir}
      echo "Signing driver image '${record.name}' (nmbl-sign, install-time, --domain driver-image)..."
      ${nmblSign}/bin/nmbl-sign sign \
        --key ${keyArg} \
        --domain driver-image \
        --out ${sigArg} \
        ${destArg}
      echo "✓ Driver image '${record.name}' installed + signed (${sigArg})"
    '';

  driverImageInstallShell =
    lib.optionalString enabled (
      assert signingChecked;
      lib.concatMapStringsSep "\n" signImageShell imageRecords
    );
in
{
  inherit driverImageInstallShell;
  # The pure squashfs derivations, surfaced for `system.build` introspection /
  # store-path identity checks (the rescue closure must stay unchanged).
  driverImages = imageRecords;
}
