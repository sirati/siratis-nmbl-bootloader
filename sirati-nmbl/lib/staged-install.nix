# Staged-boot install-time staging + `nmbl-sign` signing (test-matrix #2).
#
# FEATURE #2 (staged boot): the priority volume carries an image with MORE
# drivers + instructions that NMBL loads as a second stage. After the priority
# gate attests the (inside-LUKS) volume, `apply_staged_boot` single-fd verifies
# BOTH the staged image (`--domain driver-image`) AND a signed config fragment
# (`--domain staged-fragment`), transactionally merges the fragment into the
# base config, then re-runs the merged config's effects (explicit modules,
# driver images, activations).
#
# This module is the BUILD/INSTALL side for the test scenario: for the
# configured `boot.nmbl.staged.*` pointer set it
#   * builds a small (pure) staged squashfs image, and
#   * emits an install-time shell that stages the priority file, the staged
#     image, and the signed config fragment onto the priority volume's
#     filesystem (the decrypted cryptroot, mounted at the install root `/`),
#     then signs each in place with `nmbl-sign` reading the ML-DSA private key
#     IMPURELY from a path — exactly the driver-image/UKI runtime-install model,
#     so NO signing key ever enters a derivation.
#
# Signing model (mirrors lib/modules/driver-image.nix):
#   * The squashfs + fragment derivations are PURE — never a private key.
#   * The detached signatures are produced AT INSTALL TIME by `nmbl-sign sign`,
#     reading the key from `boot.nmbl.signing.generationKeyFile` (the same impure
#     on-disk path the per-generation signing uses; its public half is the baked
#     trust anchor). The key NEVER enters the store / system closure.
#   * At boot the priority gate verifies `priority.signed` under
#     `nmbl:priority-file:v1`, then `apply_staged_boot` verifies the image under
#     `nmbl:driver-image:v1` and the fragment under `nmbl:staged-fragment:v1`.
#
# Used as a pure function:
#   import ./staged-install.nix { inherit pkgs lib config cfg nmblSign; }
#   -> { stagedInstallShell = <shell fragment>; }

{
  pkgs,
  lib,
  config,
  cfg,
  # The host-platform `nmbl-sign` signer (flake `_module.args.nmblSign`). Only
  # dereferenced when staged boot is enabled; a null signer on an enabled build
  # is a hard eval error (it would otherwise ship unsigned blobs the runtime
  # refuses).
  nmblSign ? null,
}:

let
  stagedCfg = cfg.staged or { enable = false; };
  signingCfg = config.boot.nmbl.signing or { };
  # Gate staging+signing on `!deferInstallSigning`, EXACTLY like the UKI and
  # per-generation signing (lib/install-signing.nix). The build-time disko
  # `vmDiskImage` builds with `deferInstallSigning = true` (the default) — the
  # impure ML-DSA key is unreadable in that sealed image builder, so signing
  # there would fail and kill the build. The runtime nixos-anywhere install
  # variant forces `deferInstallSigning = false`, so the staged artifacts are
  # staged + signed on the disk the scenario actually boots. (The runner copies
  # that runtime-signed disk over `NMBL_DISK_IMAGE`, never the deferred image.)
  deferInstallSigning = signingCfg.deferInstallSigning or false;
  enabled = (stagedCfg.enable or false) && !deferInstallSigning;
  # Reuse the per-generation ML-DSA private key path (impure string path; its
  # public half is the baked trust anchor). A string path stays out of the
  # store; a Nix path literal would be imported and is rejected below.
  keyFile = signingCfg.generationKeyFile or null;
  storeDir = builtins.storeDir;
  keyStr = if keyFile == null then null else toString keyFile;
  keyIsStorePath = keyStr != null && lib.hasPrefix storeDir keyStr;

  # Eval-time guards, only meaningful when staged boot is enabled.
  signingChecked =
    assert lib.assertMsg (!enabled || nmblSign != null) ''
      boot.nmbl.staged.enable = true but the `nmbl-sign` signer is not available
      to the module (nmblSign == null). The staged image + fragment are signed
      at install time with `nmbl-sign`; without the signer the runtime would
      refuse the (unsigned) staged blobs. Thread `_module.args.nmblSign`.
    '';
    assert lib.assertMsg (!enabled || keyFile != null) ''
      boot.nmbl.staged.enable = true but boot.nmbl.signing.generationKeyFile is
      null. The staged image + fragment + priority file are signed at install
      time with the operator's ML-DSA private key (the pair of a baked
      boot.nmbl.signing.publicKeys entry). Set
      boot.nmbl.signing.generationKeyFile to the on-disk private key path.
    '';
    assert lib.assertMsg (!(enabled && keyIsStorePath)) ''
      boot.nmbl.signing.generationKeyFile resolves to a Nix store path:
        ${toString keyStr}
      The ML-DSA signing PRIVATE key must NEVER enter the store / system
      closure. Pass it as a STRING path to an on-disk secret read at install
      time, not a Nix path literal like ./gen.key (which Nix imports into the
      store).
    '';
    true;

  # The signed config fragment the staged volume ships. A PARTIAL Config overlay
  # (config/fragment.rs): it adds one EXTRA explicit kernel module the base
  # config does NOT load, so the staged re-run loading it is the observable
  # proof the merge took effect (the module ships in the initrd via
  # boot.initrd.availableKernelModules but is never in the base explicit list).
  # The merge replaces the whole [kernel_modules] table; modules_dir is kept at
  # the runtime default so the dep walk still resolves.
  fragmentModule = stagedCfg.fragmentModule or "dummy";
  fragmentText = ''
    # NMBL staged-boot config fragment (test scenario, FEATURE #2).
    # Applied on top of the base config once its detached signature verifies.
    [kernel_modules]
    explicit = [ "${fragmentModule}" ]
    modules_dir = "/lib/modules"
  '';
  fragmentFile = pkgs.writeText "nmbl-staged-fragment.toml" fragmentText;

  # The staged driver image: a small, valid, self-contained squashfs. It is
  # verified (single-fd, --domain driver-image) but NOT loop-mounted in this
  # scenario (the fragment carries no [driver_images] table that names it), so
  # its content only has to be a real squashfs that passes the signature check.
  # We ship a marker file so the blob is non-empty and deterministic.
  stagedImage = pkgs.runCommand "nmbl-staged.sfs"
    { nativeBuildInputs = [ pkgs.squashfsTools ]; }
    ''
      mkdir -p root/nmbl-staged
      printf 'nmbl staged-boot test image (FEATURE #2)\n' > root/nmbl-staged/MARKER
      mksquashfs root "$out" \
        -comp zstd -Xcompression-level 19 \
        -noappend -all-root -no-progress
    '';

  # Strip a leading slash so a volume-relative path joins cleanly under the
  # priority-volume root (= the install chroot's `/`).
  volRel = p: if lib.hasPrefix "/" p then lib.removePrefix "/" p else p;

  # The priority-volume-relative destinations the runtime reads (joined under
  # the gate's mountpoint at boot, here under `/` in the install chroot).
  imageDest = "/" + volRel stagedCfg.image;
  fragmentDest = "/" + volRel stagedCfg.fragment;
  fragmentSigDest = "/" + volRel stagedCfg.sig;
  priorityDest = "/" + volRel cfg.secureBoot.signedFilePath;
  # The priority-file sidecar uses the configured suffix (default `.sig`).
  prioritySigSuffix = signingCfg.sigPathSuffix or ".sig";
  prioritySigDest = priorityDest + prioritySigSuffix;
  # The staged image sidecar is a `<image><suffix>` sibling (verify::sidecar_path).
  imageSigDest = imageDest + prioritySigSuffix;

  signArg = p: lib.escapeShellArg p;

  stagedInstallShell = lib.optionalString enabled (
    assert signingChecked;
    ''
      echo "Staging NMBL staged-boot artifacts onto the priority volume (install root)..."

      # 1. Priority file: any blob the gate verifies under nmbl:priority-file:v1.
      #    A small marker keeps it deterministic + non-empty.
      install -D -m 0644 /dev/null ${signArg priorityDest}
      printf 'nmbl priority-volume attestation file (FEATURE #2)\n' > ${signArg priorityDest}
      echo "Signing priority file (nmbl-sign, install-time, --domain priority-file)..."
      ${nmblSign}/bin/nmbl-sign sign \
        --key ${signArg (toString keyStr)} \
        --domain priority-file \
        --out ${signArg prioritySigDest} \
        ${signArg priorityDest}

      # 2. Staged driver image (squashfs), verified under nmbl:driver-image:v1.
      install -D -m 0644 ${stagedImage} ${signArg imageDest}
      echo "Signing staged image (nmbl-sign, install-time, --domain driver-image)..."
      ${nmblSign}/bin/nmbl-sign sign \
        --key ${signArg (toString keyStr)} \
        --domain driver-image \
        --out ${signArg imageSigDest} \
        ${signArg imageDest}

      # 3. Signed config fragment, verified under nmbl:staged-fragment:v1. Its
      #    detached signature lives at the explicit [staged].sig path.
      install -D -m 0644 ${fragmentFile} ${signArg fragmentDest}
      echo "Signing staged fragment (nmbl-sign, install-time, --domain staged-fragment)..."
      ${nmblSign}/bin/nmbl-sign sign \
        --key ${signArg (toString keyStr)} \
        --domain staged-fragment \
        --out ${signArg fragmentSigDest} \
        ${signArg fragmentDest}

      echo "✓ Staged-boot artifacts staged + signed on the priority volume:"
      echo "    ${priorityDest} (+ ${prioritySigDest})"
      echo "    ${imageDest} (+ ${imageSigDest})"
      echo "    ${fragmentDest} (+ ${fragmentSigDest})"
    ''
  );
in
{
  inherit stagedInstallShell;
  # Surfaced for store-path introspection / debugging.
  stagedBuild = {
    inherit stagedImage fragmentFile;
  };
}
