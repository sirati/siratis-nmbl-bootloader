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
# The luks-password passphrase modal NMBL renders when the cryptroot needs the
# operator's passphrase. Seeing it means "type the passphrase", not a failure.
PASSWORD_PROMPT_RE='Enter LUKS passphrase|passphrase for cryptroot'

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
# can proceed to generation verify+measure+kexec. The modal may take a moment
# to render after the runner returns; feed the passphrase as soon as it shows
# (and once more on first appearance below in case it rendered late).
echo "=== feeding the LUKS passphrase so cryptroot opens ===" >&2
for _ in $(seq 1 24); do
  if seen_in_history "$PASSWORD_PROMPT_RE"; then
    send_cmd "$LUKS_PASSPHRASE"
    break
  fi
  if seen_in_history "kexec|root@${CONFIG_NAME}"; then
    break
  fi
  sleep 5
done

# Wait for the booted root shell. If a refusal appears at any point it is an
# immediate failure (checked after, via history) — the signed generation must
# verify and boot.
echo "=== waiting up to ${SHELL_TIMEOUT}s for the post-kexec root shell ===" >&2
booted=false
for _ in $(seq 1 "$((SHELL_TIMEOUT / 5))"); do
  if seen_in_history "$REFUSE_RE"; then
    echo "FAIL: NMBL REFUSED a correctly-signed generation (refusal marker seen)" >&2
    exit 1
  fi
  if seen_in_history "root@${CONFIG_NAME}"; then
    booted=true
    break
  fi
  # Re-feed the passphrase if the modal is still up (slow/late render) and we
  # have not yet kexec'd into the generation.
  if seen_in_history "$PASSWORD_PROMPT_RE" && ! seen_in_history "kexec|root@${CONFIG_NAME}"; then
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
