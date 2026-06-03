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
# missing /dev/tpmrm0 aborts the boot. We do NOT gate on a separate live-serial
# "TPM present / measured boot" marker — those (sb-state / PCR-11 extend) are
# emitted AFTER the kexec cpio-log freeze AND while NMBL holds the console, so
# they reach neither live serial nor the post-kexec journal. Instead the unseal
# success itself is the precondition: phase 2 types NOTHING, so reaching the
# shell can only happen if `--token-only` unsealed (no passphrase fallback
# exists), which proves the TPM is wired AND the seal policy matched the live
# PCRs — strictly more than mere presence. The journal unseal marker then
# confirms the mechanism.
#
# Required on PATH: vm-serial-man, the runners (NMBL_RUNNER + NMBL_ENROLL_RUNNER,
# or `run-test-secure-boot` / `run-test-secure-boot-enroll`), screen, coreutils,
# gnugrep, qemu, OVMFFull, swtpm. Exit 0 on success, 1 on any failure.

set -uo pipefail

CONFIG_NAME="test-secure-boot"
ENROLL_CONFIG_NAME="test-secure-boot-enroll"

# The booted system's journal tag NMBL's pre-kexec transcript is replayed under
# (lib/modules/log-import.nix, drained one line per entry via systemd-cat -t).
# Phase-2 unseal proof is read from HERE, not from live serial (see UNSEAL note).
JOURNAL_TAG="nmbl-init"
# Auto-unseal SUCCESS: a TPM-token-SPECIFIC marker NMBL emits ONLY on a genuine
# `cryptsetup open --token-only` unseal (src/activation/mod.rs:271, full text
# `luks-tpm: unsealed <name> via TPM token (cryptsetup --token-only)`).
# `--token-only` can NEVER fall back to a password keyslot, so this line proves
# the seal/unseal roundtrip succeeded — it shares no substring with the generic
# "activation luks-tpm completed" line, so a password fallback can never match
# it. We read it from the IMPORTED `nmbl-init` journal, NOT live serial: NMBL
# unseals during phase-3 storage activation while it still OWNS the TUI console
# (set_tui_active), which suppresses the `nmbl_*!` stderr branch — so the marker
# never reaches LIVE serial on this console-holding boot. But the unseal is
# emitted BEFORE the kexec cpio-log freeze (handoff.rs:321), so it IS carried
# into the next kernel's /nmbl-log/nmbl.log and recoverable post-kexec. The
# journal phrase is a metachar-free substring unique to the unseal line (the
# generic completed line lacks it), safe for assert_journal_tag's plain grep.
UNSEAL_OK_PHRASE="via TPM token"
# A password PROMPT means auto-unseal FAILED (fell back to the modal). The modal
# prompt is rendered by the terminus AFTER the console is released, so it DOES
# reach live serial — we assert its ABSENCE on serial on phase 2.
PASSWORD_PROMPT_RE='Enter LUKS passphrase|passphrase for cryptroot'
# The enroll tool's success marker (lib/tpm-enroll.nix): a systemd-tpm2 token
# is now present in the header. Phase 1 must reach this before we power-cycle.
# The enroll tool runs on the BOOTED system (NMBL no longer holds the console),
# so its output reaches serial — we keep this on serial.
ENROLL_OK_RE='a systemd-tpm2 token is present|success — a systemd-tpm2 token'
#
# WHY THERE IS NO SEPARATE "TPM present / measured boot" precondition wait:
# the old TPM_PRESENT_RE gated on `measured boot` / `PCR-11` / `extend PCR`
# markers (sbstate.rs:194,218 + measure.rs:198). Those are ALL emitted inside
# measure_handoff (handoff_load.rs:85 / handoff.rs:344) — AFTER the kexec
# cpio-log freeze (handoff.rs:321) AND while the console is held — so they are
# unrecoverable from BOTH live serial and the post-kexec journal. Rather than
# drop a security check, we rely on the strictly STRONGER unseal proof: the
# `via TPM token` journal marker proves NMBL opened /dev/tpmrm0, the seal policy
# matched the live measured PCRs, and `--token-only` unsealed the key — which
# subsumes "TPM present + measured path ran". requireTpm=true still guarantees a
# TPM-less VM aborts before the shell, so reaching the shell + this marker proves
# the TPM was wired with no coverage lost.

SHELL_TIMEOUT="${NMBL_SHELL_TIMEOUT:-240}"
ENROLL_WATCH="${NMBL_ENROLL_WATCH:-180}"

# The LUKS device the post-kexec enroll seals. Matches the disko layout
# (disk-main-luks = /dev/vda3 wrapped LUKS2) declared in the test config.
LUKS_DEV="${NMBL_LUKS_DEV:-/dev/disk/by-partlabel/disk-main-luks}"
ENROLL_PCRS="${NMBL_ENROLL_PCRS:-11+7}"

# The fixed install LUKS passphrase (disko-luks-password.nix). It answers NMBL
# stage-0's luks-password modal on the phase-1 enroll boot AND feeds
# systemd-cryptenroll the existing keyslot so the seal can be added.
ENROLL_PASSPHRASE="${NMBL_LUKS_PASSPHRASE:-test}"

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

# Best-effort dump of the VM's serial scrollback so a FAILED boot shows WHERE it
# stalled (firmware? NMBL verify? the passphrase modal? the post-kexec system?).
# Never let the dump itself error the script — the manager/socket may already be
# gone, in which case we simply note that and move on.
dump_serial_history() {
  echo "=== VM serial history (best-effort diagnostic) ===" >&2
  local -a sock_args
  _socket_args sock_args
  if vm-serial-man tail 400 "${sock_args[@]}" >&2 2>/dev/null; then
    :
  elif vm-serial-man lines 1 400 "${sock_args[@]}" >&2 2>/dev/null; then
    :
  else
    echo "(no VM serial history available — manager socket gone or empty)" >&2
  fi
  echo "=== end VM serial history ===" >&2
}

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
  if [ "$rc" -ne 0 ]; then
    dump_serial_history || true
  fi
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
# fixed install passphrase so the enroll twin unlocks cryptroot and kexecs. The
# system initrd then unlocks from the injected key (passToStage1), not a
# prompt, so this is the only passphrase entry on the enroll boot.
#
# WHY WE TYPE BLIND (no wait-for-prompt gate): NMBL's passphrase modal is a
# full-screen ratatui repaint that is NOT newline-terminated. vm-serial-man's
# history is built by `read_line` (splits on '\n'), so while NMBL sits on the
# modal awaiting input the un-terminated frame never flushes to the buffer —
# `wait_for`/`seen_in_history` for the modal text can hang/miss even though the
# box is on screen. So we do what an operator does: give the boot a head-start
# to reach the LUKS stage, then type the passphrase + Enter on a fixed cadence
# until the enroll system actually boots. `send_cmd` appends the newline, so
# each attempt submits. Extra Enters on an already-unlocked system are harmless
# (blank shell prompts). The post-kexec markers we wait on ARE newline-
# terminated and flush normally.
echo "=== feeding the enroll-boot LUKS passphrase (blind) ===" >&2
# A short head-start so the firmware → NMBL → activation path can paint the
# modal before the first keystroke (typing into a pre-modal console is a no-op).
sleep 10
# Informational only (never gates the typing): if the modal text DID flush to
# history, note it — helps a reader correlate the keystroke with the prompt.
if seen_in_history "$PASSWORD_PROMPT_RE"; then
  echo "=== luks-password modal text seen in history; answering ===" >&2
fi
send_cmd "$ENROLL_PASSPHRASE"

# The passphrase-unlock twin must reach the booted system so we can enroll.
# Re-type the passphrase periodically (BLIND — the modal may never flush, see
# above) in case the first attempt raced the modal's appearance.
echo "=== waiting up to ${SHELL_TIMEOUT}s for the enroll system shell ===" >&2
booted=false
i=0
for _ in $(seq 1 "$((SHELL_TIMEOUT / 5))"); do
  if seen_in_history "root@${ENROLL_CONFIG_NAME}|root@${CONFIG_NAME}"; then
    booted=true
    break
  fi
  # Re-feed the passphrase every ~15s until a kexec/boot marker appears, so a
  # slow/late modal still gets answered without gating on the unseeable frame.
  i=$((i + 1))
  if [ $((i % 3)) -eq 0 ] && ! seen_in_history "kexec|root@"; then
    send_cmd "$ENROLL_PASSPHRASE"
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
# systemd-cryptenroll (the fixed install passphrase, same as the modal answer).
send_cmd "PASSWORD=${ENROLL_PASSPHRASE} nmbl-tpm-enroll --device ${LUKS_DEV} --pcrs ${ENROLL_PCRS}"
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

# ── THE decisive proof: boot to the shell HAVING TYPED NOTHING. ─────────────
# Phase 2 is the tpm-unlock config: NMBL's `luks-tpm` activation runs
# `cryptsetup open --token-only` and CANNOT fall back to a passphrase keyslot.
# We deliberately send NO keystrokes here (unlike phase 1's blind-typed modal).
# So the ONLY way this VM reaches the post-kexec root shell is a successful TPM
# auto-unseal: if the seal policy did not match the live PCRs, the activation
# fails closed (no fallback), nobody answers anything, and the boot never
# reaches a shell → this gate times out → FAIL. "We typed nothing and still
# booted" is therefore itself the negative proof that the unlock was TPM-only.
echo "=== waiting up to ${SHELL_TIMEOUT}s for the post-kexec root shell" >&2
echo "    (typing NOTHING — reaching it proves TPM-only auto-unseal) ===" >&2
booted=false
for _ in $(seq 1 "$((SHELL_TIMEOUT / 5))"); do
  if seen_in_history "root@${CONFIG_NAME}"; then
    booted=true
    break
  fi
  # A password prompt reaching serial would mean activation fell back to the
  # modal (TPM unseal failed). It is normally console-suppressed like everything
  # else under the held console, so this is belt-and-braces, NOT the primary
  # negative — the primary negative is "we typed nothing and still booted."
  if seen_in_history "$PASSWORD_PROMPT_RE"; then
    echo "FAIL: cryptroot fell back to the password modal — TPM auto-unseal FAILED" >&2
    exit 1
  fi
  sleep 5
done
if [ "$booted" != true ]; then
  echo "FAIL: never reached the booted root shell — TPM auto-unseal did not unlock" >&2
  echo "      cryptroot (no passphrase was typed; --token-only cannot fall back)." >&2
  exit 1
fi
echo "=== reached the shell without typing anything — TPM-only unlock proven ===" >&2

# Confirm the shell is actually interactive (not a stale autologin banner)
# before we query the imported journal from it.
ready=false
for _ in $(seq 1 12); do
  send_cmd "echo NMBL_UNSEAL_SHELL_READY"
  if wait_for "NMBL_UNSEAL_SHELL_READY" 10; then
    ready=true
    break
  fi
done
if [ "$ready" != true ]; then
  echo "FAIL: booted shell never became interactive" >&2
  exit 1
fi

# CONFIRM THE MECHANISM from the IMPORTED journal: NMBL emits the token-specific
# unseal marker during phase-3 activation while it still owns the TUI console,
# so it is suppressed from live serial — but it precedes the kexec cpio-log
# freeze, so it survives into the booted system's journal under tag `nmbl-init`.
# This proves the unlock above was a genuine `--token-only` TPM unseal (not some
# other unlock path), subsuming the old TPM-present + measured-boot precondition.
echo "=== confirming the TPM-unseal MECHANISM via the nmbl-init journal ===" >&2
if ! assert_journal_tag "${JOURNAL_TAG}" "${UNSEAL_OK_PHRASE}"; then
  echo "FAIL: no '${UNSEAL_OK_PHRASE}' line in the ${JOURNAL_TAG} journal — the" >&2
  echo "      TPM-token unseal marker is absent, so the box did not unlock via a" >&2
  echo "      genuine cryptsetup --token-only unseal." >&2
  exit 1
fi
echo "=== PASS: cryptroot auto-unsealed from the TPM (token marker in journal) ===" >&2

# Belt-and-braces: a password prompt anywhere in the serial history means the
# seal degraded to a fallback — fail the happy path even with the unseal marker.
if seen_in_history "$PASSWORD_PROMPT_RE"; then
  echo "FAIL: a password prompt appeared despite the unseal marker — the TPM" >&2
  echo "      auto-unseal is not clean (a fallback path was exercised)." >&2
  exit 1
fi

echo "PASS: TPM seal/unseal roundtrip — enroll, power-cycle, auto-unseal, up." >&2
exit 0
