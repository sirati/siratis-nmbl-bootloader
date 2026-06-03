#!/usr/bin/env bash
# Secure-boot scenario: STAGED BOOT apply (matrix row #2 / FEATURE #2).
#
# The first (priority) volume carries an image with MORE drivers + instructions
# that NMBL loads as a SECOND STAGE. After the post-unlock priority gate attests
# the inside-LUKS cryptroot, `apply_staged_boot` single-fd verifies BOTH the
# staged image (--domain driver-image) AND the signed config fragment
# (--domain staged-fragment), transactionally merges the fragment into the base
# config, and re-runs the merged config's effects. The fragment adds ONE extra
# explicit kernel module (`dummy`) the base config never loads, so the staged
# re-run loading it is the observable proof the merge took effect.
#
# All three staged artifacts (priority file, image, fragment) were signed AT
# INSTALL RUNTIME by `nmbl-sign` reading the ML-DSA key from a PATH — no signing
# key is ever a derivation input.
#
# Flow:
#   1. Launch the staged runner (swtpm "tis" + SB-OVMF, smm=on), whose disk
#      carries the install-signed priority file + staged image + fragment on the
#      cryptroot, plus the install-signed generation.
#   2. Type the install LUKS passphrase so cryptroot opens (no TPM enrol needed).
#      Opening cryptroot is what makes the inside-LUKS priority gate fire.
#   3. Assert NMBL did NOT refuse: no refuse-countdown / reboot-into-rescue /
#      signature-failure marker appears.
#   4. Assert the system reaches the post-kexec root shell (the merged config
#      kexec'd and the new system booted).
#   5. From that shell, assert in the IMPORTED `nmbl-init` journal:
#        a. the priority gate attested the volume
#           ("priority-gate (PostUnlock): signature VALID"),
#        b. the staged fragment was applied
#           ("staged-boot: fragment applied"),
#        c. the staged re-run loaded the extra module (the staged module-load
#           phase marker), proving the merged config's effect actually ran.
#      These NMBL markers cannot be read from LIVE serial: NMBL holds the TUI
#      console during the gate/merge and suppresses its stderr branch — but they
#      are frozen into the pre-kexec cpio log and replayed into the booted
#      journal under the `nmbl-init` tag.
#   6. Tear the VM down and clean up everything THIS script started.
#
# Exit 0 on success, 1 on any failure.
#
# Required on PATH: vm-serial-man, the runner ($NMBL_RUNNER), screen, coreutils,
# gnugrep, qemu, OVMFFull, swtpm.

set -uo pipefail

# The staged twin's config name (hostname + screen session + qcow2). The runner
# is the staged runner, set via NMBL_RUNNER by the flake.
CONFIG_NAME="test-secure-boot-staged"

# The fixed install LUKS passphrase (disko-luks-password.nix). Typing this opens
# cryptroot, which makes the inside-LUKS priority gate (and thus staged boot)
# run.
LUKS_PASSPHRASE="${NMBL_LUKS_PASSPHRASE:-test}"
PASSWORD_PROMPT_RE='Enter LUKS passphrase|passphrase for cryptroot|Select NixOS Generation|Passphrase'
BOOTED_RE="root@${CONFIG_NAME}"

# A REFUSAL (any form) is a HARD FAIL: every staged artifact is correctly
# signed, so the gate must attest and the fragment must verify+apply.
REFUSE_RE='RebootIntoRescue|refuse|Refusing|signature (verification )?failed|PolicyRefused|bad signature|reboot.*rescue|countdown'

# POSITIVE proof, all read from the IMPORTED nmbl-init journal (NOT live serial,
# which is console-suppressed during the gate + merge but frozen into the
# pre-kexec cpio log replayed post-kexec).
JOURNAL_TAG="nmbl-init"
# (a) the post-unlock priority gate attested the volume (yielded the
#     AttestedVolume staged boot consumes). Emitted at policy/gate.rs:183.
GATE_OK_PHRASE="signature VALID"
# (b) the staged fragment verified + merged and the merged config took effect.
#     Emitted at staged/mod.rs:138 ("staged-boot: fragment applied; merged
#     config in effect").
STAGED_OK_PHRASE="fragment applied"
# (c) the staged re-run actually re-loaded the merged config's explicit modules
#     — the phase marker the rerun pushes BEFORE loading them
#     (staged/rerun.rs:47, "staged-boot: re-loading explicit kernel modules").
#     Its presence proves the merged config's effects ran (not just that the
#     fragment parsed); the module-load itself happens immediately after.
STAGED_RERUN_PHRASE="re-loading explicit kernel modules"

# The staged path is heavier than a plain boot (priority-gate mount + two
# single-fd verifies + transactional merge + module re-load + activation re-run
# before kexec), so allow a longer budget to reach the post-kexec shell.
SHELL_TIMEOUT="${NMBL_SHELL_TIMEOUT:-360}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "${SCRIPT_DIR}/lib.sh"

RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/nmbl-sb-staged.XXXXXX")"

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

echo "=== launching SB+TPM staged VM via ${RUNNER} (workdir ${RUN_DIR}) ===" >&2
cd "${RUN_DIR}" || {
  echo "FAIL: could not cd into ${RUN_DIR}" >&2
  exit 1
}
if ! "$RUNNER"; then
  echo "FAIL: runner exited non-zero" >&2
  exit 1
fi

# Pin VM_SOCKET to THIS scenario's own manager so every send/trigger/history
# read below scopes to our VM — never a foreign manager a concurrent peer runs.
if ! pin_vm_socket "$CONFIG_NAME"; then
  echo "FAIL: could not pin VM_SOCKET to the ${CONFIG_NAME} manager" >&2
  exit 1
fi

# Answer NMBL's LUKS passphrase modal so cryptroot opens; opening it makes the
# inside-LUKS priority gate fire and staged boot run. Unlike the single-modal
# scenarios, staged boot shows the modal TWICE (base activation + the staged
# re-run's activation re-run), and the console-held ratatui frame may not be
# input-ready the instant it renders — so we re-send the passphrase every cycle
# in the wait loop below, stopping at kexec (the proven answer for the double
# modal). Send an initial answer here once the first modal is detected.
echo "=== feeding the LUKS passphrase so cryptroot opens ===" >&2
if wait_for "$PASSWORD_PROMPT_RE" 90; then
  echo "=== luks-password modal detected; answering ===" >&2
else
  echo "=== modal text not yet flushed; answering on best-effort timing ===" >&2
fi
sleep 3
send_cmd "$LUKS_PASSPHRASE"

echo "=== waiting up to ${SHELL_TIMEOUT}s for the post-kexec root shell ===" >&2
# The staged-boot path shows the LUKS passphrase modal TWICE: once for the base
# luks-password activation, and AGAIN when the staged re-run re-executes the
# activations on the merged config (the mapper is already open by then, so the
# second answer is harmless). The console-held ratatui modal may also not be
# input-ready the instant it renders, so a single send races and is dropped.
# We therefore RE-SEND the passphrase every cycle UNTIL kexec is reached (after
# which no modal can appear) — the proven-robust answer for the double modal.
booted=false
for _ in $(seq 1 "$((SHELL_TIMEOUT / 5))"); do
  if seen_in_history "$REFUSE_RE"; then
    echo "FAIL: NMBL REFUSED the staged boot (refusal marker seen) — every" >&2
    echo "      staged artifact is correctly signed, so this is a real bug." >&2
    exit 1
  fi
  if seen_in_history "$BOOTED_RE"; then
    booted=true
    break
  fi
  # Re-answer the passphrase until kexec: covers both the base-activation modal
  # and the staged-re-run modal. Stop once kexec / the booted prompt is reached.
  if ! seen_in_history "kexec_core: Starting new kernel|${BOOTED_RE}"; then
    send_cmd "$LUKS_PASSPHRASE"
  fi
  sleep 5
done
if [ "$booted" != true ]; then
  echo "FAIL: never reached the booted root shell (staged boot did not kexec)" >&2
  exit 1
fi

# Double-check no refusal slipped past in the scrollback.
if seen_in_history "$REFUSE_RE"; then
  echo "FAIL: a refusal marker is present in the history — staged verify/merge" >&2
  echo "      did NOT pass" >&2
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

# POSITIVE staged-boot proof, all from the IMPORTED journal. Booting to a shell
# is NOT enough: a build where staged boot silently did not engage would also
# reach the autologin shell. We assert the three load-bearing markers in order.
echo "=== asserting the priority gate attested the volume (nmbl-init journal) ===" >&2
if ! assert_journal_tag "${JOURNAL_TAG}" "${GATE_OK_PHRASE}"; then
  echo "FAIL: no '${GATE_OK_PHRASE}' line in the ${JOURNAL_TAG} journal — the" >&2
  echo "      post-unlock priority gate did NOT attest the volume (so staged" >&2
  echo "      boot was never reached)." >&2
  exit 1
fi
echo "=== PASS: priority gate attested the volume ===" >&2

echo "=== asserting the staged fragment was applied (nmbl-init journal) ===" >&2
if ! assert_journal_tag "${JOURNAL_TAG}" "${STAGED_OK_PHRASE}"; then
  echo "FAIL: no '${STAGED_OK_PHRASE}' line in the ${JOURNAL_TAG} journal — the" >&2
  echo "      signed staged fragment did NOT verify+merge (FEATURE #2 did not" >&2
  echo "      engage)." >&2
  exit 1
fi
echo "=== PASS: staged fragment applied (merged config in effect) ===" >&2

echo "=== asserting the staged re-run applied the merged config's effects ===" >&2
if ! assert_journal_tag "${JOURNAL_TAG}" "${STAGED_RERUN_PHRASE}"; then
  echo "FAIL: no '${STAGED_RERUN_PHRASE}' line in the ${JOURNAL_TAG} journal —" >&2
  echo "      the merged config's effects (the fragment's extra kernel module)" >&2
  echo "      were NOT re-run." >&2
  exit 1
fi
echo "=== PASS: staged re-run re-loaded the merged config's explicit modules ===" >&2

echo "PASS: staged boot — priority gate attested, signed fragment verified +" >&2
echo "      merged, merged effects re-run, system kexec'd + booted." >&2
exit 0
