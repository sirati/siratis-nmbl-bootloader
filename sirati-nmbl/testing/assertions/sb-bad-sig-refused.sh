#!/usr/bin/env bash
# Secure-boot scenario: a generation with a TAMPERED signature is REFUSED
# (#57 / matrix id #4b). NEGATIVE test — a false-green is the worst outcome.
#
# This is the clean NEGATIVE twin of sb-signed-gen-happy. It boots the SAME
# passphrase-unlock ENROLL twin (config test-secure-boot-enroll) so cryptroot
# OPENS with the fixed install passphrase and the boot actually REACHES NMBL's
# pre-kexec generation verify guard. The ONLY difference from signed-gen-happy
# is that the generation's detached signature sidecar is CORRUPTED on the staged
# disk before boot, so the verify guard fails → NMBL refuses the generation and
# reboots into rescue. We assert the refuse/relock path AND that the tampered
# generation NEVER booted its normal system AND that NO emergency shell is
# offered (the refuse terminus is a non-interactive countdown, never the
# shell-offering emergency machinery — R-1/R-13/FIX-35).
#
# Pointing this scenario at the real tpm-unlock config (as it once did) was the
# bug: that config never enrols a token in this standalone scenario, so its
# `cryptsetup open --token-only` cryptroot would NEVER open, NMBL would never
# reach generation verify, and the tampered gen would never be refused — it
# would simply never be reached, so the assertion would time out as a false RED.
#
# Flow:
#   1. Stage a writable disk copy in our run dir named exactly what the enroll
#      runner expects (test-secure-boot-enroll.qcow2), then TAMPER it: flip bytes
#      in the detached kernel+initrd signature sidecars on the FAT32 boot
#      partition (/boot/nmbl/sigs/<gen-id>/{kernel,initrd}.sig). The sidecars
#      live on the plain ESP, NOT inside the LUKS container, so no key is needed
#      to mutate them. The runner reuses our pre-staged (tampered) disk verbatim.
#   2. Launch the enroll runner (swtpm "tis" + SB-OVMF, smm=on).
#   3. Feed the install LUKS passphrase the same detect-then-answer-once way
#      signed-gen-happy does, so cryptroot opens and the boot reaches verify.
#   4. Assert a REFUSE marker (refuse countdown / reboot-into-rescue / signature
#      failure) appears.
#   5. Assert the tampered GENERATION never booted its normal system UN-REFUSED.
#      The complete serial the manager now captures includes the legitimate post-
#      refuse rescue console, so a bare `root@test-secure-boot-enroll` is NOT
#      itself a failure: it is only a failure if it appears with NO preceding
#      refuse (i.e. the bad gen was kexec'd instead of rejected). We scope the
#      check by ORDERING the booted-shell line against the first refuse line.
#   6. Assert NO emergency shell is offered: the emergency-shell prompt markers
#      MUST be ABSENT (asserting ABSENCE, not a banner — false-green guard).
#   7. Tear the VM down and clean up everything THIS script started.
#
# Exit 0 on success (refused, no shell), 1 on any failure.
#
# Required on PATH: vm-serial-man, the runner ($NMBL_RUNNER or
# `run-test-secure-boot-enroll`), screen, coreutils, gnugrep, qemu, guestfish
# (libguestfs), OVMFFull, swtpm. The pristine disk is read from $NMBL_SB_DISK
# (the install-runtime-signed enroll qcow2; the flake app exports it).

set -uo pipefail

# The passphrase-unlock ENROLL twin's config name (hostname + screen session +
# qcow2). The runner is the enroll runner, set via NMBL_RUNNER by the flake.
CONFIG_NAME="test-secure-boot-enroll"

# The fixed install LUKS passphrase (disko-luks-password.nix). NMBL stage-0's
# luks-password activation shows an answerable modal; typing this opens cryptroot
# so the boot can reach generation verify (which then REFUSES the tampered gen).
LUKS_PASSPHRASE="${NMBL_LUKS_PASSPHRASE:-test}"
# Detector for NMBL's luks-password modal (same matcher as signed-gen-happy).
PASSWORD_PROMPT_RE='Enter LUKS passphrase|passphrase for cryptroot|Select NixOS Generation|Passphrase'
# cryptsetup's re-prompt after a rejected attempt; only then do we re-send.
REPROMPT_RE='Wrong password \(attempt|cryptsetup rejected the passphrase'

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
# The tampered generation BOOTING its normal multi-user system is a HARD FAIL: it
# must never reach the booted system. The signal is the booted system's autologin
# prompt `root@${CONFIG_NAME}` (services.getty.autologinUser=root,
# networking.hostName=${CONFIG_NAME}). Anchor the trailing host with `\b` so the
# match is the WHOLE enroll-twin hostname, never a longer suffix. The tampered
# gen lives inside the LUKS container, so NMBL only reaches generation verify
# +kexec AFTER cryptroot opens; on a tampered gen the pre-kexec guard refuses and
# reboots into rescue, so this prompt can only legitimately come from a NON-bad-
# gen shell (the refuse-terminus rescue console). We therefore do NOT treat it as
# a failure unconditionally — we scope it to a boot that was NOT refused (see the
# ordering check below).
BOOTED_RE="root@${CONFIG_NAME}\\b"

REFUSE_WATCH="${NMBL_REFUSE_WATCH:-150}"
# After a refuse, watch a bit longer to be SURE no shell appears and the bad
# gen never boots (the countdown defaults to 30s before auto-reboot).
ABSENCE_WATCH="${NMBL_ABSENCE_WATCH:-60}"
# Time to wait for the LUKS modal to flush before answering it.
PASSWORD_WAIT="${NMBL_PASSWORD_WAIT:-90}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "${SCRIPT_DIR}/lib.sh"

RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/nmbl-sb-badsig.XXXXXX")"

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

# The pristine disk to tamper. The flake app exports $NMBL_SB_DISK to the
# install-runtime-signed enroll qcow2 store path.
PRISTINE_DISK="${NMBL_SB_DISK:-}"
if [ -z "$PRISTINE_DISK" ] || [ ! -f "$PRISTINE_DISK" ]; then
  echo "FAIL: \$NMBL_SB_DISK is unset or not a file ('${PRISTINE_DISK}')." >&2
  echo "      Set it to the install-runtime-signed enroll qcow2." >&2
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
# the install-signed one from the runtime path.
echo "=== staging a writable disk copy to tamper ===" >&2
cp "$PRISTINE_DISK" "${RUN_DIR}/${CONFIG_NAME}.qcow2"
chmod 644 "${RUN_DIR}/${CONFIG_NAME}.qcow2"

# TAMPER: corrupt the detached generation signature sidecars on the FAT32 boot
# partition so the pre-kexec verify guard sees a BAD signature (not a missing
# one). The disko-luks layout is: sda1 = EF02 boot, sda2 = vfat ESP (/boot),
# sda3 = LUKS. The sidecars live under /nmbl/sigs/<gen-id>/{kernel,initrd}.sig on
# the ESP, OUTSIDE the LUKS container, so no key is needed. We overwrite each
# sidecar's leading bytes in place (a deterministic, content-independent
# corruption that the ML-DSA verifier rejects) but KEEP the file present, so this
# is a true bad-signature refusal, not a missing-sidecar one. We tamper EVERY
# generation's kernel AND initrd sidecar so whichever generation NMBL selects is
# refused regardless of its gen-id.
echo "=== tampering: corrupting the generation signature sidecar(s) on the boot partition ===" >&2

# A small host file of fixed garbage whose bytes we splice over each sidecar's
# head via `upload-offset` (writes local content into the remote file at the
# given offset, leaving the rest — and the file's existence — intact).
TAMPER_SRC="${RUN_DIR}/tamper.bin"
printf 'NMBL_BAD_SIGNATURE_TAMPER_XXXXXX' >"${TAMPER_SRC}"

# 1) Enumerate every sidecar path on the ESP. guestfish `find <dir>` prints each
#    entry's path RELATIVE to <dir> (no leading slash), one per line.
SIG_LIST="${RUN_DIR}/siglist.txt"
if ! guestfish --rw -a "${RUN_DIR}/${CONFIG_NAME}.qcow2" >"${SIG_LIST}" <<'GF'
run
mount /dev/sda2 /
find /nmbl/sigs
umount /
GF
then
  echo "FAIL: guestfish could not enumerate the signature sidecars" >&2
  exit 1
fi

# Keep only the kernel/initrd .sig sidecar entries and re-anchor each as an
# absolute ESP path under /nmbl/sigs (find emitted them relative to that dir).
mapfile -t SIDECARS < <(
  grep -E '/(kernel|initrd)\.sig$' "${SIG_LIST}" \
    | sed -e 's#^/*#/nmbl/sigs/#' || true
)
if [ "${#SIDECARS[@]}" -eq 0 ]; then
  echo "FAIL: no kernel/initrd .sig sidecars found under /nmbl/sigs on the ESP" >&2
  echo "      (enumeration output below) — the enroll disk may be unsigned." >&2
  cat "${SIG_LIST}" >&2 || true
  exit 1
fi

# 2) Corrupt every sidecar's head in a SINGLE guestfish session (one appliance
#    boot for all of them). Build the command script: mount, then one
#    upload-offset per sidecar, then unmount.
TAMPER_GF="${RUN_DIR}/tamper.gf"
{
  echo "run"
  echo "mount /dev/sda2 /"
  for sc_abs in "${SIDECARS[@]}"; do
    echo "upload-offset ${TAMPER_SRC} ${sc_abs} 0"
  done
  echo "umount /"
} >"${TAMPER_GF}"

if ! guestfish --rw -a "${RUN_DIR}/${CONFIG_NAME}.qcow2" -f "${TAMPER_GF}"; then
  echo "FAIL: guestfish could not corrupt the signature sidecar(s)" >&2
  echo "      (tamper script below)" >&2
  cat "${TAMPER_GF}" >&2 || true
  exit 1
fi
echo "=== tamper step done (corrupted ${#SIDECARS[@]} sidecar(s)) ===" >&2

echo "=== launching SB+TPM VM via ${RUNNER} (workdir ${RUN_DIR}) ===" >&2
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

# Feed NMBL stage-0's LUKS passphrase modal so cryptroot opens and the boot can
# proceed to generation verify (which then REFUSES the tampered generation).
#
# DETECT-THEN-ANSWER-ONCE, exactly as signed-gen-happy does: WAIT for the modal
# text to appear, then send the passphrase EXACTLY ONCE. A blind fixed-cadence
# re-send would buffer stray chars before the box is input-ready and pile up
# FAILED unlock attempts. We only re-send if cryptsetup genuinely RE-PROMPTS, and
# cap total attempts so a wrong/raced first try gets at most a couple of careful
# retries. `send_cmd` appends the newline so each attempt submits.
echo "=== feeding the LUKS passphrase so cryptroot opens ===" >&2
if wait_for "$PASSWORD_PROMPT_RE" "$PASSWORD_WAIT"; then
  echo "=== luks-password modal detected; answering once ===" >&2
else
  echo "=== modal text not yet flushed; answering on best-effort timing ===" >&2
fi
sleep 3
send_cmd "$LUKS_PASSPHRASE"
attempts=1
MAX_ATTEMPTS=3
reprompts_answered=0

# Wait for the refuse — re-sending the passphrase ONLY on a NEW genuine
# cryptsetup re-prompt (capped), never on a blind timer. The refuse is the PASS
# condition here: the tampered generation must be rejected at the verify guard.
echo "=== asserting NMBL REFUSES the tampered generation (up to ${REFUSE_WATCH}s) ===" >&2
refused=false
for _ in $(seq 1 "$((REFUSE_WATCH / 5))"); do
  if seen_in_history "$REFUSE_RE"; then
    refused=true
    break
  fi
  # If the bad gen somehow BOOTED un-refused, fail fast — that is the worst case.
  if seen_in_history "$BOOTED_RE" && ! seen_in_history "$REFUSE_RE"; then
    echo "FAIL: the tampered generation BOOTED (root shell of ${CONFIG_NAME}) with" >&2
    echo "      NO refuse marker — the bad gen was kexec'd instead of being rejected." >&2
    exit 1
  fi
  # Re-answer ONLY a NEW cryptsetup re-prompt (our previous answer raced/failed),
  # under the attempt cap, and never once we are past cryptroot.
  if [ "$attempts" -lt "$MAX_ATTEMPTS" ] && ! seen_in_history "kexec|${BOOTED_RE}|${REFUSE_RE}"; then
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
if [ "$refused" != true ]; then
  echo "FAIL: no refuse/rescue marker within ${REFUSE_WATCH}s — the bad gen was NOT refused" >&2
  exit 1
fi
echo "=== PASS: NMBL refused the tampered generation ===" >&2

# No emergency shell may EVER be offered: the refuse terminus is a non-
# interactive countdown, never the shell-offering emergency machinery
# (R-1/R-13/FIX-35). This is an unconditional ABSENCE assertion.
#
# The tampered GENERATION must never boot its normal multi-user system. That is a
# failure ONLY if it happens WITHOUT a refuse — i.e. NMBL kexec'd the tampered
# gen instead of rejecting it. Once the refuse has fired (asserted above), the
# bad gen cannot have kexec'd, so any `root@${CONFIG_NAME}` shell that shows up is
# the legitimate post-refuse rescue/terminus console, NOT the bad gen. We
# therefore SCOPE the booted-shell failure to ordering: a booted shell is a HARD
# FAIL iff it appears with NO preceding refuse. A shell that appears only AFTER
# the first refuse line is benign. This keeps the real security check (a true un-
# refused bad-gen boot still HARD-FAILS) while not flagging the rescue shell the
# complete serial now captures. Both checks poll over a watch window so a late
# event still trips.
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
