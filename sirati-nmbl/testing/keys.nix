# INSECURE TEST-ONLY signing keypair: Nix glue for the VM test matrix.
#
# This module exposes the fixed, committed ML-DSA-87 keypair under
# `testing/keys/` to the test harness:
#
#   * `publicKey` — the committed PUBLIC key file, imported into the store so it
#     can be baked into nmbl-init as the trust anchor (`signing.publicKeys`). A
#     public key is safe to put in a derivation: it is not a secret.
#   * `assertAbsentFromClosure` — a BUILD CHECK that FAILS if the insecure-test
#     PRIVATE key (by store path) leaks into a NMBL closure (initramfs / UKI /
#     signed disk). Mirrors the closure-leak / nmbl-tpm-enroll absence asserts
#     in lib/{install-signing,config}.nix — a signing PRIVATE key must NEVER be
#     an input to a Nix derivation. The private key's STORE path is resolved here
#     ONLY to learn the string to assert absent; it goes in the GUARD's inputs,
#     never the closure under test (whose freedom from it is the whole point).
#
# DELIBERATELY ABSENT (the F6b signing-model fix): there is no `signTestArtifact`
# nor `bakedPublicKey` that imports the PRIVATE key into a signing derivation.
# Test artifacts (the secure-boot test disk + UKI) are signed at INSTALL RUNTIME
# by lib/install-{signing,gen-signing}.nix reading the key from a PATH staged in
# the rescue installer — exactly as production does — so the private key never
# enters any derivation. `nmblSign` is therefore no longer threaded in.

{
  pkgs,
  lib,
}:

let
  # The committed PRIVATE key path, resolved to its store path ONLY so the
  # absence guard below knows the path string to assert is missing. Importing it
  # here puts it in the *guard* derivation's inputs (so the guard can name it),
  # NOT in the closure-under-test — the guard's whole purpose is to prove that
  # closure never references it. This is the same posture as the production
  # `insecure-test-key-absent` check. The key is intentionally public test-only
  # material (see testing/keys/README.md); it is never used to SIGN anything in
  # a derivation.
  privateKey = ./keys/insecure-test-ml-dsa-87.key;
  publicKey = ./keys/insecure-test-ml-dsa-87.pub;

  # Build check: the insecure-test PRIVATE key must be ABSENT from `closurePath`'s
  # transitive closure. Fails the build on a leak. `closurePath` is typically a
  # production `config.system.build.nmblInitramfs` / UKI derivation.
  #
  # Same posture as the `nmbl-tpm-enroll`-absence / closure-leak asserts in
  # lib/config.nix: it computes the transitive closure of `closurePath` and
  # FAILs if ANY signing PRIVATE key's store path is among the closure's
  # store-paths. By default it checks the ML-DSA generation key; `extraKeyPaths`
  # lets a caller ALSO assert the Secure-Boot `db` private key is absent (the
  # secure-boot test disk must reference NEITHER). The keys are fixed store
  # paths; a build that signs only at INSTALL RUNTIME (reading the key from a
  # staged path, never a derivation input) can never reference them.
  assertAbsentFromClosure =
    {
      name ? "insecure-test-key-absent",
      # A single closure root, OR (via `rootPaths`) several. The check passes
      # only when NO listed key appears in the COMBINED transitive closure.
      closurePath ? null,
      rootPaths ? (if closurePath == null then [ ] else [ closurePath ]),
      # Additional PRIVATE-key paths to also assert absent (e.g. the SB db key).
      # Each is INTERPOLATED (`"${p}"`) so Nix imports it and we learn the STORE
      # path a leak would actually appear as — `toString` would yield the source
      # path, which can never match a closure and would make the check trivially
      # pass. Importing here puts the key in the GUARD's inputs only (so it can
      # name the path); the guard's whole job is to prove the closure-under-test
      # does NOT reference it.
      extraKeyPaths ? [ ],
    }:
    let
      keyStorePaths = [ "${privateKey}" ] ++ map (p: "${p}") extraKeyPaths;
      # `closureInfo` takes multiple roots directly — no symlinkJoin (which would
      # collide on overlapping basenames between e.g. a diskoScript and toplevel).
      closure = pkgs.closureInfo { inherit rootPaths; };
      grepArgs = lib.concatMapStringsSep " " (p: "-e ${lib.escapeShellArg p}") keyStorePaths;
    in
    pkgs.runCommand name { } ''
      if grep -qxF ${grepArgs} ${closure}/store-paths; then
        echo "FAIL: an insecure-test signing PRIVATE key leaked into the" >&2
        echo "      closure under test. A signing private key must NEVER be an" >&2
        echo "      input to a Nix derivation — sign at install runtime from the" >&2
        echo "      key's path instead. Offending key(s):" >&2
        grep -xF ${grepArgs} ${closure}/store-paths >&2 || true
        exit 1
      fi
      echo "OK: no insecure-test signing private key is in the closure of:"
      printf '  %s\n' ${lib.escapeShellArgs rootPaths}
      touch "$out"
    '';
in
{
  inherit
    privateKey
    publicKey
    assertAbsentFromClosure
    ;
}
