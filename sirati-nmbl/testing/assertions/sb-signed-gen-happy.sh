#!/usr/bin/env bash
# Secure-boot scenario: signed generation happy path.
#
# A correctly-signed generation must verify, measure, and kexec into the
# system. The generation's kernel + initrd were signed at install time with
# the INSECURE-TEST private key whose public half is baked into nmbl-init, so
# the pre-kexec verify guard (verify_measure_then_load) passes and NMBL boots
# the system normally — NO refuse, NO reboot-into-rescue.
#
# The cryptroot must OPEN before NMBL reaches generation verify+kexec (storage
# activation runs first). This scenario boots the passphrase-unlock twin of the
# config — the SAME install-signed generation, only the LUKS unlock differs —
# and types the fixed install passphrase so the boot reaches the verify+measure
# +kexec we want to prove. The tpm-unlock config is exercised by the roundtrip.
#
# Flow:
#   1. Launch the passphrase-unlock runner (swtpm "tis" + SB-OVMF, smm=on),
#      whose disk carries /boot/nmbl/sigs/<gen-id>/{kernel,initrd}.sig.
#   2. Type the install LUKS passphrase so cryptroot opens.
#   3. Assert NMBL did NOT refuse: no refuse-countdown / reboot-into-rescue /
#      signature-failure marker appears.
#   4. Assert the system reaches the post-kexec root shell (verify passed, the
#      generation was kexec'd).
#   5. Tear the VM down and clean up everything THIS script started.
#
# Exit 0 on success, 1 on any failure.
#
# Required on PATH: vm-serial-man, the runner ($NMBL_RUNNER or
# `run-test-secure-boot-enroll`), screen, coreutils, gnugrep, qemu, OVMFFull,
# swtpm.

set -uo pipefail

# The passphrase-unlock twin's config name (hostname + screen session +
# qcow2). The runner is the enroll runner, set via NMBL_RUNNER by the flake.
CONFIG_NAME="test-secure-boot-enroll"

# The fixed install LUKS passphrase (disko-luks-password.nix). NMBL stage-0's
# luks-password activation shows an answerable modal; typing this opens
# cryptroot so the boot can reach generation verify+kexec.
LUKS_PASSPHRASE="${NMBL_LUKS_PASSPHRASE:-test}"
# A best-effort detector for NMBL's luks-password modal. Matches the enroll
# twin's configurable prompt label AND the modal chrome NMBL always renders
# (the "Passphrase" box title + the permanent "Select NixOS Generation"
# checkbox row). This is ONLY a diagnostic / fast-path hint: the modal is a
# full-screen ratatui repaint that is NOT newline-terminated, so vm-serial-man's
# line-oriented history (read_line splits on '\n') may never flush the frame
# while NMBL blocks awaiting input. We therefore DO NOT gate typing on seeing
# it — we type the passphrase on a fixed cadence below regardless (a human at
# the console types once the box is up; we simply can't reliably "see" it).
PASSWORD_PROMPT_RE='Enter LUKS passphrase|passphrase for cryptroot|Select NixOS Generation|Passphrase'
# The booted-system prompt (getty autologin). networking.hostName == the config
# name (vm-config.nix), so the post-kexec shell prints "root@<config-name>".
# Unlike the modal, this is a newline-terminated line and flushes to history.
BOOTED_RE="root@${CONFIG_NAME}"

# A REFUSAL (any form) on the happy path is a HARD FAIL: the signed generation
# should verify. Covers the refuse countdown, the policy-refused terminus, and
# the signature-failure stage markers.
REFUSE_RE='RebootIntoRescue|refuse|Refusing|signature (verification )?failed|PolicyRefused|bad signature|reboot.*rescue|countdown'
# POSITIVE proof the verify+measure guard actually RAN — not merely that the
# box booted. `signature verified:` is emitted ONLY on the enforce-mode
# verify-OK arm (src/boot/handoff.rs); `measure: extended PCR-11` is emitted by
# the post-verify measure step (src/tpm/measure.rs). Asserting these rejects a
# build where signing silently didn't engage (it would boot but emit neither).
VERIFY_OK_RE='signature verified:'
MEASURE_OK_RE='measure: extended PCR-11'

SHELL_TIMEOUT="${NMBL_SHELL_TIMEOUT:-240}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "${SCRIPT_DIR}/lib.sh"

RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/nmbl-sb-gen.XXXXXX")"

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

cleanup() {
  local rc=$?
  if [ "$rc" -ne 0 ]; then
    dump_serial_history || true
  fi
  echo "=== cleanup ===" >&2
  vm-serial-man stop >/dev/null 2>&1 || true
  if screen -ls 2>/dev/null | grep -q "\.${CONFIG_NAME}\b"; then
    screen -S "${CONFIG_NAME}" -X quit >/dev/null 2>&1 || true
  fi
  rm -rf "${RUN_DIR}" >/dev/null 2>&1 || true
  exit "$rc"
}
trap cleanup EXIT INT TERM

RUNNER="${NMBL_RUNNER:-run-${CONFIG_NAME}}"
if ! command -v "$RUNNER" >/dev/null 2>&1 && [ ! -x "$RUNNER" ]; then
  echo "FAIL: runner '${RUNNER}' not found on PATH" >&2
  exit 1
fi

echo "=== launching SB+TPM VM via ${RUNNER} (workdir ${RUN_DIR}) ===" >&2
cd "${RUN_DIR}" || {
  echo "FAIL: could not cd into ${RUN_DIR}" >&2
  exit 1
}
if ! "$RUNNER"; then
  echo "FAIL: runner exited non-zero" >&2
  exit 1
fi

# Answer NMBL stage-0's LUKS passphrase modal so cryptroot opens and the boot
# can proceed to generation verify+measure+kexec.
#
# WHY WE TYPE BLIND (no wait-for-prompt gate): NMBL's passphrase modal is a
# full-screen ratatui repaint that is NOT newline-terminated. vm-serial-man's
# history is built by `read_line` (splits on '\n'), so while NMBL sits on the
# modal awaiting input the un-terminated frame never flushes to the buffer —
# `seen_in_history`/`trigger` for the modal text can hang/miss even though the
# box is on screen. So we do what an operator does: give the boot a head-start
# to reach the LUKS stage, then type the passphrase and Enter on a fixed cadence
# until the system actually boots (or refuses). `send_cmd` appends the newline,
# so each attempt submits. Extra Enters on an already-unlocked system are
# harmless (blank shell prompts). The post-kexec markers we wait for ARE
# newline-terminated and flush normally.
echo "=== feeding the LUKS passphrase so cryptroot opens ===" >&2
# A short head-start so the firmware → NMBL → activation path can paint the
# modal before the first keystroke (typing into a pre-modal console is a no-op).
sleep 10
# Informational only (never gates the typing): if the modal text DID flush to
# history, note it — helps a reader correlate the keystroke with the prompt.
if seen_in_history "$PASSWORD_PROMPT_RE"; then
  echo "=== luks-password modal text seen in history; answering ===" >&2
fi
send_cmd "$LUKS_PASSPHRASE"

# Wait for the booted root shell, re-typing the passphrase periodically in case
# the first attempt raced the modal's appearance. A refusal at any point is an
# immediate failure — the signed generation must verify and boot.
echo "=== waiting up to ${SHELL_TIMEOUT}s for the post-kexec root shell ===" >&2
booted=false
i=0
for _ in $(seq 1 "$((SHELL_TIMEOUT / 5))"); do
  if seen_in_history "$REFUSE_RE"; then
    echo "FAIL: NMBL REFUSED a correctly-signed generation (refusal marker seen)" >&2
    exit 1
  fi
  if seen_in_history "$BOOTED_RE"; then
    booted=true
    break
  fi
  # Re-feed the passphrase periodically until we see a kexec/boot marker. We do
  # this BLIND (not gated on seeing the modal — it may never flush, see above),
  # every ~15s, so a slow/late modal still gets answered.
  i=$((i + 1))
  if [ $((i % 3)) -eq 0 ] && ! seen_in_history "kexec|${BOOTED_RE}"; then
    send_cmd "$LUKS_PASSPHRASE"
  fi
  sleep 5
done
if [ "$booted" != true ]; then
  echo "FAIL: never reached the booted root shell (signed gen did not kexec)" >&2
  exit 1
fi

# Double-check no refusal slipped past in the scrollback.
if seen_in_history "$REFUSE_RE"; then
  echo "FAIL: a refusal marker is present in the history — verify did NOT pass" >&2
  exit 1
fi

# Confirm the shell is actually interactive (not a stale autologin banner).
ready=false
for _ in $(seq 1 12); do
  send_cmd "echo NMBL_SHELL_READY"
  if wait_for "NMBL_SHELL_READY" 10; then
    ready=true
    break
  fi
done
if [ "$ready" != true ]; then
  echo "FAIL: booted shell never became interactive" >&2
  exit 1
fi

# POSITIVE verify+measure proof. Booting to a shell is NOT enough: a build
# where the secure-boot feature silently didn't engage would also reach the
# autologin shell. Require BOTH the enforce-mode verify-OK marker AND the
# PCR-11 measure marker in the scrollback — they fire only when the
# verify→measure guard actually ran (handoff.rs / measure.rs).
echo "=== asserting the verify+measure guard RAN (positive markers) ===" >&2
if ! seen_in_history "$VERIFY_OK_RE"; then
  echo "FAIL: no positive signature-verified marker — the verify guard did NOT" >&2
  echo "      run/pass (signing may have silently not engaged)." >&2
  exit 1
fi
if ! seen_in_history "$MEASURE_OK_RE"; then
  echo "FAIL: no 'measure: extended PCR-11' marker — the measured-boot step did" >&2
  echo "      NOT run, so verify+measure was not exercised on this boot." >&2
  exit 1
fi
echo "=== PASS: signature verified AND PCR-11 extended (verify+measure ran) ===" >&2

echo "PASS: signed generation verified, measured, kexec'd — system booted." >&2
exit 0
