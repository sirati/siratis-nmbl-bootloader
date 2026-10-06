# Insecure test-only signing keys

Do not use these keys for anything real.

This directory holds fixed, committed, publicly known keys for the NMBL VM
test matrix. The private keys are in version control in the clear, so anyone
with this repository can forge anything signed with them.

| File | Contents |
|------|----------|
| `insecure-test-ml-dsa-87.key` | ML-DSA-87 private key (`NMBLSK01` container). Public and insecure. |
| `insecure-test-ml-dsa-87.pub` | ML-DSA-87 raw public key (2592 bytes). The test configs bake it with `boot.nmbl.signing.publicKeys`. |
| `insecure-test-sb-db.key` | RSA-2048 private UEFI Secure Boot `db` signing key (PEM). Public and insecure. The install step `sbsign`s the test NMBL UKI with it. |
| `insecure-test-sb-db.crt` | Self-signed X.509 `db` certificate (PEM) for that key. The test runners enroll it into the OVMF `db`, so the enforcing firmware starts the test-signed UKI and still refuses unsigned ones. |

## How the tests use them

The secure-boot scenarios sign at install time, from file paths, in the same
way production does. No Nix derivation signs anything with these keys.
The `sb-install-*` orchestrators (`testing/sb-install.nix`) read the keys from
`--keys-dir`, `$NMBL_TEST_KEYS_DIR`, or `$PWD/testing/keys`. They copy them
into the installed root at `/var/lib/nmbl-test-keys/` between the disko and
install phases of nixos-anywhere, and delete them after the install:

- the ML-DSA key as `insecure-test-gen.key` (generation and external config
  signatures) and as `insecure-test-image.key` (driver images);
- `insecure-test-sb-db.key` and `insecure-test-sb-db.crt` for the UKI.

The runner for the NMBL scenarios enrolls `insecure-test-sb-db.crt` into
`db` in addition to the Microsoft KEK and `db` (`virt-fw-vars --add-db`).
The unsigned-UKI smoke test (`check-sb-unsigned-uki`) keeps the Microsoft-only
`db`, so the firmware still refuses its UKI. See
[secure-boot-matrix.md](../secure-boot-matrix.md).

The `test-secure-boot-domain-transplant-refused` scenario signs the generation
kernel with the committed ML-DSA key under the `driver-image` domain, outside
any derivation, and checks that NMBL refuses it.

## Guard rails

`testing/keys.nix` exposes `privateKey`, `publicKey` and
`assertAbsentFromClosure`. `assertAbsentFromClosure` builds a check that fails
if the store path of a test private key appears in the closure of the given
roots. The flake builds these checks with it:

| Check | Closure |
|---|---|
| `insecure-test-key-absent` | the NMBL initramfs of the production-shaped `test-gpt-uefi-grub` config |
| `test-secure-boot-no-private-key` | the `test-secure-boot` install store paths (disko script and toplevel), also checking the `db` key |
| `test-secure-boot-driver-no-private-key` | the same for `test-secure-boot-driver` |

All three are in `checks.x86_64-linux`, for example
`nix build .#checks.x86_64-linux.insecure-test-key-absent`.

## Regenerating

The `db` pair:

```
openssl req -new -x509 -newkey rsa:2048 -nodes \
  -subj "/CN=INSECURE NMBL TEST Secure-Boot db/" \
  -keyout testing/keys/insecure-test-sb-db.key \
  -out    testing/keys/insecure-test-sb-db.crt \
  -days 36500 -sha256
```

The ML-DSA pair, with the host signer:

```
nmbl-sign keygen --alg ml-dsa-87 \
  --out-priv testing/keys/insecure-test-ml-dsa-87.key \
  --out-pub  testing/keys/insecure-test-ml-dsa-87.pub
```

## Real keys

For real signing, generate a fresh keypair and keep the private key out of
the Nix store and out of version control.

`nmbl-sign keygen --stdio` writes the private key to stdout and the raw public
key to fd 3, so a secrets store can take the private key from the pipe and it
does not touch disk:

```
nmbl-sign keygen --alg ml-dsa-87 --stdio \
  3> nmbl.pub | nix-secrets <store command for the private key>
```

`nmbl-sign sign --key-stdin` reads the key from a pipe. Set
`boot.nmbl.signing.generationKeyCommand` and `imageKeyCommand` to a command
that prints the key (for example `[ "nix-secrets" "pipe-secret" "nmbl-key" ]`),
and the install step pipes it into `nmbl-sign sign --key-stdin` once per
signature. `generationKeyFile` and `imageKeyFile` take a path to an on-disk
secret instead (for example `"/run/secrets/nmbl.key"`).
