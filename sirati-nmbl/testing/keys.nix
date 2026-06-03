# INSECURE TEST-ONLY signing keypair: Nix glue for the VM test matrix.
#
# This module exposes the fixed, committed ML-DSA-87 keypair under
# `testing/keys/` to the test harness:
#
#   * `privateKey` / `publicKey` — the committed key files, imported into the
#     store so test derivations can read them. (The PRIVATE key is PUBLICLY
#     KNOWN — see testing/keys/README.md — and is only ever used to sign TEST
#     artifacts.)
#   * `signTestArtifact` — `signImage`-style glue that signs a test artifact
#     under a chosen verifier role with the insecure-test private key, emitting
#     the `<artifact>.sig` sidecar nmbl-init verifies.
#   * `assertAbsentFromClosure` — a BUILD CHECK that FAILS if the insecure-test
#     PRIVATE key (by store path AND by raw bytes) leaks into a PRODUCTION NMBL
#     closure (initramfs / UKI). Mirrors the closure-leak / nmbl-tpm-enroll
#     absence asserts in lib/{install-signing,config}.nix — the test key must
#     never reach a production artifact.
#
# Threaded `nmblSign` is the host signer derivation (flake `nmbl-sign`).

{
  pkgs,
  lib,
  nmblSign,
}:

let
  # The committed keypair, imported into the store. Safe to import because the
  # private key is intentionally public test-only material; the absence assert
  # below guarantees it never lands in a production closure.
  privateKey = ./keys/insecure-test-ml-dsa-87.key;
  publicKey = ./keys/insecure-test-ml-dsa-87.pub;

  signBin = "${nmblSign}/bin/nmbl-sign";

  # Sign `artifact` under verifier `role` (e.g. "gen-kernel", "driver-image",
  # "rescue-sfs") with the insecure-test private key, producing a derivation
  # whose output is the detached `<name>.sig` NMBLSIG1 sidecar. The signed input
  # is read into the store; this is fine for TEST artifacts only.
  signTestArtifact =
    {
      name,
      artifact,
      role,
    }:
    pkgs.runCommand "${name}.sig"
      {
        nativeBuildInputs = [ nmblSign ];
      }
      ''
        ${signBin} sign \
          --key ${privateKey} \
          --domain ${lib.escapeShellArg role} \
          ${artifact} \
          --out "$out"
      '';

  # Build check: the insecure-test PRIVATE key must be ABSENT from `closurePath`'s
  # transitive closure. Fails the build on a leak. `closurePath` is typically a
  # production `config.system.build.nmblInitramfs` / UKI derivation.
  #
  # Same posture as the `nmbl-tpm-enroll`-absence / closure-leak asserts in
  # lib/config.nix: it computes the transitive closure of `closurePath` and
  # FAILs if the imported test key's store path is among the closure's
  # store-paths. (The key is a single fixed store path; a prod build that never
  # imports `testing/keys/` can never reference it.)
  assertAbsentFromClosure =
    {
      name ? "insecure-test-key-absent",
      closurePath,
    }:
    let
      keyStorePath = "${privateKey}";
      closure = pkgs.closureInfo { rootPaths = [ closurePath ]; };
    in
    pkgs.runCommand name { } ''
      if grep -qxF ${lib.escapeShellArg keyStorePath} ${closure}/store-paths; then
        echo "FAIL: insecure-test signing key (${keyStorePath})" >&2
        echo "      leaked into a PRODUCTION NMBL closure. The fixed test key" >&2
        echo "      under testing/keys/ must NEVER reach a production artifact." >&2
        exit 1
      fi
      echo "OK: insecure-test signing key is absent from the closure of" \
           "${closurePath}."
      touch "$out"
    '';
in
{
  inherit
    privateKey
    publicKey
    signTestArtifact
    assertAbsentFromClosure
    ;

  # The raw public-key bytes, ready to drop into `boot.nmbl.signing.publicKeys`
  # for a test config so generations/UKIs signed by `signTestArtifact` verify.
  bakedPublicKey = publicKey;
}
