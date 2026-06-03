# INSECURE TEST-ONLY signing keys

**DO NOT USE THESE KEYS FOR ANYTHING REAL.**

This directory holds a **fixed, committed, PUBLICLY-KNOWN** ML-DSA-87 keypair
used *only* by the NMBL VM test matrix to sign test generations / UKIs / driver
images so the verify→measure→kexec path can be exercised end to end. The private
key is checked into version control in the clear, so anything signed with it is
trivially forgeable by anyone with this repository.

| File | What it is |
|------|------------|
| `insecure-test-ml-dsa-87.key` | ML-DSA-87 **PRIVATE** key (`NMBLSK01` container). PUBLIC, INSECURE. |
| `insecure-test-ml-dsa-87.pub` | ML-DSA-87 raw public key (2592 bytes) — the blob `boot.nmbl.signing.publicKeys` bakes. |

Generated reproducibly with the host signer:

```
nmbl-sign keygen --alg ml-dsa-87 \
  --out-priv testing/keys/insecure-test-ml-dsa-87.key \
  --out-pub  testing/keys/insecure-test-ml-dsa-87.pub
```

## Guard rails

* `testing/keys.nix` exposes the keypair to the test harness ONLY and provides
  `signImage` glue (`signTestArtifact`) for signing test artifacts with it.
* `testing/keys.nix` also exports `assertAbsentFromClosure`, a build check that
  FAILS if the **private** key's bytes ever appear in a production NMBL
  initramfs/UKI closure. This test key must never reach a production artifact.
  The check mirrors the existing closure-leak / `nmbl-tpm-enroll`-absence
  asserts.

If you need real signing, generate a fresh keypair, keep the private key OFF the
store (pass it as a string path to an on-disk secret, e.g.
`"/run/secrets/nmbl.key"`), and never commit it.
