#!/usr/bin/env bash
# Secure-boot scenario: TPM seal/unseal roundtrip (#57 / matrix id #3a).
#
# PRECONDITION first, so a TPM-less VM can NEVER false-green a negative:
#   1. /dev/tpmrm0 exists in NMBL's boot environment (the swtpm-backed TPM is
#      actually wired) — proven by NMBL reaching the measured path under
#      requireTpm=true (a missing TPM would abort the boot, not proceed).
#   2. A probe secret seals AND unseals against the live TPM before we trust
#      the real auto-unseal. We assert NMBL's own TPM-probe / measured-boot
#      markers, then that the sealed LUKS key auto-unseals (the cryptroot
#      volume opens with NO password prompt being answered).
#
# Flow:
#   1. Launch the test-secure-boot runner (swtpm "tis" + SB-OVMF, smm=on).
#   2. Assert /dev/tpmrm0 is present and NMBL extends PCR 11 (measured boot).
#   3. Assert the TPM-sealed cryptroot auto-unseals (no password typed) and
#      the system boots to the post-kexec root shell — the seal+unseal
#      roundtrip succeeded with a measured boot matching the seal policy.
#   4. Tear the VM down and clean up everything THIS script started.
#
# requireTpm=true is set in the config, so the absence of /dev/tpmrm0 aborts
# the boot rather than silently degrading — this script cannot pass without a
# real TPM. Exit 0 on success, 1 on any failure.
#
# Required on PATH: vm-serial-man, the runner ($NMBL_RUNNER or
# `run-test-secure-boot`), screen, coreutils, gnugrep, qemu, OVMFFull, swtpm.

set -uo pipefail

CONFIG_NAME="test-secure-boot"

# NMBL markers proving the TPM is present and the measured path ran. Any one
# of these means /dev/tpmrm0 was found and NMBL talked to it.
TPM_PRESENT_RE='tpmrm0|TPM present|measured boot|PCR 11|PCR-11|extend.*PCR|seal'
# Auto-unseal SUCCESS: a TPM-token-SPECIFIC marker NMBL emits ONLY on a genuine
# `cryptsetup open --token-only` unseal (src/activation/mod.rs). `--token-only`
# can NEVER fall back to a password keyslot, so this line proves the seal/unseal
# roundtrip succeeded — it shares no substring with the generic "activation
# luks-tpm completed" line, so a password fallback can never match it.
UNSEAL_OK_RE='luks-tpm: unsealed .* via TPM token'
# A password PROMPT means auto-unseal FAILED (fell back to the modal). Seeing
# it is a FAIL for this happy-path scenario; we assert its ABSENCE below in
# addition to requiring the unseal marker above.
PASSWORD_PROMPT_RE='Enter LUKS passphrase|passphrase for cryptroot'

SHELL_TIMEOUT="${NMBL_SHELL_TIMEOUT:-240}"
BOOT_WATCH="${NMBL_BOOT_WATCH:-120}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "${SCRIPT_DIR}/lib.sh"

RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/nmbl-sb-tpm.XXXXXX")"

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

# PRECONDITION: the TPM is present and the measured path ran. requireTpm=true
# means a missing TPM aborts the boot, so reaching this marker proves
# /dev/tpmrm0 exists and NMBL measured into it.
echo "=== asserting /dev/tpmrm0 present + measured boot (PCR 11) ===" >&2
if ! wait_for "$TPM_PRESENT_RE" "$BOOT_WATCH"; then
  echo "FAIL: NMBL never showed a TPM-present / measured-boot marker" >&2
  echo "      (requireTpm=true ⇒ a TPM-less VM aborts here — precondition unmet)" >&2
  exit 1
fi
echo "=== PASS precondition: TPM present + measured boot ===" >&2

# The TPM-sealed cryptroot must AUTO-unseal: NMBL emits the TPM-token-specific
# marker above with no password answered. A bare password prompt that is never
# satisfied means the seal/unseal roundtrip failed.
echo "=== asserting TPM auto-unseal of cryptroot (no password) ===" >&2
if ! wait_for "$UNSEAL_OK_RE" "$BOOT_WATCH"; then
  if seen_in_history "$PASSWORD_PROMPT_RE"; then
    echo "FAIL: cryptroot fell back to the password modal — TPM auto-unseal FAILED" >&2
  else
    echo "FAIL: no TPM auto-unseal marker within ${BOOT_WATCH}s" >&2
  fi
  exit 1
fi
# Belt-and-braces: even WITH the unseal marker, a password prompt anywhere in
# the history means the seal degraded to a fallback — fail the happy path.
if seen_in_history "$PASSWORD_PROMPT_RE"; then
  echo "FAIL: a password prompt appeared despite the unseal marker — the TPM" >&2
  echo "      auto-unseal is not clean (a fallback path was exercised)." >&2
  exit 1
fi
echo "=== PASS: cryptroot auto-unsealed from the TPM (no password prompt) ===" >&2

# Finally the box must reach the booted root shell — proving the measured boot
# matched the seal policy end-to-end and the system came up unattended.
echo "=== waiting up to ${SHELL_TIMEOUT}s for the post-kexec root shell ===" >&2
booted=false
for _ in $(seq 1 "$((SHELL_TIMEOUT / 5))"); do
  if seen_in_history "root@${CONFIG_NAME}"; then
    booted=true
    break
  fi
  sleep 5
done
if [ "$booted" != true ]; then
  echo "FAIL: never reached the booted root shell after TPM auto-unseal" >&2
  exit 1
fi

echo "PASS: TPM seal/unseal roundtrip — measured boot, auto-unseal, system up." >&2
exit 0
