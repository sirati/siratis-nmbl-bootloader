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

### Wire-in note for the SB smoke precondition (already landed, F6a)

| id | app | scenario | exact assertion | expected PASS signal |
|---|---|---|---|---|
| SB | `check-sb-unsigned-uki` | firmware refuses an unsigned UKI | Boots a deliberately-UNSIGNED UKI under SB-OVMF (`smm=on`, db-enrolled). The firmware REFUSES it (Secure-Boot violation banner / UEFI shell) and NMBL NEVER runs. Distinguishes "firmware refused" (PASS) from "NMBL refused". | `assertions/sb-unsigned-uki.sh` exits 0: a SB-refusal banner appears AND no NMBL marker is present. This is the literal precondition for #29 — run it FIRST so the rest of the SB matrix cannot false-green on a non-enforcing firmware. |

## NEXT scenarios — STUBBED (described; harness work pending)

Each row says exactly what must be wired. These were left as precise stubs
because they need either a Rust/boot-flow feature seam that this F6b task does
not own, or disk/priority-volume preparation beyond the core chain.

| id | proposed app | scenario | exact assertion (target) | what's needed to wire it (TODO) |
|---|---|---|---|---|
| #4c | `test-secure-boot-wrong-key-refused` | wrong-key generation refused (NEG) | A generation signed by a NON-baked key ⇒ any-of verify fails ⇒ refused; bad gen never boots; no shell. | STUBBED. Needs a SECOND insecure keypair (sign the gen sidecars with a key whose public half is NOT baked) staged onto the disk's `/boot/nmbl/sigs/<gen-id>/`. Re-uses the `sb-bad-sig-refused.sh` assertion shape verbatim; only the disk-prep differs (re-sign with a foreign key instead of deleting the sidecar). Add `testing/keys/insecure-test-ml-dsa-87-foreign.{key,pub}` + a re-sign step. |
| #4d | `test-secure-boot-domain-transplant-refused` | domain-transplant refused (NEG, FIX-01) | A valid `driver-image`-domain signature presented as the `gen-kernel` sidecar ⇒ rejected (per-role domain separation). | STUBBED. Needs `nmbl-sign --domain driver-image` over the gen kernel, dropped at the `kernel.sig` path. Disk-prep only; assertion = `sb-bad-sig-refused.sh` shape (refuse + no boot + no shell). |
| #3b | `test-secure-boot-rescue-locks-tpm` | rescue caps TPM, mapper GONE (NEG, FIX-03) | Force a drop to rescue AFTER a post-LUKS-unlock failure. Assert (precondition) `/dev/tpmrm0` exists + a probe secret seals; then PCR 11 is CAPPED before the prompt; the `cryptroot` **mapper node is GONE** (`/dev/mapper/cryptroot` ABSENT); a post-cap unseal FAILS. Assert ABSENCE (mapper gone, unseal fails), not a banner. | STUBBED. Needs a deterministic way to force the post-unlock failure + rescue (a fault-injection knob or a config that unlocks then fails a later activation), and a rescue-shell probe that runs `ls /dev/mapper/cryptroot` (expect absent) and a TPM-unseal probe (expect FAIL). The seal/cap behaviour is #17/#26 Rust work; the assertion can be written once a force-rescue-after-unlock path is reachable. Assertion skeleton: precondition like `sb-tpm-roundtrip.sh` (seal probe) + absence sweep for `/dev/mapper/cryptroot` + an unseal-probe that must error. |
| #5a | `test-secure-boot-sentinel-rescue` | sentinel ⇒ rescue, stays capped | An empty `/boot/nmbl/rescue` sentinel forces a rescue boot; NMBL refuses the measured boot, keeps the TPM capped, goes straight to rescue. | STUBBED. Disk-prep: `touch /boot/nmbl/rescue` on the ESP (mtools/guestfish, no LUKS key needed). Assertion: refuse marker + rescue reached + (probe) TPM stays capped. Needs the sentinel→straight-to-rescue path (#30) live; assertion = refuse-shape + a capped-PCR probe. |
| #5b | `test-secure-boot-priority-ok` | priority signed-file OK | A signed priority file on the first LUKS/LVM volume verifies ⇒ proceeds to measured boot. | STUBBED. Needs a config variant with `secureBoot.priorityVolume.device` set + a signed `priority.signed` file staged on that volume (sign with `nmbl-sign --domain priority-file`). Then assertion = signed-gen-happy shape (proceeds, boots). Priority-gate is #31. |
| #5c | `test-secure-boot-bad-priority-refused` | bad/missing priority (NEG) | Priority file missing/bad with boot-FS ⊆ priority LUKS variant ⇒ cap FIRST + close mappers + sentinel persists to next boot; refuse boot AND shell; only `RebootIntoRescue`. Assert **NO shell** + `/dev/mapper/<x>` ABSENT + unseal FAILS. | STUBBED. Needs the priority-volume config variant (as #5b) but with the signed file removed/tampered, plus the rescue-shell mapper/unseal probes from #3b. Assertion = bad-sig refuse-shape + mapper-absent + unseal-fail + sentinel-persists-across-reboot check. Priority-gate is #31; the relock/close-mappers is #26/#17. |
| #2 | `test-secure-boot-staged` | staged boot apply | A priority volume carries a signed fragment + drivers: transactional merge honored ⇒ reaches kexec. | STUBBED. Needs `boot.nmbl.staged.*` + `secureBoot.enable` (already on) + a signed `fragment.toml` + image on the priority volume. Assertion: a marker proving the merged fragment took effect (e.g. an injected kernel param / extra module from the fragment) then boot. Staged apply is #33 (depends on #31 priority-gate via `AttestedVolume`). |

## Status summary

* **FULLY WIRED (ready for #57 to run):**
  `test-secure-boot-tpm-roundtrip`, `test-secure-boot-signed-gen-happy`,
  `test-secure-boot-bad-sig-refused`, `test-secure-boot-driver-image` (#1) +
  its `test-secure-boot-driver-image-bad-refused` negative, plus the
  already-landed `check-sb-unsigned-uki` precondition.
* **STUBBED (precise TODOs above):** wrong-key (#4c), domain-transplant (#4d),
  rescue-locks-tpm / mapper-gone (#3b), sentinel-rescue (#5a),
  priority-ok (#5b), bad-priority (#5c), staged-boot (#2).

The STUBBED rows are deferred because they need either a second/foreign test
key, a priority-volume config + staged signed file, a driver/staged image, or a
reachable force-rescue-after-unlock seam — none of which are part of the CORE
chain this harness wires. Each row above names the exact missing piece so the
next pass can land it incrementally; the assertion SHAPES are already proven by
the three wired scripts (`sb-signed-gen-happy.sh` for happy paths,
`sb-bad-sig-refused.sh` for refuse/absence negatives, `sb-tpm-roundtrip.sh` for
TPM seal/unseal probes).
