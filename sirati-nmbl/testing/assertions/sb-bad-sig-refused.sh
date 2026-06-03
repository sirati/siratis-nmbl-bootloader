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
#      failure) appears.
#   4. Assert the bad GENERATION never booted its normal system UN-REFUSED. The
#      complete serial the manager now captures includes the legitimate post-
#      refuse rescue console, so a bare `root@test-secure-boot` is NOT itself a
#      failure: it is only a failure if it appears with NO preceding refuse
#      (i.e. the bad gen was kexec'd instead of rejected). We scope the check by
#      ORDERING the booted-shell line against the first refuse line; an un-
#      refused bad-gen boot still HARD-FAILS, the rescue shell does not.
#   5. Assert NO emergency shell is offered: the emergency-shell prompt markers
#      MUST be ABSENT (asserting ABSENCE, not a banner — false-green guard).
#   6. Tear the VM down and clean up everything THIS script started.
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
# never drop to (or offer) the shell-offering emergency path. These are NMBL's
# ACTUAL emergency-shell signatures, not any shell-ish string (audit F6 — the
# old `/bin/sh` / `sh-[0-9]` terms over-matched). Matched case-insensitively:
#   * "NMBL: dropped to emergency shell" — the entry banner (shell/banner.rs:12)
#   * "Emergency shell"                  — the offer-menu / modal title
#   * "Pretty Shell" / "Raw Shell"       — the shell menu items (builders.rs)
#   * "Choose what to do next"           — the emergency-menu prompt
#   * "nmbl.shell"                       — the debug emergency-shell kernel arg
EMERGENCY_SHELL_RE='dropped to emergency shell|Emergency shell|Pretty Shell|Raw Shell|Choose what to do next|nmbl\.shell'
# The bad generation BOOTING its normal multi-user system is a HARD FAIL: it
# must never reach the booted system. The signal is the booted system's autologin
# prompt `root@${CONFIG_NAME}` (services.getty.autologinUser=root,
# networking.hostName=${CONFIG_NAME}). Anchor the trailing host so a hostname that
# merely has ${CONFIG_NAME} as a PREFIX (e.g. the roundtrip's `test-secure-boot-
# enroll` twin) can never substring-match this — `\b` after the name rejects the
# `-enroll` suffix. The bad gen lives inside the LUKS container, so NMBL only
# reaches generation verify+kexec AFTER cryptroot opens; on a tampered gen the
# pre-kexec guard refuses and reboots into rescue, so this prompt can only legit-
# imately come from a NON-bad-gen shell (the refuse-terminus rescue console). We
# therefore do NOT treat it as a failure unconditionally — we scope it to a boot
# that was NOT refused (see the ordering check below).
BOOTED_RE="root@${CONFIG_NAME}\\b"

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

# 2) No emergency shell may EVER be offered: the refuse terminus is a non-
#    interactive countdown, never the shell-offering emergency machinery
#    (R-1/R-13/FIX-35). This is an unconditional ABSENCE assertion.
# 3) The bad GENERATION must never boot its normal multi-user system. That is a
#    failure ONLY if it happens WITHOUT a refuse — i.e. NMBL kexec'd the tampered
#    gen instead of rejecting it. Once the refuse has fired (asserted above), the
#    bad gen cannot have kexec'd, so any `root@${CONFIG_NAME}` shell that shows up
#    is the legitimate post-refuse rescue/terminus console, NOT the bad gen. We
#    therefore SCOPE the booted-shell failure to ordering: a booted shell is a
#    HARD FAIL iff it appears with NO preceding refuse (the bad gen booted un-
#    refused). A shell that appears only AFTER the first refuse line is benign.
#    This keeps the real security check (a true un-refused bad-gen boot still
#    HARD-FAILS) while not flagging the rescue shell the complete serial now
#    captures. Both checks poll over a watch window so a late event still trips.
echo "=== asserting ABSENCE of any emergency shell, and of an UN-REFUSED bad-gen boot (${ABSENCE_WATCH}s) ===" >&2
check_no_unrefused_boot() {
  # Fail iff a booted-system shell exists that is not explained by a preceding
  # refuse. `first_match_line` returns the earliest history line of each marker.
  local boot_line refuse_line
  boot_line="$(first_match_line "$BOOTED_RE")"
  [ -z "$boot_line" ] && return 0          # no booted shell at all → fine
  refuse_line="$(first_match_line "$REFUSE_RE")"
  if [ -z "$refuse_line" ]; then
    echo "FAIL: the tampered generation BOOTED (root shell of ${CONFIG_NAME} at" >&2
    echo "      history line ${boot_line}) with NO refuse marker — the bad gen was" >&2
    echo "      kexec'd instead of being rejected." >&2
    return 1
  fi
  if [ "$boot_line" -lt "$refuse_line" ]; then
    echo "FAIL: a ${CONFIG_NAME} root shell (line ${boot_line}) appeared BEFORE the" >&2
    echo "      first refuse (line ${refuse_line}) — the bad gen booted un-refused." >&2
    return 1
  fi
  return 0
}
for _ in $(seq 1 "$((ABSENCE_WATCH / 5))"); do
  if seen_in_history "$EMERGENCY_SHELL_RE"; then
    echo "FAIL: an emergency shell was offered/entered — the refuse terminus must not" >&2
    echo "      offer a shell (R-1/R-13/FIX-35)." >&2
    exit 1
  fi
  check_no_unrefused_boot || exit 1
  sleep 5
done

# Final sweep (belt and braces).
if seen_in_history "$EMERGENCY_SHELL_RE"; then
  echo "FAIL: emergency shell present (final sweep)" >&2
  exit 1
fi
check_no_unrefused_boot || exit 1

echo "PASS: tampered generation refused; bad gen never booted un-refused; no emergency shell offered." >&2
exit 0
