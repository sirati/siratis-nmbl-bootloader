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

## How the test disk is signed (read this — the keys are NOT staged at build)

The `test-secure-boot` **disk image** is produced by disko/`make-disk-image`,
which runs `nixos-install` (and thus the NMBL `installBootLoader`) inside a
SEALED build VM whose only filesystem is the Nix store. The install-time-impure
signing keys (`generationKeyFile`, `signing.uki.{keyFile,certFile}`) DO NOT
EXIST inside that VM, so `nmbl-sign`/`sbsign` cannot read them — staging them on
the host (`/run/nmbl-test-keys/…` or `$NMBL_GEN_KEY_FILE`) does nothing for the
build VM, and attempting to sign there KILLS the build (init `exit_group(1)` →
kernel panic).

The image build therefore DEFERS in-installer signing
(`boot.nmbl.signing.deferInstallSigning = true`, set automatically for the disko
path in `testing/vm-config.nix`): it installs the UNSIGNED UKI and writes NO
generation sidecars, but the runtime POLICY is untouched — the baked trust
anchor (`publicKeys`) and `config.toml`'s `[signing].enable/enforce` still make
the booted NMBL ENFORCE signatures.

The signing is then finished HOST-SIDE by `flake.nix`'s
`secureBootSignedDisk` derivation (`nix build .#test-secure-boot-disk`), where
the committed INSECURE-TEST keys are available: it `sbsign`s the NMBL UKI with
`insecure-test-sb-db.{key,crt}` and signs each generation's kernel/initrd with
`insecure-test-ml-dsa-87.key` (per-role `gen-kernel`/`gen-initrd` domains),
writing `EFI/BOOT/BOOTX64.EFI` and `/nmbl/sigs/<gen-id>/{kernel,initrd}.sig`
onto the disk's UNENCRYPTED ESP. The `gen-id` is the content-addressed store
basename of the system toplevel — the SAME id `nmbl-init` computes at boot.

The store-imported keys are fine HERE because this is a TEST disk, not a
production NMBL closure; the production closure-leak guard
(`nix build .#insecure-test-key-absent`) still holds for prod configs, and the
in-installer `lib/install-{signing,gen-signing}.nix` asserts still reject a
store-path key for any non-deferred (real) install.

## Runner prerequisites (set by the flake apps, but listed for #57)

* `$NMBL_RUNNER` — exported by each app to the per-scenario runner. The runner
  copies the HOST-SIGNED `vmDiskImage` (`secureBootSignedConfig`, i.e.
  `.#test-secure-boot-disk`) `nixos.qcow2`, so the booted disk already carries
  the sbsign'd UKI and the generation sidecars.
* **No key staging is needed.** `$NMBL_GEN_KEY_FILE` /
  `$NMBL_SB_DB_{KEY,CERT}_FILE` are still honoured by the config's
  `generationKeyFile`/`uki.{keyFile,certFile}` defaults for a REAL
  (non-deferred) install, but the disk-image build path ignores them (signing
  is deferred to the host-side step above). The build needs `--impure` only
  because those `getEnv` defaults are evaluated, not read.
* `$NMBL_SB_DISK` — exported by the bad-sig app to the HOST-SIGNED
  `secureBootSignedConfig` `vmDiskImage` `nixos.qcow2`; the bad-sig script
  tampers a copy of it (removing a signed `initrd.sig` sidecar).

## CORE scenarios — FULLY WIRED

| id | app | scenario | exact assertion | expected PASS signal |
|---|---|---|---|---|
| #3a-pre | `test-secure-boot-tpm-roundtrip` | TPM seal/unseal roundtrip | **Precondition**: `/dev/tpmrm0` present + measured boot (PCR 11 extended) — reaching the measured path under `requireTpm=true` proves a real TPM. Then the TPM-sealed `cryptroot` AUTO-unseals (NO password answered) and the system reaches the post-kexec root shell. | `assertions/sb-tpm-roundtrip.sh` exits 0: TPM-present marker seen, auto-unseal marker seen (NOT the password modal), `root@test-secure-boot` shell reached and interactive. |
| #4a | `test-secure-boot-signed-gen-happy` | signed generation boots | A correctly-signed generation verifies → measures → kexecs. NO refuse / reboot-into-rescue / signature-failure marker appears; the system reaches the booted root shell. | `assertions/sb-signed-gen-happy.sh` exits 0: no refusal marker in history, `root@test-secure-boot` shell reached and interactive. |
| #4b | `test-secure-boot-bad-sig-refused` | tampered sidecar refused (NEG) | An `initrd.sig` sidecar is REMOVED from the FAT32 boot partition before boot → verify fails → NMBL refuses and reboots into rescue. Assert (a) a refuse/rescue/signature-failure marker appears, (b) the bad generation NEVER boots (no `root@test-secure-boot`), (c) **NO emergency shell is offered** — assert the ABSENCE of the emergency-shell prompt markers (R-1/R-13/FIX-35). | `assertions/sb-bad-sig-refused.sh` exits 0: refuse marker present; booted-bad-gen marker ABSENT; emergency-shell markers ABSENT. |

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
| #1 | `test-secure-boot-driver-image` | driver-image load | A signed squashfs with an extra module: single-fd verify ⇒ loop-mounted ⇒ `init_module` pre-init ⇒ the module is in `/proc/modules` before kexec. | STUBBED. Needs a `boot.nmbl.driverImages` config variant + a signed driver squashfs staged on the ESP (`signImage`/`signTestArtifact --role driver-image`). Assertion: a unique extra module name appears in NMBL's pre-kexec `/proc/modules` log marker. Driver-image load is #23/#24. |
| #2 | `test-secure-boot-staged` | staged boot apply | A priority volume carries a signed fragment + drivers: transactional merge honored ⇒ reaches kexec. | STUBBED. Needs `boot.nmbl.staged.*` + `secureBoot.enable` (already on) + a signed `fragment.toml` + image on the priority volume. Assertion: a marker proving the merged fragment took effect (e.g. an injected kernel param / extra module from the fragment) then boot. Staged apply is #33 (depends on #31 priority-gate via `AttestedVolume`). |

## Status summary

* **FULLY WIRED (ready for #57 to run):**
  `test-secure-boot-tpm-roundtrip`, `test-secure-boot-signed-gen-happy`,
  `test-secure-boot-bad-sig-refused`, plus the already-landed
  `check-sb-unsigned-uki` precondition.
* **STUBBED (precise TODOs above):** wrong-key (#4c), domain-transplant (#4d),
  rescue-locks-tpm / mapper-gone (#3b), sentinel-rescue (#5a),
  priority-ok (#5b), bad-priority (#5c), driver-image (#1), staged-boot (#2).

The STUBBED rows are deferred because they need either a second/foreign test
key, a priority-volume config + staged signed file, a driver/staged image, or a
reachable force-rescue-after-unlock seam — none of which are part of the CORE
chain this harness wires. Each row above names the exact missing piece so the
next pass can land it incrementally; the assertion SHAPES are already proven by
the three wired scripts (`sb-signed-gen-happy.sh` for happy paths,
`sb-bad-sig-refused.sh` for refuse/absence negatives, `sb-tpm-roundtrip.sh` for
TPM seal/unseal probes).
