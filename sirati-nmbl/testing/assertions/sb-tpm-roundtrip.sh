#!/usr/bin/env bash
# Secure-boot scenario: TPM seal/unseal ROUNDTRIP (#57 / matrix id #3a).
#
# This is a TRUE measured-boot roundtrip across a power-cycle, not a single
# boot. NMBL's `luks-tpm` activation runs `cryptsetup open --token-only` and
# CANNOT fall back to a passphrase, so the cryptroot only auto-unseals once a
# LUKS2 `systemd-tpm2` token has been SEALED to the live TPM's measured PCRs.
# A freshly-installed disk ships only a passphrase keyslot — nothing is sealed
# yet — so a naive single boot can never produce the unseal marker. We therefore
# drive two phases against ONE persistent swtpm:
#
#   ── Phase 1 (ENROLL) ──────────────────────────────────────────────────────
#   Boot a passphrase-unlocking twin of the config (NMBL_ENROLL_RUNNER) whose
#   cryptroot opens with the install passphrase. It boots the SAME generation
#   (same kernel/initrd/cmdline ⇒ same PCR-11 event sequence) as the real
#   config, reaches the post-kexec system, and runs `nmbl-tpm-enroll` there.
#   That seals a random volume key to the TPM under the CURRENT measured PCRs
#   (11+7) and writes a `systemd-tpm2` token into the LUKS2 header on the disk.
#   We then STOP the VM — the runner ran with --tpm-persist, so the swtpm state
#   directory (the sealed object + hierarchy/NV) is KEPT, only QEMU+swtpm die.
#
#   ── Phase 2 (UNSEAL) ──────────────────────────────────────────────────────
#   Boot the real tpm-unlocking config (NMBL_RUNNER) against the SAME disk and
#   the SAME persisted swtpm state dir. A fresh QEMU power-on issues
#   TPM2_Startup(CLEAR): the PCRs RESET to their reset value and NMBL extends
#   the SAME deterministic PCR-11 sequence it extended in phase 1, so the final
#   PCR-11 value matches the value the token was sealed against. PCR 7 (the SB
#   state) is identical too (same firmware, same db, same UKI). cryptsetup's
#   token handler unseals the key and `cryptsetup open --token-only` succeeds:
#   NMBL emits `luks-tpm: unsealed … via TPM token` with NO passphrase prompt.
#
# WHY A POWER-CYCLE, NOT A WARM REBOOT: PCRs only reset on a TPM2_Startup(CLEAR)
# (a power-on), never on a warm reboot. A warm reboot would re-extend PCR 11 ON
# TOP of the already-extended value (double-extend) and the seal policy would no
# longer match. Phase 2 is a brand-new QEMU process (true power-cycle) against
# the persisted swtpm, so the PCRs are reproducible.
#
# WHY THE SAME swtpm STATE DIR: the sealed object lives in the swtpm's
# non-volatile state. --tpm-persist tells vm-serial-man to keep "$WORK_DIR/
# swtpm-state" between the two runner invocations so phase 2 can unseal what
# phase 1 sealed. This script owns deleting it at cleanup.
#
# PRECONDITION (so a TPM-less VM can NEVER false-green): requireTpm=true means a
# missing /dev/tpmrm0 aborts the boot; reaching the measured path proves the TPM
# is wired. The happy path additionally asserts NO password prompt, so a
# passphrase fallback can never be mistaken for an unseal.
#
# Required on PATH: vm-serial-man, the runners (NMBL_RUNNER + NMBL_ENROLL_RUNNER,
# or `run-test-secure-boot` / `run-test-secure-boot-enroll`), screen, coreutils,
# gnugrep, qemu, OVMFFull, swtpm. Exit 0 on success, 1 on any failure.

set -uo pipefail

CONFIG_NAME="test-secure-boot"
ENROLL_CONFIG_NAME="test-secure-boot-enroll"

# NMBL markers proving the TPM is present and the measured path ran. Any one
# of these means /dev/tpmrm0 was found and NMBL talked to it.
TPM_PRESENT_RE='tpmrm0|TPM present|measured boot|PCR 11|PCR-11|extend.*PCR|seal'
# Auto-unseal SUCCESS: a TPM-token-SPECIFIC marker NMBL emits ONLY on a genuine
# `cryptsetup open --token-only` unseal (src/activation/mod.rs). `--token-only`
# can NEVER fall back to a password keyslot, so this line proves the seal/unseal
# roundtrip succeeded — it shares no substring with the generic "activation
# luks-tpm completed" line, so a password fallback can never match it.
UNSEAL_OK_RE='luks-tpm: unsealed .* via TPM token'
# A password PROMPT means auto-unseal FAILED (fell back to the modal). Seeing
# it on phase 2 is a FAIL for the happy path; we assert its ABSENCE there.
PASSWORD_PROMPT_RE='Enter LUKS passphrase|passphrase for cryptroot'
# The enroll tool's success marker (lib/tpm-enroll.nix): a systemd-tpm2 token
# is now present in the header. Phase 1 must reach this before we power-cycle.
ENROLL_OK_RE='a systemd-tpm2 token is present|success — a systemd-tpm2 token'

SHELL_TIMEOUT="${NMBL_SHELL_TIMEOUT:-240}"
BOOT_WATCH="${NMBL_BOOT_WATCH:-120}"
ENROLL_WATCH="${NMBL_ENROLL_WATCH:-180}"

# The LUKS device the post-kexec enroll seals. Matches the disko layout
# (disk-main-luks = /dev/vda3 wrapped LUKS2) declared in the test config.
LUKS_DEV="${NMBL_LUKS_DEV:-/dev/disk/by-partlabel/disk-main-luks}"
ENROLL_PCRS="${NMBL_ENROLL_PCRS:-11+7}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "${SCRIPT_DIR}/lib.sh"

# ONE shared work directory across both phases so the persisted swtpm-state dir
# (under $WORK_DIR) carries the sealed object from phase 1 into phase 2. The
# runners cd into their own per-run dir, but we pass NMBL_TEST_WORKDIR so both
# point the swtpm state at the same place.
RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/nmbl-sb-tpm.XXXXXX")"

# Shared, persisted swtpm state directory for BOTH phases. Exporting
# NMBL_SWTPM_STATE makes every runner invocation point its `--tpm` at this one
# dir (instead of its own per-run $WORK_DIR/swtpm-state), so the token phase 1
# seals survives into phase 2's power-on. Combined with --tpm-persist on both
# runners, the dir is kept across the stop between phases. This script removes
# it at cleanup (rm -rf "$RUN_DIR").
export NMBL_SWTPM_STATE="${RUN_DIR}/swtpm-state-shared"

stop_vm() {
  vm-serial-man stop >/dev/null 2>&1 || true
  for s in "${CONFIG_NAME}" "${ENROLL_CONFIG_NAME}"; do
    if screen -ls 2>/dev/null | grep -q "\.${s}\b"; then
      screen -S "${s}" -X quit >/dev/null 2>&1 || true
    fi
  done
}

cleanup() {
  local rc=$?
  echo "=== cleanup ===" >&2
  stop_vm
  rm -rf "${RUN_DIR}" >/dev/null 2>&1 || true
  exit "$rc"
}
trap cleanup EXIT INT TERM

RUNNER="${NMBL_RUNNER:-run-${CONFIG_NAME}}"
ENROLL_RUNNER="${NMBL_ENROLL_RUNNER:-run-${ENROLL_CONFIG_NAME}}"
for r in "$RUNNER" "$ENROLL_RUNNER"; do
  if ! command -v "$r" >/dev/null 2>&1 && [ ! -x "$r" ]; then
    echo "FAIL: runner '${r}' not found on PATH" >&2
    echo "      The roundtrip needs BOTH NMBL_RUNNER (the tpm-unlock config) and" >&2
    echo "      NMBL_ENROLL_RUNNER (a passphrase-unlock twin that boots the SAME" >&2
    echo "      generation so phase 1 can run nmbl-tpm-enroll in the booted system)." >&2
    exit 1
  fi
done

# ── Phase 1: ENROLL ────────────────────────────────────────────────────────
echo "=== Phase 1: enrolling a TPM-sealed token (passphrase-unlock boot) ===" >&2
cd "${RUN_DIR}" || { echo "FAIL: could not cd into ${RUN_DIR}" >&2; exit 1; }
if ! "$ENROLL_RUNNER"; then
  echo "FAIL: enroll runner exited non-zero" >&2
  exit 1
fi

# NMBL's stage-1 luks-password activation shows a passphrase modal; type the
# fixed install passphrase ("test") so the enroll twin unlocks cryptroot and
# kexecs. Retry a few times so we don't race the modal's appearance. The
# system initrd then unlocks from the injected key (passToStage1), not a
# prompt, so this is the only passphrase entry on the enroll boot.
echo "=== feeding the enroll-boot LUKS passphrase ===" >&2
if wait_for "$PASSWORD_PROMPT_RE" 90; then
  for _ in 1 2 3; do
    send_cmd "test"
    if wait_for "$TPM_PRESENT_RE|kexec|root@" 20; then
      break
    fi
  done
fi

# The passphrase-unlock twin must reach the booted system so we can enroll.
echo "=== waiting up to ${SHELL_TIMEOUT}s for the enroll system shell ===" >&2
booted=false
for _ in $(seq 1 "$((SHELL_TIMEOUT / 5))"); do
  if seen_in_history "root@${ENROLL_CONFIG_NAME}|root@${CONFIG_NAME}"; then
    booted=true
    break
  fi
  # Re-feed the passphrase if the modal is still up (slow appearance).
  if seen_in_history "$PASSWORD_PROMPT_RE" && ! seen_in_history "kexec|root@"; then
    send_cmd "test"
  fi
  sleep 5
done
if [ "$booted" != true ]; then
  echo "FAIL: enroll boot never reached the system shell (cannot enroll)" >&2
  exit 1
fi

# Make the shell interactive, then seal the volume key to the CURRENT PCRs.
send_cmd "echo NMBL_ENROLL_SHELL_READY"
if ! wait_for "NMBL_ENROLL_SHELL_READY" 20; then
  echo "FAIL: enroll system shell never became interactive" >&2
  exit 1
fi
echo "=== running nmbl-tpm-enroll (seal LUKS key to PCRs ${ENROLL_PCRS}) ===" >&2
# PASSWORD env feeds the existing passphrase non-interactively to
# systemd-cryptenroll (the install passphrase is the fixed test value "test").
send_cmd "PASSWORD=test nmbl-tpm-enroll --device ${LUKS_DEV} --pcrs ${ENROLL_PCRS}"
if ! wait_for "$ENROLL_OK_RE" "$ENROLL_WATCH"; then
  echo "FAIL: nmbl-tpm-enroll did not report a sealed systemd-tpm2 token" >&2
  exit 1
fi
echo "=== PASS: token sealed to the TPM; powering down for the power-cycle ===" >&2

# Controlled power-cycle: STOP this VM (kills QEMU + swtpm) but KEEP the
# persisted swtpm-state dir so phase 2's fresh power-on reloads the sealed
# object. The runner ran with --tpm-persist precisely so the dir survives.
stop_vm

# ── Disk handoff: share ONE disk across both phases ─────────────────────────
# The token the enroll phase sealed lives in the LUKS2 header on the enroll
# disk's vda3. Phase 2 must boot THAT disk (so the token is present), but with
# the tpm-unlock NMBL stage. For an efi-stub loader the NMBL config (password
# vs token unlock) is the embedded initrd inside the UKI on the ESP, so we swap
# ONLY /EFI/BOOT/BOOTX64.EFI for the real tpm-unlock UKI — vda3 (and its token)
# is untouched. We hand the result to the real runner as its own disk name so
# its "use existing disk" branch reuses the enrolled+swapped image.
ENROLL_DISK="${RUN_DIR}/${ENROLL_CONFIG_NAME}.qcow2"
SHARED_DISK="${RUN_DIR}/${CONFIG_NAME}.qcow2"
if [ ! -f "$ENROLL_DISK" ]; then
  echo "FAIL: enroll disk ${ENROLL_DISK} not found after phase 1" >&2
  exit 1
fi
if [ -z "${NMBL_SB_TPM_UKI:-}" ] || [ ! -f "${NMBL_SB_TPM_UKI}" ]; then
  echo "FAIL: NMBL_SB_TPM_UKI (the real tpm-unlock signed UKI) is unset/missing" >&2
  echo "      — cannot swap the ESP stage for phase 2." >&2
  exit 1
fi
echo "=== swapping the ESP UKI to the tpm-unlock stage (token on vda3 kept) ===" >&2
mv -f "$ENROLL_DISK" "$SHARED_DISK"
export LIBGUESTFS_BACKEND=direct
if ! guestfish --rw -a "$SHARED_DISK" <<EOF
run
mount /dev/sda2 /
mkdir-p /EFI/BOOT
upload ${NMBL_SB_TPM_UKI} /EFI/BOOT/BOOTX64.EFI
umount /
EOF
then
  echo "FAIL: could not swap the ESP UKI onto the shared disk" >&2
  exit 1
fi

# ── Phase 2: UNSEAL ────────────────────────────────────────────────────────
# The real runner copies its disk only if absent; SHARED_DISK already exists at
# the runner's diskName ($CONFIG_NAME.qcow2 in $RUN_DIR), so it reuses the
# enrolled+swapped image. Same persisted swtpm (NMBL_SWTPM_STATE) ⇒ a fresh
# power-on resets PCRs and NMBL re-extends the same sequence the seal was bound
# to. (Phase 2 runs in the same $RUN_DIR cwd as phase 1.)
echo "=== Phase 2: power-cycling into the tpm-unlock config (same swtpm) ===" >&2
if ! "$RUNNER"; then
  echo "FAIL: unseal runner exited non-zero" >&2
  exit 1
fi

# PRECONDITION: TPM present + measured path ran (requireTpm=true ⇒ a TPM-less
# VM would have aborted before here).
echo "=== asserting /dev/tpmrm0 present + measured boot (PCR 11) ===" >&2
if ! wait_for "$TPM_PRESENT_RE" "$BOOT_WATCH"; then
  echo "FAIL: NMBL never showed a TPM-present / measured-boot marker" >&2
  echo "      (requireTpm=true ⇒ a TPM-less VM aborts here — precondition unmet)" >&2
  exit 1
fi
echo "=== PASS precondition: TPM present + measured boot ===" >&2

# The TPM-sealed cryptroot must AUTO-unseal: NMBL emits the token-specific
# marker with no passphrase answered. A bare password prompt means the
# seal/unseal roundtrip failed (PCRs didn't match, or no token was sealed).
echo "=== asserting TPM auto-unseal of cryptroot (no password) ===" >&2
if ! wait_for "$UNSEAL_OK_RE" "$BOOT_WATCH"; then
  if seen_in_history "$PASSWORD_PROMPT_RE"; then
    echo "FAIL: cryptroot fell back to the password modal — TPM auto-unseal FAILED" >&2
  else
    echo "FAIL: no TPM auto-unseal marker within ${BOOT_WATCH}s" >&2
  fi
  exit 1
fi
# Belt-and-braces: even WITH the unseal marker, a password prompt anywhere in
# the history means the seal degraded to a fallback — fail the happy path.
if seen_in_history "$PASSWORD_PROMPT_RE"; then
  echo "FAIL: a password prompt appeared despite the unseal marker — the TPM" >&2
  echo "      auto-unseal is not clean (a fallback path was exercised)." >&2
  exit 1
fi
echo "=== PASS: cryptroot auto-unsealed from the TPM (no password prompt) ===" >&2

# Finally the box must reach the booted root shell — proving the measured boot
# matched the seal policy end-to-end and the system came up unattended.
echo "=== waiting up to ${SHELL_TIMEOUT}s for the post-kexec root shell ===" >&2
booted=false
for _ in $(seq 1 "$((SHELL_TIMEOUT / 5))"); do
  if seen_in_history "root@${CONFIG_NAME}"; then
    booted=true
    break
  fi
  sleep 5
done
if [ "$booted" != true ]; then
  echo "FAIL: never reached the booted root shell after TPM auto-unseal" >&2
  exit 1
fi

echo "PASS: TPM seal/unseal roundtrip — enroll, power-cycle, auto-unseal, up." >&2
exit 0
