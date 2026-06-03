#!/usr/bin/env bash
# Secure-boot scenario: tampered/removed generation sidecar is REFUSED
# (#57 / matrix id #4b). NEGATIVE test — false-green is the worst outcome.
#
# A generation whose signature sidecar is removed (or tampered) must fail the
# pre-kexec verify guard, so NMBL refuses it and reboots into rescue. We assert
# the refuse/relock path AND that the bad generation NEVER booted AND that NO
# emergency shell is offered (the refuse terminus is a non-interactive
# countdown, never the shell-offering emergency machinery — R-1/R-13/FIX-35).
#
# Flow:
#   1. Stage a PRISTINE disk copy in our run dir, then TAMPER it: remove the
#      detached initrd signature sidecar from the FAT32 boot partition
#      (/boot/nmbl/sigs/<gen-id>/initrd.sig). The sidecars live on the plain
#      ESP, NOT inside the LUKS container, so no key is needed to mutate them.
#      The runner then reuses our pre-staged (tampered) disk verbatim.
#   2. Launch the test-secure-boot runner (swtpm "tis" + SB-OVMF, smm=on).
#   3. Assert a REFUSE marker (refuse countdown / reboot-into-rescue / signature
#      failure) appears AND the bad generation never reached the booted system
#      (no post-kexec root shell of test-secure-boot).
#   4. Assert NO emergency shell is offered: the emergency-shell prompt markers
#      MUST be ABSENT (asserting ABSENCE, not a banner — false-green guard).
#   5. Tear the VM down and clean up everything THIS script started.
#
# Exit 0 on success (refused, no shell), 1 on any failure.
#
# Required on PATH: vm-serial-man, the runner ($NMBL_RUNNER or
# `run-test-secure-boot`), screen, coreutils, gnugrep, qemu, guestfish
# (libguestfs), OVMFFull, swtpm. The pristine disk is read from
# $NMBL_SB_DISK (the test-secure-boot vmDiskImage's nixos.qcow2; the flake app
# exports it).

set -uo pipefail

CONFIG_NAME="test-secure-boot"

# A REFUSE happened: any of the refuse/rescue/signature-failure markers.
REFUSE_RE='RebootIntoRescue|Refusing|refuse|signature (verification )?failed|PolicyRefused|bad signature|reboot.*rescue|countdown|verify failed|relock'
# An emergency SHELL being offered is a HARD FAIL: the refuse terminus must
# never drop to (or offer) the shell-offering emergency path. These are the
# emergency-shell prompt signatures NMBL prints when it offers a shell.
EMERGENCY_SHELL_RE='emergency shell|drop to.*shell|nmbl.shell|Entering emergency|/bin/sh|rescue shell|press .* for a shell|sh-[0-9]'
# The bad generation BOOTING is a HARD FAIL: it must never reach the system.
BOOTED_RE="root@${CONFIG_NAME}"

REFUSE_WATCH="${NMBL_REFUSE_WATCH:-150}"
# After a refuse, watch a bit longer to be SURE no shell appears and the bad
# gen never boots (the countdown defaults to 30s before auto-reboot).
ABSENCE_WATCH="${NMBL_ABSENCE_WATCH:-60}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "${SCRIPT_DIR}/lib.sh"

RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/nmbl-sb-badsig.XXXXXX")"

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

# The pristine disk to tamper. The flake app exports $NMBL_SB_DISK to the
# vmDiskImage's nixos.qcow2 store path.
PRISTINE_DISK="${NMBL_SB_DISK:-}"
if [ -z "$PRISTINE_DISK" ] || [ ! -f "$PRISTINE_DISK" ]; then
  echo "FAIL: \$NMBL_SB_DISK is unset or not a file ('${PRISTINE_DISK}')." >&2
  echo "      Set it to the test-secure-boot vmDiskImage nixos.qcow2." >&2
  exit 1
fi
if ! command -v guestfish >/dev/null 2>&1; then
  echo "FAIL: guestfish (libguestfs) not found on PATH — cannot tamper the disk." >&2
  exit 1
fi

cd "${RUN_DIR}" || {
  echo "FAIL: could not cd into ${RUN_DIR}" >&2
  exit 1
}

# Stage a writable copy named exactly what the runner expects, so the runner's
# "use existing disk" branch reuses OUR tampered image instead of re-copying
# the pristine one from the store.
echo "=== staging a writable disk copy to tamper ===" >&2
cp "$PRISTINE_DISK" "${RUN_DIR}/${CONFIG_NAME}.qcow2"
chmod 644 "${RUN_DIR}/${CONFIG_NAME}.qcow2"

# Remove the detached initrd signature sidecars from the FAT32 boot partition.
# The disko-luks layout is: sda1 = EF02 boot, sda2 = vfat ESP (/boot), sda3 =
# LUKS. The sidecars live under /nmbl/sigs/<gen-id>/ on the ESP, OUTSIDE the
# LUKS container, so no key is needed. Mount sda2 at the guest root and
# glob-delete every initrd.sig under it — once any generation's initrd sidecar
# is gone, the enforcing pre-kexec guard refuses that generation.
echo "=== tampering: removing initrd.sig sidecar(s) from the boot partition ===" >&2
if ! guestfish --rw -a "${RUN_DIR}/${CONFIG_NAME}.qcow2" <<'GF'
run
mount /dev/sda2 /
glob rm-f /nmbl/sigs/*/initrd.sig
umount /
GF
then
  echo "FAIL: guestfish could not remove the initrd.sig sidecar(s)" >&2
  exit 1
fi
echo "=== tamper step done ===" >&2

echo "=== launching SB+TPM VM via ${RUNNER} (workdir ${RUN_DIR}) ===" >&2
if ! "$RUNNER"; then
  echo "FAIL: runner exited non-zero" >&2
  exit 1
fi

# 1) A refuse marker MUST appear: the tampered generation must be rejected.
echo "=== asserting NMBL REFUSES the tampered generation ===" >&2
if ! wait_for "$REFUSE_RE" "$REFUSE_WATCH"; then
  echo "FAIL: no refuse/rescue marker within ${REFUSE_WATCH}s — the bad gen was NOT refused" >&2
  exit 1
fi
echo "=== PASS: NMBL refused the tampered generation ===" >&2

# 2) The bad generation must NEVER have booted, and NO emergency shell may be
#    offered. Both are ABSENCE assertions over a watch window: poll the history
#    and fail the instant either forbidden marker appears.
echo "=== asserting ABSENCE of a booted bad-gen AND of any emergency shell (${ABSENCE_WATCH}s) ===" >&2
for _ in $(seq 1 "$((ABSENCE_WATCH / 5))"); do
  if seen_in_history "$BOOTED_RE"; then
    echo "FAIL: the tampered generation BOOTED (root shell of ${CONFIG_NAME} seen)" >&2
    exit 1
  fi
  if seen_in_history "$EMERGENCY_SHELL_RE"; then
    echo "FAIL: an emergency shell was offered/entered — the refuse terminus must not" >&2
    echo "      offer a shell (R-1/R-13/FIX-35)." >&2
    exit 1
  fi
  sleep 5
done

# Final absence sweep (belt and braces).
if seen_in_history "$BOOTED_RE"; then
  echo "FAIL: bad generation booted (final sweep)" >&2
  exit 1
fi
if seen_in_history "$EMERGENCY_SHELL_RE"; then
  echo "FAIL: emergency shell present (final sweep)" >&2
  exit 1
fi

echo "PASS: tampered generation refused; bad gen never booted; no emergency shell offered." >&2
exit 0
