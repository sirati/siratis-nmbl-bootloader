#!/usr/bin/env bash
# Secure-Boot enforcement smoke test (#55 / R-10): assert the firmware REFUSES
# an UNSIGNED UKI. This is the literal precondition for #29 closing — it proves
# Secure Boot actually ENFORCES in the test environment, so the rest of the SB
# matrix cannot false-green on a firmware that would happily run anything.
#
# Flow:
#   1. Launch the sb-unsigned-uki runner. That boots a GPT/ESP disk whose
#      \EFI\BOOT\BOOTX64.EFI is an UNSIGNED NMBL UKI, under a Secure-Boot-
#      ENFORCING OVMFFull (db = Microsoft KEK/db, smm=on).
#   2. Assert the firmware refused to launch it: an OVMF Secure-Boot
#      violation banner appears (Security Violation / Access Denied / image
#      failed to load) OR it drops to the UEFI shell — and CRUCIALLY no NMBL
#      phase marker is ever seen (NMBL never ran).
#   3. Tear the VM down and clean up everything THIS script started.
#
# Distinguishes "firmware refused" (PASS here) from "NMBL refused" (a NMBL
# phase banner would mean the UKI launched — that is a FAIL: SB did not
# enforce). Exit 0 on success (firmware refused), 1 on any failure.
#
# Required on PATH: vm-serial-man, the runner ($NMBL_RUNNER or
# `run-sb-unsigned-uki` via $PATH), screen, coreutils, gnugrep.

set -uo pipefail

CONFIG_NAME="sb-unsigned-uki"

# OVMF / shim Secure-Boot refusal signatures. Any one means the firmware
# blocked the unsigned image. Case-insensitive, ORed into one regex.
SB_REFUSED_RE='Security Violation|Access Denied|Image failed to load|not allowed|verification failed|Secure Boot|UEFI Interactive Shell|Shell> '

# Markers that NMBL actually EXECUTED — i.e. the unsigned UKI LAUNCHED. Seeing
# any of these is a HARD FAIL: Secure Boot did not enforce.
#
# These must be RUNTIME markers NMBL prints only once its init runs, NOT the
# firmware's boot-entry NAME. The EFI boot entry / systemd-boot menu label is
# literally "NMBL" (lib/install-signing.nix `efibootmgr -L NMBL`; install-script
# `title NMBL Bootloader`), and OVMF/BdsDxe prints that label as it scans boot
# options EVEN WHEN it then refuses the image (run10: firmware printed
# "Access Denied -- rejected probably by Secure Boot" with NMBL never running,
# yet the old `NMBL` token false-matched the boot-entry label and FAILed). So we
# match the BootReporter phase progression NMBL renders to serial as it boots
# (`phase 1: mount …`, `phase 2a/2b: … kernel modules`, `phase 3:`, `phase 4:`
# — src/main.rs, src/modules.rs, src/activation/mod.rs) and the `nmbl-init`
# runtime startup banner (src/main.rs `nmbl_info!("nmbl-init starting")`). The
# firmware emits none of these; they appear only if NMBL's init actually ran.
NMBL_RAN_RE='nmbl-init starting|phase [1-9][a-z]?:'

# How long to watch for the firmware's decision. The refusal is near-immediate
# (the boot manager rejects the image before any OS code runs), but give OVMF
# time to scan boot options and print.
SB_WATCH="${NMBL_SB_WATCH:-90}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "${SCRIPT_DIR}/lib.sh"

RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/nmbl-sb-uki.XXXXXX")"

cleanup() {
  local rc=$?
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

echo "=== launching SB-enforcing VM via ${RUNNER} (workdir ${RUN_DIR}) ===" >&2
cd "${RUN_DIR}" || {
  echo "FAIL: could not cd into ${RUN_DIR}" >&2
  exit 1
}
if ! "$RUNNER"; then
  echo "FAIL: runner exited non-zero" >&2
  exit 1
fi

# Pin VM_SOCKET to THIS scenario's own manager so every history/trigger read
# below scopes to our VM — never a foreign manager a concurrent peer may run.
if ! pin_vm_socket "$CONFIG_NAME"; then
  echo "FAIL: could not pin VM_SOCKET to the ${CONFIG_NAME} manager" >&2
  exit 1
fi

# Watch the firmware's decision. Two outcomes must be distinguished:
#   - firmware REFUSED  ⇒ a SB violation banner / UEFI shell appears, and NMBL
#     never runs  ⇒ PASS.
#   - UKI LAUNCHED      ⇒ a NMBL marker appears  ⇒ FAIL (SB did not enforce).
echo "=== watching up to ${SB_WATCH}s for the firmware's Secure-Boot decision ===" >&2

# First: a NMBL marker appearing at ANY point is an immediate, unambiguous
# failure — it means the unsigned image executed. Check history after the watch.
if wait_for "$SB_REFUSED_RE" "$SB_WATCH"; then
  # The firmware printed a refusal. Make sure NMBL did NOT also run (the UKI
  # must never have launched). seen_in_history scans the whole capture.
  if seen_in_history "$NMBL_RAN_RE"; then
    echo "FAIL: a Secure-Boot banner appeared BUT a NMBL marker is also present" >&2
    echo "      — the unsigned UKI launched, so Secure Boot did NOT enforce." >&2
    echo "=== diagnostic: serial lines matching NMBL_RAN_RE ===" >&2
    vm-serial-man find "$(_ci_pattern "$NMBL_RAN_RE")" --socket "$VM_SOCKET" 2>&1 \
      | sed -E 's/\x1b\[[0-9;?]*[a-zA-Z]//g' | awk 'NR<=30' >&2
    echo "=== diagnostic: last 80 serial lines ===" >&2
    vm-serial-man tail 80 --socket "$VM_SOCKET" 2>&1 \
      | sed -E 's/\x1b\[[0-9;?]*[a-zA-Z]//g' >&2
    exit 1
  fi
  echo "PASS: firmware REFUSED the unsigned UKI (Secure-Boot violation; NMBL never ran)." >&2
  exit 0
fi

# No refusal banner within the window. If NMBL ran, SB failed open; otherwise
# the firmware behaved unexpectedly (no banner, no boot) — fail either way so a
# silent/ambiguous run can never false-green.
if seen_in_history "$NMBL_RAN_RE"; then
  echo "FAIL: the unsigned UKI LAUNCHED (NMBL marker seen) — Secure Boot did NOT enforce." >&2
else
  echo "FAIL: no Secure-Boot refusal banner and no NMBL boot within ${SB_WATCH}s" >&2
  echo "      — the firmware's decision could not be confirmed (treat as a failure)." >&2
fi
exit 1
