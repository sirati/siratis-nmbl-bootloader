#!/usr/bin/env bash
# Secure-boot scenario: a CORRUPT driver image is REFUSED (#1 NEG / FEATURE-#1).
# NEGATIVE test — a false-green is the worst outcome.
#
# The clean negative twin of sb-driver-image. The SAME test-secure-boot-driver
# config + install-runtime-signed disk, but the driver squashfs
# (/boot/nmbl/driver-extra.sfs) is CORRUPTED on the FAT32 boot partition before
# boot, so NMBL's single-fd verify (the SHA-512 it streams over the pinned fd no
# longer matches the detached signature) FAILS.
#
# Per the IMPLEMENTED enforce-mode behaviour — NOT degrade-and-continue — the
# loader returns NmblError::DriverImage, which the #24 call site routes through
# `policy::refuse_unsigned` → RebootIntoRescue (src/imageload/verify.rs +
# src/main_parts/boot_runtime.rs, R-1). The image is NEVER loop-mounted on a
# verify failure (verify is step 1, before mount), and NMBL refuses the whole
# boot rather than silently dropping the image. CRUCIALLY the driver-image load
# runs BEFORE `open_console` and the LUKS passphrase modal, so this refuse fires
# WITHOUT any passphrase being entered.
#
# We assert:
#   (a) a refuse/rescue marker appears (refusal markers DO reach live serial),
#   (b) the booted system is NEVER reached (no `root@<host>` un-refused),
#   (c) the driver-image-LOADED success marker is ABSENT from the nmbl-init
#       journal (the image was refused, so it was never loaded),
#   (d) NO emergency shell is offered (the refuse terminus is a non-interactive
#       countdown — R-1/R-13/FIX-35).
#
# Flow:
#   1. Stage a writable disk copy named what the runner expects, then CORRUPT
#      /nmbl/driver-extra.sfs on the ESP (overwrite its leading bytes; the file
#      stays present, so the detached sig is still found but no longer matches —
#      a true bad-image refusal, not a missing-file one). The squashfs lives on
#      the plain ESP, OUTSIDE the LUKS container, so no key is needed.
#   2. Launch the driver runner (swtpm "tis" + SB-OVMF, smm=on).
#   3. Assert a REFUSE marker appears.
#   4. Assert the driver gen never booted un-refused AND no emergency shell.
#   5. Assert (best-effort, only if the box somehow reached a journal) that the
#      driver-image-loaded marker is ABSENT.
#   6. Tear the VM down and clean up everything THIS script started.
#
# Exit 0 on success (refused, image never loaded, no shell), 1 on any failure.
#
# Required on PATH: vm-serial-man, the runner ($NMBL_RUNNER or
# `run-test-secure-boot-driver`), screen, coreutils, gnugrep, qemu, guestfish
# (libguestfs), OVMFFull, swtpm. The pristine signed disk is read from
# $NMBL_SB_DISK (the install-runtime-signed driver qcow2; the flake app exports
# it).

set -uo pipefail

CONFIG_NAME="test-secure-boot-driver"

# A best-effort passphrase fallback. The refuse fires BEFORE the LUKS modal, so
# we should never need this; we keep a single late best-effort send in case some
# future ordering change moves the modal earlier. Sending it is harmless.
LUKS_PASSPHRASE="${NMBL_LUKS_PASSPHRASE:-test}"

# A REFUSE happened: any refuse/rescue/signature/driver-image-failure marker.
REFUSE_RE='RebootIntoRescue|Refusing|refuse|signature (verification )?failed|PolicyRefused|bad signature|reboot.*rescue|countdown|verify failed|relock|DriverImage|driver-image (load )?failed'
# An emergency SHELL being offered is a HARD FAIL. NMBL's actual emergency-shell
# signatures (matched case-insensitively).
EMERGENCY_SHELL_RE='dropped to emergency shell|Emergency shell|Pretty Shell|Raw Shell|Choose what to do next|nmbl\.shell'
# The driver gen BOOTING its normal system is a HARD FAIL when un-refused.
BOOTED_RE="root@${CONFIG_NAME}\\b"

REFUSE_WATCH="${NMBL_REFUSE_WATCH:-150}"
ABSENCE_WATCH="${NMBL_ABSENCE_WATCH:-60}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "${SCRIPT_DIR}/lib.sh"

RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/nmbl-sb-driver-bad.XXXXXX")"

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

PRISTINE_DISK="${NMBL_SB_DISK:-}"
if [ -z "$PRISTINE_DISK" ] || [ ! -f "$PRISTINE_DISK" ]; then
  echo "FAIL: \$NMBL_SB_DISK is unset or not a file ('${PRISTINE_DISK}')." >&2
  echo "      Set it to the install-runtime-signed driver qcow2." >&2
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
# "use existing disk" branch reuses OUR tampered image.
echo "=== staging a writable disk copy to tamper ===" >&2
cp "$PRISTINE_DISK" "${RUN_DIR}/${CONFIG_NAME}.qcow2"
chmod 644 "${RUN_DIR}/${CONFIG_NAME}.qcow2"

# CORRUPT the driver squashfs on the ESP so the single-fd verify mismatches its
# detached signature. The disko-luks layout: sda1 = EF02 boot, sda2 = vfat ESP
# (/boot), sda3 = LUKS. The image lives at /nmbl/driver-extra.sfs on the ESP,
# OUTSIDE the LUKS container. We overwrite its leading bytes in place (keeping the
# file present + the .sig untouched, so this is a true BAD-IMAGE refusal, not a
# missing-file one).
echo "=== tampering: corrupting the driver squashfs on the boot partition ===" >&2
TAMPER_SRC="${RUN_DIR}/tamper.bin"
printf 'NMBL_BAD_DRIVER_IMAGE_TAMPER_BYTES_0123456789' >"${TAMPER_SRC}"

DRIVER_SFS="/nmbl/driver-extra.sfs"
TAMPER_GF="${RUN_DIR}/tamper.gf"
{
  echo "run"
  echo "mount /dev/sda2 /"
  echo "upload-offset ${TAMPER_SRC} ${DRIVER_SFS} 0"
  echo "umount /"
} >"${TAMPER_GF}"

if ! guestfish --rw -a "${RUN_DIR}/${CONFIG_NAME}.qcow2" -f "${TAMPER_GF}"; then
  echo "FAIL: guestfish could not corrupt the driver squashfs (${DRIVER_SFS})" >&2
  echo "      (tamper script below) — the disk may not carry the image." >&2
  cat "${TAMPER_GF}" >&2 || true
  exit 1
fi
echo "=== tamper step done (corrupted ${DRIVER_SFS}) ===" >&2

echo "=== launching SB+TPM driver-image VM via ${RUNNER} (workdir ${RUN_DIR}) ===" >&2
if ! "$RUNNER"; then
  echo "FAIL: runner exited non-zero" >&2
  exit 1
fi

if ! pin_vm_socket "$CONFIG_NAME"; then
  echo "FAIL: could not pin VM_SOCKET to the ${CONFIG_NAME} manager" >&2
  exit 1
fi

# Watch for the refuse. The driver-image load (and its refuse) runs BEFORE the
# LUKS modal and the console, so no passphrase is needed for the refuse to fire.
# We send one late best-effort passphrase only if the box unexpectedly reached a
# prompt, never as the refuse trigger.
echo "=== asserting NMBL REFUSES the corrupt driver image (up to ${REFUSE_WATCH}s) ===" >&2
refused=false
sent_pw=false
for _ in $(seq 1 "$((REFUSE_WATCH / 5))"); do
  if seen_in_history "$REFUSE_RE"; then
    refused=true
    break
  fi
  if seen_in_history "$BOOTED_RE" && ! seen_in_history "$REFUSE_RE"; then
    echo "FAIL: the driver gen BOOTED (root shell of ${CONFIG_NAME}) with NO refuse —" >&2
    echo "      the corrupt image was NOT refused (verify silently passed/degraded)." >&2
    exit 1
  fi
  # Best-effort single late passphrase, only if a modal somehow appeared first.
  if [ "$sent_pw" != true ] && seen_in_history 'Enter LUKS passphrase|passphrase for cryptroot'; then
    sleep 3
    send_cmd "$LUKS_PASSPHRASE"
    sent_pw=true
  fi
  sleep 5
done
if [ "$refused" != true ]; then
  echo "FAIL: no refuse/rescue marker within ${REFUSE_WATCH}s — the corrupt image was" >&2
  echo "      NOT refused. Degrade-and-continue would be a security regression here." >&2
  exit 1
fi
echo "=== PASS: NMBL refused the corrupt driver image ===" >&2

# The driver-image-LOADED success marker must NEVER appear: a refused image is
# never loaded. This marker is suppressed from live serial (TUI-held), but on a
# refused boot it is never emitted at all, so its ABSENCE from history is a sound
# negative check (we cannot reach the post-kexec journal — the box reboots into
# rescue). Assert it is absent from the captured serial history.
if seen_in_history "driver-image loaded"; then
  echo "FAIL: a 'driver-image loaded' marker appeared — the corrupt image was loaded" >&2
  echo "      despite the verify failure (FIX-02/FIX-05 regression)." >&2
  exit 1
fi

# No emergency shell may EVER be offered, and the driver gen must never boot
# un-refused. Both polled over a watch window so a late event still trips.
echo "=== asserting ABSENCE of any emergency shell, and of an UN-REFUSED boot (${ABSENCE_WATCH}s) ===" >&2
check_no_unrefused_boot() {
  local boot_line refuse_line
  boot_line="$(first_match_line "$BOOTED_RE")"
  [ -z "$boot_line" ] && return 0
  refuse_line="$(first_match_line "$REFUSE_RE")"
  if [ -z "$refuse_line" ]; then
    echo "FAIL: the driver gen BOOTED (root shell at line ${boot_line}) with NO refuse." >&2
    return 1
  fi
  if [ "$boot_line" -lt "$refuse_line" ]; then
    echo "FAIL: a ${CONFIG_NAME} root shell (line ${boot_line}) appeared BEFORE the first" >&2
    echo "      refuse (line ${refuse_line}) — the driver gen booted un-refused." >&2
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
if seen_in_history "driver-image loaded"; then
  echo "FAIL: 'driver-image loaded' marker present (final sweep) — bad image was loaded" >&2
  exit 1
fi
check_no_unrefused_boot || exit 1

echo "PASS: corrupt driver image refused; image never loaded; gen never booted; no emergency shell." >&2
exit 0
