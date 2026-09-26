# Secure-boot VM test matrix (#57 runner spec)

This is the manifest the **#57 Sonnet VM runner** consumes. Every secure-boot
scenario is listed with its app name, the exact assertion, the expected PASS
signal, and whether it is **FULLY WIRED** (an assertion script + app exist and
are ready to run) or **STUBBED** (described here; the harness still needs the
listed work). BUILD-ONLY artifacts here; the actual VM runs are #57's job.

All scenarios boot the **`test-secure-boot`** NixOS config
(`testing/build_configurations.nix`), which wires the whole chain:

* `boot.nmbl.signing.{enable=true, enforce=true, algorithm="ml-dsa-87",
  publicKeys=[insecure-test-ml-dsa-87.pub], generationKeyFile=<impure>}`
* `boot.nmbl.signing.uki.{enable=true, keyFile=<impure>, certFile=<impure>}`
  — the NMBL UKI is `sbsign`'d at install with the INSECURE-TEST `db` key so
  the enforcing firmware ACCEPTS it (audit F1).
* `boot.nmbl.tpm.{measure=true, requireTpm=true, pcrIndex=11}`
* `boot.nmbl.secureBoot.{enable=true, enforce=true, requireTpm=true}`
  (`priorityVolume.device=null` ⇒ no priority mount in the core flow)
* a luks-tpm `cryptroot` device (`unlock="tpm"`, `tpmPcrs=[11 7]`)
* `loader="efi-stub"` ⇒ NMBL boots as a UKI

Run under the **swtpm "tis" + SB-OVMF (smm=on)** seam via
`mkRunner { tpm="tis"; secureBoot=true; dbCert=<insecure-test-sb-db.crt>; }`.

**Firmware `db` enrollment (audit F1, load-bearing).** The three NMBL
scenarios boot under an ENFORCING Secure-Boot OVMF whose `db` VARS is the
Microsoft `OVMF_VARS.ms.fd` with the INSECURE-TEST `db` cert
(`testing/keys/insecure-test-sb-db.crt`) ADDITIONALLY enrolled (`virt-fw-vars
--add-db`). Because the NMBL UKI is `sbsign`'d at install with the matching key
(`insecure-test-sb-db.key`), the firmware launches it — NMBL actually runs.
Anything NOT signed by MS or this test cert is still refused, so the
`check-sb-unsigned-uki` smoke (which keeps the MS-ONLY `db`, `dbCert` unset)
still correctly proves firmware-refusal of an unsigned UKI. Net: the NMBL-
behaviour rows boot NMBL under real enforcing SB; the unsigned-UKI smoke still
proves the firmware enforces.

`requireTpm=true` is load-bearing for the negatives: a TPM-less VM aborts the
boot rather than degrading, so a negative can never false-green on a box
without `/dev/tpmrm0`.

## How the test disk is signed (AT INSTALL RUNTIME — no key in any derivation)

HARD PROJECT PRINCIPLE: a signing PRIVATE key must NEVER be an input to a Nix
derivation (a derivation's inputs land in the world-readable `/nix/store`). The
secure-boot test disk is therefore signed AT INSTALL RUNTIME by NMBL's normal
install-time path-based code — exactly like production — NOT by a build-time
derivation that store-imports the keys.

The `test-secure-boot` config already declares its signing keys as on-disk
PATHs, not Nix path literals:

* `signing.generationKeyFile = "/var/lib/nmbl-test-keys/insecure-test-gen.key"`
* `signing.uki.keyFile = "/var/lib/nmbl-test-keys/insecure-test-sb-db.key"`
* `signing.uki.certFile = "/var/lib/nmbl-test-keys/insecure-test-sb-db.crt"`

The signed disk is produced by the RUNTIME orchestrator
`.#sb-install-test-secure-boot` (`testing/sb-install.nix`), which mirrors the
production `install-test-*` nixos-anywhere flow:

1. Boots a SystemRescue VM with a fresh 16G disk.
2. Runs `nixos-anywhere --phases kexec,disko` (kexec into the installer, lay
   out the disko LUKS layout) against the **install variant** of the config
   (`boot.nmbl.signing.deferInstallSigning = lib.mkForce false`, so in-installer
   signing actually runs).
3. `scp`s the committed test keys into the freshly-installed root fs (mounted at
   `/mnt` after disko) at
   `/mnt/var/lib/nmbl-test-keys/insecure-test-{gen.key,sb-db.key,sb-db.crt}` —
   read from a RUNTIME directory (`--keys-dir`, default `$NMBL_TEST_KEYS_DIR`
   else `$PWD/testing/keys`), never imported into a derivation. `/var/lib` (not
   `/run`) because the install phase's `installBootLoader` runs inside the
   `nixos-install` chroot, whose activation mounts a fresh tmpfs over `/run`
   right before signing — a `/run`-staged key would be shadowed and unreadable.
4. Runs `nixos-anywhere --phases install`. NMBL's `installBootLoader` runs in
   the install chroot where the `/var/lib/nmbl-test-keys/...` paths now exist:
   `lib/install-signing.nix` `sbsign`s the NMBL UKI with the staged `db`
   key/cert and writes
   `EFI/BOOT/BOOTX64.EFI`; `lib/install-gen-signing.nix` signs each generation's
   kernel/initrd with the staged ML-DSA key (per-role `gen-kernel`/`gen-initrd`)
   into `/nmbl/sigs/<gen-id>/{kernel,initrd}.sig`. All from PATHS, at runtime.
5. Leaves the SIGNED disk at `$WORK_DIR/disk1.qcow2`.

The booted disk is thus signed by the real install-time path-based code, and NO
signing key is in any derivation closure. The closure guard
`.#checks.x86_64-linux.test-secure-boot-no-private-key` (mirroring the prod
`insecure-test-key-absent` guard) asserts the install `--store-paths`
(diskoScript + toplevel) reference NEITHER the ML-DSA generation key NOR the SB
`db` private key.

## Runner prerequisites (set by the flake apps, but listed for #57)

Each scenario app FIRST runs the install orchestrator to produce the signed
disk, then boots it. The apps require an SSH key for nixos-anywhere:

* `NMBL_SSH_KEY` (or `SSH_PRIVATE_KEY`, or `--ssh-key`) — a passphrase-less SSH
  PRIVATE key file; nixos-anywhere needs it for its bootstrap. Pass it through
  to the orchestrator (the scenario apps forward the environment).
* `NMBL_TEST_KEYS_DIR` (optional) — directory holding the committed install-time
  signing keys (`insecure-test-gen.key` or `insecure-test-ml-dsa-87.key`, plus
  `insecure-test-sb-db.{key,crt}`). Defaults to `$PWD/testing/keys` (run the app
  from the `sirati-nmbl` checkout, or set this). These are read by PATH at
  install time and are NEVER a derivation input.
* `$NMBL_RUNNER` / `$NMBL_ENROLL_RUNNER` — exported by each app to the
  per-scenario runner. `$NMBL_DISK_IMAGE` is exported to the
  install-runtime-SIGNED `disk1.qcow2`, so the runner boots THAT disk.
* `$NMBL_SB_DISK` — exported by the bad-sig app to the same signed disk; the
  bad-sig script tampers a copy (removing a signed `initrd.sig` sidecar).
* `$NMBL_SB_TPM_UKI` — for the roundtrip, the real config's INSTALL-SIGNED UKI,
  extracted from the installed disk's ESP (no host-side `sbsign` derivation).

To pre-stage the signed disk by hand:

    nix run .#sb-install-test-secure-boot -- --ssh-key ~/.ssh/id_ed25519
    # → leaves $PWD/.sb-install-test-secure-boot/disk1.qcow2 (signed)

The scenario apps run this for you; set `NMBL_SB_SIGNED_DISK` /
`NMBL_SB_ENROLL_DISK` to reuse an already-produced disk and skip the install.

## CORE scenarios — FULLY WIRED

| id | app | scenario | exact assertion | expected PASS signal |
|---|---|---|---|---|
| #3a-pre | `test-secure-boot-tpm-roundtrip` | TPM seal/unseal roundtrip | **Precondition**: `/dev/tpmrm0` present + measured boot (PCR 11 extended) — reaching the measured path under `requireTpm=true` proves a real TPM. Then the TPM-sealed `cryptroot` AUTO-unseals (NO password answered) and the system reaches the post-kexec root shell. | `assertions/sb-tpm-roundtrip.sh` exits 0: TPM-present marker seen, auto-unseal marker seen (NOT the password modal), `root@test-secure-boot` shell reached and interactive. |
| #4a | `test-secure-boot-signed-gen-happy` | signed generation boots | A correctly-signed generation verifies → measures → kexecs. NO refuse / reboot-into-rescue / signature-failure marker appears; the system reaches the booted root shell. | `assertions/sb-signed-gen-happy.sh` exits 0: no refusal marker in history, `root@test-secure-boot` shell reached and interactive. |
| #4b | `test-secure-boot-bad-sig-refused` | tampered sidecar refused (NEG) | An `initrd.sig` sidecar is REMOVED from the FAT32 boot partition before boot → verify fails → NMBL refuses and reboots into rescue. Assert (a) a refuse/rescue/signature-failure marker appears, (b) the bad generation NEVER boots (no `root@test-secure-boot`), (c) **NO emergency shell is offered** — assert the ABSENCE of the emergency-shell prompt markers (R-1/R-13/FIX-35). | `assertions/sb-bad-sig-refused.sh` exits 0: refuse marker present; booted-bad-gen marker ABSENT; emergency-shell markers ABSENT. |
| #1 | `test-secure-boot-driver-image` | driver-image load | A signed squashfs carrying `dummy` (a module NOT in the base initrd): single-fd verify ⇒ loop-mounted ⇒ `init_module` pre-init. The `test-secure-boot-driver` config opens cryptroot with the install passphrase so the boot reaches the post-kexec shell; NMBL emits `driver-image loaded: … dummy …` before the cpio-log freeze, so it lands in the post-kexec `nmbl-init` journal. (`/proc/modules` cannot prove it — kexec resets module state.) | `assertions/sb-driver-image.sh` exits 0: no refusal; `root@test-secure-boot-driver` shell reached+interactive; the `driver-image loaded` marker AND `dummy` present in the `nmbl-init` journal. |
| #1-NEG | `test-secure-boot-driver-image-bad-refused` | corrupt driver image refused (NEG) | The driver squashfs (`/boot/nmbl/driver-extra.sfs`) is CORRUPTED on the ESP before boot → single-fd verify fails → NMBL refuses (enforce: `imageload/verify.rs` → `policy::refuse_unsigned` → `RebootIntoRescue`, R-1; the image is NEVER mounted). The refuse fires BEFORE the LUKS modal/console (driver-image load precedes `open_console`). Assert (a) a refuse marker, (b) `driver-image loaded` ABSENT, (c) the gen never boots un-refused, (d) NO emergency shell. | `assertions/sb-driver-image-bad-refused.sh` exits 0: refuse marker present; `driver-image loaded` ABSENT; booted-gen ABSENT (un-refused); emergency-shell markers ABSENT. |
| #2 | `test-secure-boot-staged` | staged boot apply (FEATURE #2) | The inside-LUKS priority volume (`cryptroot`) carries a signed config fragment + staged image NMBL loads as a SECOND STAGE. cryptroot opens with the install passphrase → the POST-UNLOCK priority gate attests the volume → `apply_staged_boot` single-fd verifies the image (`--domain driver-image`) AND the fragment (`--domain staged-fragment`), transactionally merges the fragment (which adds ONE extra explicit kernel module the base never loads), re-runs the merged config's effects, then the system kexecs + boots. All three artifacts (priority file, image, fragment) are signed AT INSTALL RUNTIME by `nmbl-sign` from a PATH (no key in any derivation; `lib/staged-install.nix`). | `assertions/sb-staged.sh` exits 0: no refuse marker; `root@test-secure-boot-staged` shell reached + interactive; the `nmbl-init` journal carries the post-unlock gate `signature VALID`, `staged-boot: fragment applied`, and the staged `re-loading explicit kernel modules` markers. |

### Driver-image config prerequisites (learned wiring #1, both VM-verified GREEN)

A `boot.nmbl.driverImages`-enabled config has two non-obvious requirements the
`test-secure-boot-driver` config encodes (each was a real refuse caught in the VM):

* **Bootstrap (external) config is REQUIRED.** The loader resolves the boot-
  relative image path against the runtime boot mountpoint, which only exists once
  Phase 0.5 mounts `/boot` (bootstrap mode). In embedded-config mode the loader
  refuses with *"driver images require bootstrap mode"*. So set
  `boot.nmbl.configLocation = "external"` + `boot.nmbl.bootstrap.bootFs.device`.
* **`loop` + `squashfs` must be loaded EARLY.** The loader loop-mounts the
  squashfs but does NOT modprobe these itself, so they must be present before the
  driver-image phase: `boot.nmbl.earlyKernelModules = [ "loop" "squashfs" ]`. A
  missing `loop` surfaces as *"loop-alloc failed: opening /dev/loop-control"*.

A product gap fixed in passing: `lib/install-bootloader.nix` now defers the
driver-image `nmbl-sign` step under `deferInstallSigning` (like the UKI/gen
signing), so a disko/sealed image build — which lacks the impure `imageKeyFile` —
no longer fails trying to sign the driver squashfs.

### Wire-in note for the SB smoke precondition (already landed, F6a)

| id | app | scenario | exact assertion | expected PASS signal |
|---|---|---|---|---|
| SB | `check-sb-unsigned-uki` | firmware refuses an unsigned UKI | Boots a deliberately-UNSIGNED UKI under SB-OVMF (`smm=on`, db-enrolled). The firmware REFUSES it (Secure-Boot violation banner / UEFI shell) and NMBL NEVER runs. Distinguishes "firmware refused" (PASS) from "NMBL refused". | `assertions/sb-unsigned-uki.sh` exits 0: a SB-refusal banner appears AND no NMBL marker is present. This is the literal precondition for #29 — run it FIRST so the rest of the SB matrix cannot false-green on a non-enforcing firmware. |

## Formerly stubbed scenarios — now wired

All six share the core harness. The three disk-preparation negatives and the
sentinel scenario use `testing/assertions/sb-disk-tamper-refused.sh`, selected
by `NMBL_SB_TAMPER`; #3b is an opt-in third phase of `sb-tpm-roundtrip.sh`.

Running any scenario needs `NMBL_TEST_KEYS_DIR` (the committed test signing
keys) and `NMBL_SSH_KEY` (any throwaway passphrase-less key; the installer
authorises it only inside its own nixos-anywhere VM). Point
`NMBL_SB_SIGNED_DISK` / `NMBL_SB_ENROLL_DISK` at an existing install to skip
reinstalling.

| id | app | disk preparation | PASS requires |
|---|---|---|---|
| #4c | `test-secure-boot-wrong-key-refused` | every kernel/initrd sidecar replaced by a VALID signature from a fresh key that is not baked into NMBL | refuse; the generation never boots un-refused; no emergency shell |
| #4d | `test-secure-boot-domain-transplant-refused` | the kernel sidecar replaced by the BAKED key's signature over the same kernel bytes under the `driver-image` domain | same as #4c: per-role domain separation rejects it |
| #5a | `test-secure-boot-sentinel-rescue` | empty `/boot/nmbl/rescue` on the ESP | the sentinel is detected, rescue is entered after `seal: lock PCR capped`, and the generation never boots |
| #5b | `test-secure-boot-staged` (existing) | none; the install-signed priority file | `priority-gate (PostUnlock): signature VALID` and the staged boot completes |
| #5c | `test-secure-boot-bad-priority-refused` | the signed priority file on the inside-LUKS priority volume is overwritten (cryptsetup inside the guestfish appliance) | NMBL's `SECURE BOOT: REFUSED` screen; no shell; the generation never boots |
| #3b | `test-secure-boot-rescue-locks-tpm` | after the enrol and TPM-unseal phases, the sentinel is dropped and the tpm-unlock config boots a third time | the TPM unseal happens, then `seal: lock PCR capped` and `seal: closed TPM-unsealed mapper cryptroot`; in rescue `/dev/mapper/cryptroot` is absent and `cryptsetup open --token-only` fails |

**The TPM roundtrip was broken in three independent ways**, all fixed (the
roundtrip and #3b now pass):

1. The initramfs shipped the static cryptsetup, which is built with
   `--disable-external-tokens` and so can never load systemd's
   `systemd-tpm2` token plugin: every `--token-only` open failed with "No
   usable token is available", whatever the PCRs. A `luks-tpm` initramfs now
   ships a dynamic cryptsetup with the plugin directory compiled in, plus
   the plugin and its tpm2-tss closure.
2. The enroll step sealed to the booted system's live PCR 11, which already
   includes NMBL's handoff extension. The unseal runs before that extension,
   so the value never recurred. `nmbl-tpm-enroll --uki <UKI>` now predicts the
   unlock-time PCR 11 of the UKI that will perform the unlock (systemd-stub's
   measurement only) with `systemd-measure`, and the roundtrip seals to the
   tpm-unlock UKI's prediction. Which generation the enroll phase booted no
   longer matters.
3. The kexec drops the mapping NMBL opened, and NixOS stage 1 cannot unseal
   the token again because PCR 11 has moved. A `luks-tpm` volume now hands
   the unsealed token passphrase to stage 1 through `passToStage1`, like a
   `luks-password` volume does (`nmbl-tpm-passphrase` reads it right after
   the unseal).

#3b then found that a rescue forced after phase 3b (the embedded-config
sentinel re-check) could not close the TPM-unsealed mapper, because the root
filesystem was still mounted on it: the seal failed and diverted to the refuse
reboot. The strict seal now lazily detaches every mount backed by the mapper
(matched by `major:minor` in `/proc/self/mountinfo`) before `cryptsetup close`.

The relock of password-unlocked volumes on a refuse runs while NMBL holds the
console, so it is not visible on serial; its order (cap, close TPM mappers,
sentinel, relock) is pinned by the policy unit tests.

#5a found that an embedded-config system never read the rescue sentinel (it
looked at the literal `/boot/nmbl/rescue` before `/boot` was mounted), so a
refuse's "next boot enters rescue" did not happen. The sentinel now resolves
through the boot mountpoint and is re-checked after phase 3b mounts `/boot`.

## Status summary

* **Wired:** `test-secure-boot-tpm-roundtrip`, `test-secure-boot-signed-gen-happy`,
  `test-secure-boot-bad-sig-refused`, `test-secure-boot-driver-image` +
  `test-secure-boot-driver-image-bad-refused`, `test-secure-boot-staged`,
  `check-sb-unsigned-uki`, and the six scenarios above.
