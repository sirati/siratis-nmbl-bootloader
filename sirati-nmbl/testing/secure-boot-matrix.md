# Secure-boot VM test matrix

This file lists every secure-boot VM scenario with its flake app, the config
it boots, what its assertion script checks, and what a pass looks like. All
scenarios are wired as flake apps in `sirati-nmbl/flake.nix`. The assertion
scripts are in `testing/assertions/`.

## The configs

`testing/build_configurations.nix` defines the base config
`test-secure-boot`:

- `boot.nmbl.signing`: `enable = true`, `enforce = true`,
  `algorithm = "ml-dsa-87"`, `publicKeys = [ ./keys/insecure-test-ml-dsa-87.pub ]`,
  and `generationKeyFile = "/var/lib/nmbl-test-keys/insecure-test-gen.key"`;
- `boot.nmbl.signing.uki`: `enable = true`, with `keyFile` and `certFile` under
  `/var/lib/nmbl-test-keys/`. The install step `sbsign`s the NMBL UKI with the
  test `db` key, so the enforcing firmware accepts it;
- `boot.nmbl.tpm`: `measure = true`, `requireTpm = true`, `pcrIndex = 11`;
- `boot.nmbl.secureBoot`: `enable = true`, `enforce = true`,
  `requireTpm = true`, with no priority volume;
- a `cryptroot` LUKS device with `unlock = "tpm"`, `tpmPcrs = [ 11 7 ]` and
  `passToStage1`;
- `loader = "efi-stub"`, so NMBL boots as a UKI.

The environment variables `NMBL_GEN_KEY_FILE`, `NMBL_SB_DB_KEY_FILE` and
`NMBL_SB_DB_CERT_FILE` override the key paths at evaluation time.

The scenarios also use these variants, defined in
`testing/build_configurations.nix` and `flake.nix`:

| Config | Difference from `test-secure-boot` |
|---|---|
| `test-secure-boot-enroll` | `cryptroot` opens with the install passphrase. Same signed generation. |
| `test-secure-boot-driver` | Passphrase `cryptroot`, plus a signed driver image (see [Driver-image config prerequisites](#driver-image-config-prerequisites)). |
| `test-secure-boot-staged` | Passphrase `cryptroot`, plus a priority volume inside LUKS that carries a signed config fragment and staged image. |

Each runner is `testRunners.mkRunner` with `tpm = "tis"`, `secureBoot = true`,
`tpmPersist = true` and `dbCert = ./testing/keys/insecure-test-sb-db.crt`.
It boots OVMFFull with `smm=on` and a `db` that is the Microsoft
`OVMF_VARS.ms.fd` with the test `db` certificate added (`virt-fw-vars
--add-db`). The firmware starts the install-signed NMBL UKI and refuses
anything signed by neither Microsoft nor the test certificate. The
`check-sb-unsigned-uki` smoke test leaves `dbCert` unset, keeps the
Microsoft-only `db`, and proves that the firmware refuses an unsigned UKI.

`requireTpm = true` matters for the negative scenarios. A VM without a TPM
aborts the boot, so a negative scenario cannot pass on a machine without
`/dev/tpmrm0`.

## How the test disk is signed

A signing private key must stay out of every Nix derivation, because
derivation inputs land in the world-readable `/nix/store`. NMBL's normal
install-time code therefore signs the test disk from key paths, as in
production.

The orchestrator `nix run .#sb-install-test-secure-boot` (`testing/sb-install.nix`)
follows the nixos-anywhere install flow:

1. It boots a SystemRescue VM with a fresh 16G disk.
2. It runs `nixos-anywhere --phases kexec,disko` against the install variant of
   the config (`boot.nmbl.signing.deferInstallSigning = lib.mkForce false`, so
   signing runs in the installer).
3. It copies the committed test keys into the installed root, mounted at
   `/mnt` after disko, as `/mnt/var/lib/nmbl-test-keys/insecure-test-gen.key`,
   `insecure-test-image.key`, `insecure-test-sb-db.key` and
   `insecure-test-sb-db.crt`. It reads them from `--keys-dir`, else
   `$NMBL_TEST_KEYS_DIR`, else `$PWD/testing/keys`. The keys go under
   `/var/lib` because the install phase runs `installBootLoader` in the
   `nixos-install` chroot, whose activation mounts a fresh tmpfs over `/run`
   before signing.
4. It runs `nixos-anywhere --phases install`. In the chroot,
   `lib/install-signing.nix` `sbsign`s the NMBL UKI and writes
   `EFI/BOOT/BOOTX64.EFI`. `lib/install-gen-signing.nix` signs each
   generation's kernel and initrd with the ML-DSA key (domains `gen-kernel` and
   `gen-initrd`) into `/boot/nmbl/sigs/<gen-id>/{kernel,initrd}.sig`.
5. It deletes the staged keys from the installed disk and leaves the signed
   disk at `$WORK_DIR/disk1.qcow2`.

`sb-install-test-secure-boot-enroll`, `-driver` and `-staged` do the same for
the variants. The closure guards
`nix build .#checks.x86_64-linux.test-secure-boot-no-private-key` and
`.#checks.x86_64-linux.test-secure-boot-driver-no-private-key` check that the
install store paths (disko script and toplevel) reference neither the ML-DSA
key nor the `db` private key. See [keys/README.md](keys/README.md).

## Running a scenario

Each scenario app first runs the matching orchestrator to produce a signed
disk, then boots it. Inputs:

- `NMBL_SSH_KEY` or `--ssh-key PATH`: a passphrase-less SSH private key file
  for nixos-anywhere. `SSH_PRIVATE_KEY` can hold the key text instead. Any
  throwaway key works. Only the installer VM authorizes it.
- `NMBL_TEST_KEYS_DIR` (optional): the directory with the install-time keys
  (`insecure-test-gen.key` or `insecure-test-ml-dsa-87.key`, plus
  `insecure-test-sb-db.key` and `insecure-test-sb-db.crt`). The default
  `$PWD/testing/keys` works when you run the app from the `sirati-nmbl`
  checkout.
- `NMBL_SB_SIGNED_DISK` and `NMBL_SB_ENROLL_DISK` (optional): reuse an existing
  signed disk and skip the install.
- `NMBL_SB_INSTALL_WORK` (optional): the install work directory. The default
  is `$PWD/.sb-install-work`, with one subdirectory per config (`real`,
  `enroll`, `driver`, `staged`).
- `NMBL_WALL_TIMEOUT` (optional): the wall-clock limit for the assertion
  script in seconds (default 1800, 3000 for the TPM roundtrip).

The apps export `NMBL_RUNNER` (and `NMBL_ENROLL_RUNNER` for the roundtrip),
`NMBL_DISK_IMAGE` and `NMBL_SB_DISK` (the signed disk) to the assertion
script. The roundtrip also exports `NMBL_SB_TPM_UKI`, the install-signed UKI
of the tpm-unlock config, which it extracts from that disk's ESP.

To produce a signed disk by hand:

    nix run .#sb-install-test-secure-boot -- --ssh-key ~/.ssh/id_ed25519
    # leaves $PWD/.sb-install-test-secure-boot/disk1.qcow2 (signed)

## Firmware smoke test

| id | app | scenario | assertion | pass |
|---|---|---|---|---|
| SB | `check-sb-unsigned-uki` | firmware refuses an unsigned UKI | Boots an unsigned UKI under SB-OVMF (`smm=on`, Microsoft-only `db`). The firmware refuses it and NMBL does not run. | `assertions/sb-unsigned-uki.sh` exits 0: a Secure Boot refusal banner or the UEFI shell appears, and no NMBL marker (`nmbl-init starting`, `phase N:`) appears. Run it first, so the rest of the matrix cannot pass on non-enforcing firmware. |

## Core scenarios

| id | app | config | assertion | pass |
|---|---|---|---|---|
| #3a | `test-secure-boot-tpm-roundtrip` | `-enroll`, then `test-secure-boot` | Phase 1 boots the enroll twin with the install passphrase and runs `nmbl-tpm-enroll` with the PCRs that `nmbl-tpm-enroll --uki "$NMBL_SB_TPM_UKI" --print-pcrs` predicts. The script then swaps the ESP UKI for the tpm-unlock UKI and power-cycles against the same persisted swtpm. Phase 2 types nothing. | `assertions/sb-tpm-roundtrip.sh` exits 0: phase 1 sealed a `systemd-tpm2` token, phase 2 reaches `root@test-secure-boot` with no passphrase prompt, no boot-failure terminus and no refusal. |
| #4a | `test-secure-boot-signed-gen-happy` | `-enroll` | A correctly signed generation verifies, is measured and kexecs. | `assertions/sb-signed-gen-happy.sh` exits 0: no refusal marker, and `root@test-secure-boot-enroll` is reached and interactive. |
| #4b | `test-secure-boot-bad-sig-refused` | `-enroll` | The script overwrites the leading bytes of every kernel and initrd sidecar under `/nmbl/sigs/` on the ESP of a disk copy. Verification fails, NMBL refuses and reboots into rescue. | `assertions/sb-bad-sig-refused.sh` exits 0: a refuse marker appears, the tampered generation does not boot, and no emergency shell prompt appears. |
| #1 | `test-secure-boot-driver-image` | `-driver` | A signed squashfs carries `dummy`, a module absent from the base initrd. NMBL verifies it over one descriptor, loop-mounts it and loads the module before the kexec. `/proc/modules` cannot prove this because kexec resets module state. | `assertions/sb-driver-image.sh` exits 0: no refusal, `root@test-secure-boot-driver` is reached, and the `nmbl-init` journal has the `driver-image loaded:` marker with `dummy`. |
| #1-NEG | `test-secure-boot-driver-image-bad-refused` | `-driver` | The script corrupts `/boot/nmbl/driver-extra.sfs` on the ESP. Verification fails, and NMBL refuses (`imageload/verify.rs`, then `policy::refuse_unsigned`, then `RebootIntoRescue`) before it mounts the image and before the LUKS prompt. | `assertions/sb-driver-image-bad-refused.sh` exits 0: a refuse marker appears, `driver-image loaded` is absent, the generation does not boot, and no emergency shell appears. |
| #2 | `test-secure-boot-staged` | `-staged` | `cryptroot` opens with the install passphrase. The post-unlock priority gate attests the volume. `apply_staged_boot` verifies the image (domain `driver-image`) and the fragment (domain `staged-fragment`), merges the fragment, which adds one explicit kernel module, and re-runs the merged config's effects. Then the system kexecs. `lib/staged-install.nix` signs all three artifacts at install time. | `assertions/sb-staged.sh` exits 0: no refuse marker, `root@test-secure-boot-staged` is reached, and the `nmbl-init` journal has `signature VALID`, `fragment applied`, `re-loading explicit kernel modules` and `staged rerun: loaded` with `dummy`. |

### Driver-image config prerequisites

A config with `boot.nmbl.driverImages` has two requirements. The
`test-secure-boot-driver` config meets both, and each one caused a refusal in
the VM before it was added:

- The config must be external (bootstrap mode). The loader resolves the
  boot-relative image path against the runtime boot mountpoint, which exists
  only after phase 0.5 mounts `/boot`. With an embedded config the loader
  refuses with "driver images require bootstrap mode". The config therefore
  sets `boot.nmbl.configLocation = "external"` and
  `boot.nmbl.bootstrap.bootFs.device`.
- `loop` and `squashfs` must load early. The driver-image loader loop-mounts
  the squashfs without loading these modules itself, so the config sets
  `boot.nmbl.earlyKernelModules` to include them. A missing `loop` shows as
  "loop-alloc failed: opening /dev/loop-control".

`lib/install-bootloader.nix` defers the driver-image `nmbl-sign` step under
`deferInstallSigning`, like the UKI and generation signing, so a disko or
sealed image build without `imageKeyFile` can build.

## Tamper and rescue scenarios

The disk-preparation negatives and the sentinel scenario use
`testing/assertions/sb-disk-tamper-refused.sh`, selected by `NMBL_SB_TAMPER`.
#3b is an opt-in third phase of `sb-tpm-roundtrip.sh`
(`NMBL_SB_TPM_PHASE3=1`).

| id | app | config | disk preparation | pass requires |
|---|---|---|---|---|
| #4c | `test-secure-boot-wrong-key-refused` | `-enroll` | every kernel and initrd sidecar replaced by a valid signature from a fresh key that NMBL does not have | refuse, the generation does not boot, no emergency shell |
| #4d | `test-secure-boot-domain-transplant-refused` | `-enroll` | the kernel sidecar replaced by the baked key's signature over the same kernel bytes under the `driver-image` domain | the same as #4c. Per-role domain separation rejects it. |
| #5a | `test-secure-boot-sentinel-rescue` | `-enroll` | an empty `/boot/nmbl/rescue` on the ESP | the sentinel is detected, rescue starts after `seal: lock PCR capped`, and the generation does not boot |
| #5b | `test-secure-boot-staged` | `-staged` | none. The install-signed priority file. | `signature VALID` from the post-unlock priority gate, and the staged boot completes |
| #5c | `test-secure-boot-bad-priority-refused` | `-staged` | the signed priority file (`/nmbl-staged/priority.signed`) on the priority volume inside LUKS is overwritten with cryptsetup in the guestfish appliance | NMBL's `SECURE BOOT: REFUSED` screen, no shell, the generation does not boot |
| #3b | `test-secure-boot-rescue-locks-tpm` | `-enroll`, then `test-secure-boot` | after the enroll and unseal phases, the sentinel is added and the tpm-unlock config boots a third time | the TPM unseal happens, then `seal: lock PCR capped` and `seal: closed TPM-unsealed mapper cryptroot`. In rescue, `/dev/mapper/cryptroot` is absent and `cryptsetup open --token-only` fails. |

## Fixes found by the matrix (2026-09-26)

Commits `95183a2` and `2e04c39` wired the last stubbed scenarios. The TPM
roundtrip was broken in three independent ways, all fixed in `2e04c39`:

1. The initramfs shipped the static cryptsetup, which is built with
   `--disable-external-tokens` and cannot load systemd's `systemd-tpm2` token
   plugin. Every `--token-only` open failed with "No usable token is
   available". A `luks-tpm` initramfs now ships a dynamic cryptsetup with the
   plugin directory compiled in, plus the plugin and its tpm2-tss closure.
2. The enroll step sealed to the booted system's live PCR 11, which already
   includes NMBL's handoff extension. The unseal runs before that extension,
   so the value did not recur. `nmbl-tpm-enroll --uki <UKI>` now predicts
   the unlock-time PCR 11 of the UKI that performs the unlock (only
   systemd-stub's measurement) with `systemd-measure`. The roundtrip seals to
   the tpm-unlock UKI's prediction.
3. The kexec drops the mapping NMBL opened, and NixOS stage 1 cannot unseal
   the token again because PCR 11 has moved. A `luks-tpm` volume now hands
   the unsealed token passphrase to stage 1 through `passToStage1`, as a
   `luks-password` volume does (`nmbl-tpm-passphrase` reads it right after
   the unseal).

#3b found that a rescue forced after phase 3b (the embedded-config sentinel
re-check) could not close the TPM-unsealed mapper, because the root
filesystem was still mounted on it. The seal failed and diverted to the
refuse reboot. The strict seal now lazily detaches every mount backed by the
mapper (matched by `major:minor` in `/proc/self/mountinfo`) before
`cryptsetup close`.

On a refuse, NMBL relocks password-unlocked volumes while it has the console,
so serial output does not show the relock. The policy unit tests pin its
order: cap, close TPM mappers, sentinel, relock.

#5a found that an embedded-config system did not read the rescue sentinel. It
looked at the literal `/boot/nmbl/rescue` before `/boot` was mounted, so the
"next boot enters rescue" step of a refuse did not happen. The sentinel now
resolves through the boot mountpoint, and NMBL checks it again after phase 3b
mounts `/boot`.
