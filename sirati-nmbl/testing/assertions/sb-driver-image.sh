#!/usr/bin/env bash
# Secure-boot scenario: signed DRIVER-IMAGE load (#1 / FEATURE-#1, matrix id #1).
#
# NMBL can load out-of-tree kernel drivers from a detached, SIGNED squashfs on
# the boot partition BEFORE the generation kexec. This scenario proves the happy
# path end-to-end: NMBL verifies the image's ML-DSA sidecar against the baked
# trust anchor, loop-mounts the squashfs, and `finit_module`s a module that is
# NOT in this config's base initrd (`dummy`, the net dummy device) — all before
# pivoting to the booted system.
#
# The proof is NMBL's pre-kexec driver-image-load marker, read from the IMPORTED
# `nmbl-init` journal — NOT live serial, and NOT the post-kexec `/proc/modules`:
#   * The module is loaded into the INITRAMFS kernel; kexec then boots a FRESH
#     kernel, so `dummy` is GONE from `/proc/modules` in the booted system —
#     `lsmod` after kexec proves nothing. (This is the kexec module-state reset,
#     not a bug.)
#   * NMBL holds the ratatui TUI console while it loads the image, so the marker
#     is SUPPRESSED from live serial (the `nmbl_*!` stderr branch is gated on
#     `!tui_active`). But it is emitted via `emit_kmsg` BEFORE the cpio-log
#     freeze, so it is carried into the next kernel's /nmbl-log/nmbl.log and
#     replayed into the booted journal under the `nmbl-init` tag
#     (lib/modules/log-import.nix) — exactly like the `signature verified`
#     marker the signed-gen-happy scenario reads.
# So we boot to the shell (cryptroot opens with the install passphrase), then
# assert the per-image marker `driver-image loaded: … dummy …` is present in the
# nmbl-init journal. Booting alone is NOT enough — a build where driver-image
# loading silently no-op'd would still reach the shell.
#
# Flow:
#   1. Launch the driver runner (swtpm "tis" + SB-OVMF, smm=on), whose signed
#      disk carries /boot/nmbl/driver-extra.sfs{,.sig}.
#   2. Type the install LUKS passphrase so cryptroot opens.
#   3. Assert NMBL did NOT refuse (the signed image must verify).
#   4. Assert the post-kexec root shell is reached + interactive.
#   5. Assert the nmbl-init journal carries the driver-image-loaded marker AND
#      the module name `dummy` — positive proof the module came from the IMAGE.
#   6. Tear the VM down and clean up everything THIS script started.
#
# Exit 0 on success, 1 on any failure.
#
# Required on PATH: vm-serial-man, the runner ($NMBL_RUNNER or
# `run-test-secure-boot-driver`), screen, coreutils, gnugrep, qemu, OVMFFull,
# swtpm.

set -uo pipefail

# The driver config's name (hostname + screen session + qcow2). The runner is
# set via NMBL_RUNNER by the flake.
CONFIG_NAME="test-secure-boot-driver"

# The fixed install LUKS passphrase (disko-luks-password.nix). NMBL stage-0's
# luks-password activation shows an answerable modal; typing this opens cryptroot
# so the boot reaches the driver-image load + generation kexec.
LUKS_PASSPHRASE="${NMBL_LUKS_PASSPHRASE:-test}"
PASSWORD_PROMPT_RE='Enter LUKS passphrase|passphrase for cryptroot|Select NixOS Generation|Passphrase'
REPROMPT_RE='Wrong password \(attempt|cryptsetup rejected the passphrase'
BOOTED_RE="root@${CONFIG_NAME}"

# A REFUSAL (any form) on the happy path is a HARD FAIL: the signed driver image
# must verify, and the signed generation must boot.
REFUSE_RE='RebootIntoRescue|refuse|Refusing|signature (verification )?failed|PolicyRefused|bad signature|reboot.*rescue|countdown|DriverImage'
# POSITIVE proof the driver-image load ran AND loaded the module FROM the image.
# NMBL's imageload loader emits `driver-image loaded: <n> module(s) [...] from
# <path> (loopN)` (src/imageload/mod.rs) BEFORE the kexec cpio-log freeze, so it
# is replayed into the booted journal under tag `nmbl-init`. We assert BOTH the
# marker phrase and the module name `dummy` so a generic load can't false-green.
# Plain substrings (no regex metachars) safe for assert_journal_tag's grep.
JOURNAL_TAG="nmbl-init"
DRIVER_LOADED_PHRASE="driver-image loaded"
DRIVER_MODULE_PHRASE="dummy"

SHELL_TIMEOUT="${NMBL_SHELL_TIMEOUT:-240}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "${SCRIPT_DIR}/lib.sh"

RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/nmbl-sb-driver.XXXXXX")"

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

echo "=== launching SB+TPM driver-image VM via ${RUNNER} (workdir ${RUN_DIR}) ===" >&2
cd "${RUN_DIR}" || {
  echo "FAIL: could not cd into ${RUN_DIR}" >&2
  exit 1
}
if ! "$RUNNER"; then
  echo "FAIL: runner exited non-zero" >&2
  exit 1
fi

# Pin VM_SOCKET to THIS scenario's own manager so every send/trigger/history read
# below scopes to our VM — never a foreign manager a concurrent peer runs.
if ! pin_vm_socket "$CONFIG_NAME"; then
  echo "FAIL: could not pin VM_SOCKET to the ${CONFIG_NAME} manager" >&2
  exit 1
fi

# Answer NMBL stage-0's LUKS passphrase modal so cryptroot opens and the boot
# proceeds through the driver-image load to generation kexec.
echo "=== feeding the LUKS passphrase so cryptroot opens ===" >&2
if wait_for "$PASSWORD_PROMPT_RE" 90; then
  echo "=== luks-password modal detected; answering once ===" >&2
else
  echo "=== modal text not yet flushed; answering on best-effort timing ===" >&2
fi
sleep 3
send_cmd "$LUKS_PASSPHRASE"
attempts=1
MAX_ATTEMPTS=3
reprompts_answered=0

echo "=== waiting up to ${SHELL_TIMEOUT}s for the post-kexec root shell ===" >&2
booted=false
for _ in $(seq 1 "$((SHELL_TIMEOUT / 5))"); do
  if seen_in_history "$REFUSE_RE"; then
    echo "FAIL: NMBL REFUSED on the happy driver-image path (refusal marker seen)" >&2
    exit 1
  fi
  if seen_in_history "$BOOTED_RE"; then
    booted=true
    break
  fi
  if [ "$attempts" -lt "$MAX_ATTEMPTS" ] && ! seen_in_history "kexec|${BOOTED_RE}"; then
    reprompts_now="$(seen_count "$REPROMPT_RE")"
    if [ "$reprompts_now" -gt "$reprompts_answered" ]; then
      echo "=== cryptsetup re-prompted; re-answering (attempt $((attempts + 1))/${MAX_ATTEMPTS}) ===" >&2
      sleep 2
      send_cmd "$LUKS_PASSPHRASE"
      attempts=$((attempts + 1))
      reprompts_answered="$reprompts_now"
    fi
  fi
  sleep 5
done
if [ "$booted" != true ]; then
  echo "FAIL: never reached the booted root shell (driver-image gen did not kexec)" >&2
  exit 1
fi

# Double-check no refusal slipped past.
if seen_in_history "$REFUSE_RE"; then
  echo "FAIL: a refusal marker is present in the history — verify did NOT pass" >&2
  exit 1
fi

# Confirm the shell is actually interactive.
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

# POSITIVE driver-image-load proof, read from the IMPORTED nmbl-init journal
# (NOT live serial; NOT post-kexec /proc/modules). Booting to a shell is NOT
# enough — a build where driver-image loading silently no-op'd would still reach
# the shell. Assert both the marker phrase AND the module name so the module is
# proven to have come from the IMAGE.
echo "=== asserting the driver-image load ran via the nmbl-init journal ===" >&2
if ! assert_journal_tag "${JOURNAL_TAG}" "${DRIVER_LOADED_PHRASE}"; then
  echo "FAIL: no '${DRIVER_LOADED_PHRASE}' line in the ${JOURNAL_TAG} journal — NMBL" >&2
  echo "      did NOT load the signed driver image before kexec." >&2
  exit 1
fi
if ! assert_journal_tag "${JOURNAL_TAG}" "${DRIVER_MODULE_PHRASE}"; then
  echo "FAIL: the driver-image marker is present but does not name '${DRIVER_MODULE_PHRASE}'" >&2
  echo "      — the module from the image was not the one loaded." >&2
  exit 1
fi
echo "=== PASS: driver-image load marker + module '${DRIVER_MODULE_PHRASE}' in nmbl-init journal ===" >&2

echo "PASS: signed driver image verified + loaded '${DRIVER_MODULE_PHRASE}' pre-kexec; system booted." >&2
exit 0
