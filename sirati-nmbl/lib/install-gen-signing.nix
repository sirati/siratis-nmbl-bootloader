# NMBL install-time per-generation signing (#53 — boot-guard sidecars).
#
# Returns a shell-script fragment (or "" when disabled) spliced into the
# installer (lib/install-signing.nix). When `boot.nmbl.signing.enable` is set
# it signs EVERY bootable NixOS generation's kernel + initrd at install time,
# writing detached ML-DSA sidecars onto the writable boot partition where the
# in-initramfs verify guard (#18/#20) scans for them. Without this an
# ENFORCING install would refuse every generation — the guard would find no
# sidecar to verify.
#
# Cross-references (the contracts this fragment must hold byte-for-byte):
#   * gen-id (FIX-07): computed via `nmbl-init --print-gen-id <link>`, the SAME
#     derivation the runtime uses (generations/gen_id.rs:
#     basename(canonicalize(toplevel))), so signer and verifier agree on the id.
#   * sidecar layout: `<boot>/nmbl/sigs/<gen-id>/{kernel,initrd}<sigPathSuffix>`,
#     EXACTLY where src/sig/scan.rs::resolve_sig_sidecar looks.
#   * per-role domains: `gen-kernel` / `gen-initrd` (nmbl-host-tools domain.rs).
#   * signed blobs: `<toplevel>/kernel` and `<toplevel>/initrd`, the same paths
#     the runtime resolves+verifies (generations/resolve.rs).
#
# The signing PRIVATE key is read IMPURELY from a non-store path and never
# enters the store/closure (closure-leak assert below; same posture as the UKI
# and driver-image keys).

{
  lib,
  config,
  # Install-time per-generation ML-DSA signing policy
  # (config.boot.nmbl.signing.{enable,generationKeyFile,sigPathSuffix}).
  genSigning ? {
    enable = false;
    keyFile = null;
    sigPathSuffix = ".sig";
  },
  # The host-platform `nmbl-sign` derivation (flake `_module.args.nmblSign`).
  # `null` on an older host flake; only dereferenced when signing is enabled.
  nmblSign ? null,
}:

let
  genSignEnable = genSigning.enable or false;
  genKeyFile = genSigning.keyFile or null;
  # Sidecar filename suffix — MUST match `signing.sigPathSuffix` (the Rust
  # `config.signing.sig_path_suffix`) so `src/sig/scan.rs::resolve_sig_sidecar`
  # finds `<boot>/nmbl/sigs/<gen-id>/{kernel,initrd}<suffix>` at boot.
  genSigSuffix = genSigning.sigPathSuffix or ".sig";

  storeDir = builtins.storeDir;

  # CLOSURE-LEAK ASSERT (CRITICAL) — same posture as the UKI/image keys. The
  # ML-DSA generation-signing PRIVATE key must never enter the store / system
  # closure: `toString` interpolates the bare on-disk path (a STRING path stays
  # out of the store; a Nix path literal would be imported), and the eval FAILS
  # if it resolves under `builtins.storeDir`.
  genKeyStr = if genKeyFile == null then null else toString genKeyFile;
  genKeyIsStorePath = genKeyStr != null && lib.hasPrefix storeDir genKeyStr;

  genClosureLeakChecked =
    assert lib.assertMsg (!(genSignEnable && genKeyFile == null)) ''
      boot.nmbl.signing.enable is set but boot.nmbl.signing.generationKeyFile
      is null. Each bootable generation's kernel + initrd is signed at install
      time so NMBL's pre-kexec verify guard has sidecars to check; an enforcing
      install would otherwise refuse every generation. Set
      boot.nmbl.signing.generationKeyFile to the on-disk ML-DSA private key
      (the PRIVATE half of a baked boot.nmbl.signing.publicKeys entry).
    '';
    assert lib.assertMsg (!(genSignEnable && nmblSign == null)) ''
      boot.nmbl.signing.enable is set but the `nmbl-sign` signer is not
      available to install-gen-signing.nix (nmblSign == null). Generations are
      signed at install time with `nmbl-sign sign --domain gen-kernel` /
      `--domain gen-initrd`; update the host flake so it threads
      `_module.args.nmblSign`.
    '';
    assert lib.assertMsg (!(genSignEnable && genKeyIsStorePath)) ''
      boot.nmbl.signing.generationKeyFile resolves to a Nix store path:
        ${toString genKeyStr}
      The ML-DSA signing PRIVATE key must NEVER enter the store / system
      closure. Pass it as a STRING path to an on-disk secret read at install
      time, e.g. boot.nmbl.signing.generationKeyFile = "/run/secrets/nmbl-gen.key";
      not a Nix path literal like ./gen.key (which Nix imports into the store).
    '';
    true;

  # Escaped install-time-impure key path + the `nmbl-init` / `nmbl-sign`
  # binaries. The key only appears as a literal argument to the imperative
  # signing command, never inside a derivation.
  genKeyArg = lib.escapeShellArg (toString genKeyStr);
  nmblInitBin = "${config.system.build.nmblInit}/bin/nmbl-init";
  nmblSignBin = lib.optionalString (nmblSign != null) "${nmblSign}/bin/nmbl-sign";

  # The install-time NixOS system-profile directory. At install time the target
  # root is `/`, so generations live at `/nix/var/nix/profiles/system-*-link`
  # (the SAME directory whose `system/init` the installer symlinks). This is the
  # install-time view of the runtime mount-relative `paths.nixProfilesDir`; the
  # gen-id computed from each link is identical to the one NMBL computes at boot
  # (both canonicalize the same store path).
  installProfilesDir = "/nix/var/nix/profiles";
  genSigsRoot = "/boot/nmbl/sigs";
  genSigsRootArg = lib.escapeShellArg genSigsRoot;

  # Per-generation signing shell. Force `genClosureLeakChecked` so the eval-time
  # guards always run when this fragment is built.
  genSignShell =
    assert genClosureLeakChecked;
    ''
      echo "Signing bootable NixOS generations for the NMBL boot guard..."
      mkdir -p ${genSigsRootArg}

      # Track the gen-ids we (re)signed so stale sidecar dirs can be pruned.
      nmbl_live_gen_ids=""

      # Generation lister (FIX-48): iterate the SAME `system-<N>-link` profile
      # symlinks NixOS / systemd-boot enumerate. nullglob-guarded so an empty
      # profiles dir is a clean skip, not a literal `system-*-link` path.
      shopt -s nullglob
      for generation in ${installProfilesDir}/system-*-link; do
        [ -L "$generation" ] || continue

        # gen-id (FIX-07): ask the SAME binary the runtime uses so the id is
        # byte-for-byte identical (canonicalize the link -> store basename).
        # nmbl-init exits non-zero / empty on an unresolvable link; skip it.
        gen_id=$(${nmblInitBin} --print-gen-id "$generation" 2>/dev/null || true)
        if [ -z "$gen_id" ]; then
          echo "  WARNING: could not compute gen-id for $generation; skipping"
          continue
        fi

        # Resolve the generation's toplevel + its kernel/initrd. The runtime
        # verifier resolves+checks `<toplevel>/kernel` and `<toplevel>/initrd`
        # (generations/resolve.rs), so we sign exactly those blobs.
        top=$(readlink -f "$generation")
        gen_kernel="$top/kernel"
        gen_initrd="$top/initrd"
        if [ ! -e "$gen_kernel" ] || [ ! -e "$gen_initrd" ]; then
          echo "  WARNING: generation $gen_id missing kernel/initrd; skipping"
          continue
        fi

        # Sidecar dir on the writable boot FS — EXACTLY where
        # src/sig/scan.rs::resolve_sig_sidecar looks:
        #   <boot>/nmbl/sigs/<gen-id>/{kernel,initrd}<sigPathSuffix>
        sig_dir=${genSigsRootArg}/"$gen_id"
        mkdir -p "$sig_dir"

        echo "  Signing generation $gen_id (kernel + initrd)..."
        ${nmblSignBin} sign \
          --key ${genKeyArg} \
          --domain gen-kernel \
          --out "$sig_dir/kernel${genSigSuffix}" \
          "$gen_kernel"
        ${nmblSignBin} sign \
          --key ${genKeyArg} \
          --domain gen-initrd \
          --out "$sig_dir/initrd${genSigSuffix}" \
          "$gen_initrd"
        echo "  ✓ generation $gen_id signed -> $sig_dir/{kernel,initrd}${genSigSuffix}"

        nmbl_live_gen_ids="$nmbl_live_gen_ids $gen_id"
      done
      shopt -u nullglob

      # Prune stale per-generation sidecar dirs for generations that no longer
      # exist, so /boot/nmbl/sigs does not accumulate across upgrades (consistent
      # with how the bootloader GCs entries for removed generations).
      if [ -d ${genSigsRootArg} ]; then
        for sig_dir in ${genSigsRootArg}/*; do
          [ -d "$sig_dir" ] || continue
          existing_id=$(basename "$sig_dir")
          keep=no
          for live_id in $nmbl_live_gen_ids; do
            if [ "$existing_id" = "$live_id" ]; then keep=yes; break; fi
          done
          if [ "$keep" = no ]; then
            echo "  Pruning stale generation sidecars for $existing_id"
            rm -rf "$sig_dir"
          fi
        done
      fi
      echo "✓ Generation signing complete."
    '';
in

lib.optionalString genSignEnable genSignShell
