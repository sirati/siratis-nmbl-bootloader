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
#      signature-failure marker appears on serial.
#   4. Assert the system reaches the post-kexec root shell (cryptroot opened,
#      verify passed, the generation was kexec'd, the new system booted).
#   5. From that shell, assert NMBL's enforce-mode `signature verified` marker
#      is present in the IMPORTED `nmbl-init` journal — positive proof the verify
#      guard ran+passed. It cannot be read from live serial: NMBL holds the TUI
#      console during verify and suppresses its stderr branch. (The companion
#      `measure: extended PCR-11` marker is emitted AFTER the kexec cpio-log
#      freeze, so it is unrecoverable post-kexec; measured-boot is proven by the
#      tpm-roundtrip scenario instead — see the marker block below.)
#   6. Tear the VM down and clean up everything THIS script started.
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
# Detector for NMBL's luks-password modal. The modal IS captured in history now
# that the manager survives the boot (idle-flush flushes the un-terminated
# ratatui frame), so we DETECT it and answer ONCE — see the typing block below.
# Matches the prompt line the enroll twin renders ("Enter LUKS passphrase for
# cryptroot …") and, as a fallback, the box title + permanent generation row.
PASSWORD_PROMPT_RE='Enter LUKS passphrase|passphrase for cryptroot|Select NixOS Generation|Passphrase'
# cryptsetup's re-prompt after a rejected attempt. NMBL repaints a
# "Wrong password (attempt N)" box ("cryptsetup rejected the passphrase") and
# re-shows the modal. Seeing this AFTER we already answered means our one
# passphrase was wrong/raced — only then do we (carefully, capped) re-send.
REPROMPT_RE='Wrong password \(attempt|cryptsetup rejected the passphrase'
# The booted-system prompt (getty autologin). networking.hostName == the config
# name (vm-config.nix), so the post-kexec shell prints "root@<config-name>".
# Unlike the modal, this is a newline-terminated line and flushes to history.
BOOTED_RE="root@${CONFIG_NAME}"

# A REFUSAL (any form) on the happy path is a HARD FAIL: the signed generation
# should verify. Covers the refuse countdown, the policy-refused terminus, and
# the signature-failure stage markers.
REFUSE_RE='RebootIntoRescue|refuse|Refusing|signature (verification )?failed|PolicyRefused|bad signature|reboot.*rescue|countdown'
# POSITIVE proof the verify guard actually RAN and PASSED — not merely that the
# box booted (an accidentally-non-enforcing build would also reach the autologin
# shell). NMBL emits `signature verified: generation <n> kernel+initrd OK
# (enforce)` ONLY on the enforce-mode verify-OK arm (src/boot/handoff.rs:220),
# and it does so BEFORE the kexec cpio-log is snapshotted (handoff.rs:307) and
# frozen (handoff.rs:321) — so the line IS carried into the next kernel's
# /nmbl-log/nmbl.log and replayed into the booted journal under the `nmbl-init`
# tag (lib/modules/log-import.nix). We assert it from the IMPORTED journal, NOT
# from live serial: NMBL holds the interactive console during verify
# (set_tui_active), which suppresses the `nmbl_*!` stderr branch, so the marker
# never reaches LIVE serial on this console-holding boot — but it is recoverable
# post-kexec via the journal. `JOURNAL_TAG` is the systemd-cat tag; the phrase is
# a plain substring (no regex metachars) safe for assert_journal_tag's grep.
JOURNAL_TAG="nmbl-init"
VERIFY_OK_PHRASE="signature verified"
# NOTE: the old live-serial `measure: extended PCR-11` assertion is GONE. That
# marker is emitted at handoff.rs:344 (measure_handoff) AFTER the cpio log is
# frozen at handoff.rs:321, so it is structurally ABSENT from the post-kexec
# `nmbl-init` journal AND console-suppressed on live serial — it is unrecoverable
# here without an NMBL-core reorder (out of scope). Measured-boot is covered by
# the tpm-roundtrip scenario + nmbl-init-rs measure_tests.rs instead.

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
# DETECT-THEN-ANSWER-ONCE: the modal IS in the captured history now (the manager
# survives the boot and idle-flush flushes the un-terminated ratatui frame), so
# we WAIT for the modal text to appear, then send the passphrase EXACTLY ONCE.
# The old blind fixed-cadence re-send buffered stray chars before the box was
# input-ready and piled up FAILED unlock attempts until NMBL hit its retry cap
# and REFUSED (locking the TPM) — that is the bug. We only re-send if cryptsetup
# genuinely RE-PROMPTS ("Wrong password (attempt N)"), and we cap total attempts
# so a wrong/raced first try gets at most a couple of careful retries, never a
# rapid loop. `send_cmd` appends the newline so each attempt submits.
echo "=== feeding the LUKS passphrase so cryptroot opens ===" >&2
# Wait for the modal to actually appear before typing (typing into a pre-modal
# console buffers stray chars). wait_for polls history + arms a trigger.
if wait_for "$PASSWORD_PROMPT_RE" 90; then
  echo "=== luks-password modal detected; answering once ===" >&2
else
  # The frame may not have flushed yet but the boot could still be at the modal;
  # fall through and answer anyway rather than stall (a late flush is common).
  echo "=== modal text not yet flushed; answering on best-effort timing ===" >&2
fi
# A brief settle so the modal is fully input-ready, then the single answer.
sleep 3
send_cmd "$LUKS_PASSPHRASE"
attempts=1
MAX_ATTEMPTS=3
# Number of distinct cryptsetup re-prompts we have ALREADY answered. Each genuine
# re-prompt adds a NEW "Wrong password (attempt N)" frame to history, so we count
# those frames and only answer ones we have not yet responded to — this prevents
# the same re-prompt from being answered repeatedly (the pileup that triggers
# REFUSE) while still recovering from a genuine re-prompt.
reprompts_answered=0

# Wait for the booted root shell. Re-send ONLY on a NEW genuine cryptsetup
# re-prompt (capped) — never on a blind timer. A refusal at any point is an
# immediate failure: the signed generation must verify and boot.
echo "=== waiting up to ${SHELL_TIMEOUT}s for the post-kexec root shell ===" >&2
booted=false
for _ in $(seq 1 "$((SHELL_TIMEOUT / 5))"); do
  if seen_in_history "$REFUSE_RE"; then
    echo "FAIL: NMBL REFUSED a correctly-signed generation (refusal marker seen)" >&2
    exit 1
  fi
  if seen_in_history "$BOOTED_RE"; then
    booted=true
    break
  fi
  # Count distinct re-prompt frames currently in history. If a NEW one appeared
  # (our previous answer was wrong/raced) and we are under the attempt cap,
  # answer it ONCE. seen_count returns the number of matching history lines.
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

# POSITIVE verify proof, read from the IMPORTED journal (NOT live serial).
# Booting to a shell is NOT enough: a build where the secure-boot feature
# silently didn't engage would also reach the autologin shell. NMBL emits the
# enforce-mode `signature verified` marker before the kexec cpio-log freeze, so
# it survives into the booted system's journal under tag `nmbl-init` — but it is
# console-suppressed on live serial (the verify runs while NMBL owns the TUI),
# so we MUST query the journal, exactly as testing/assertions/log-import.sh
# proves the nmbl-init tag carries NMBL's pre-kexec transcript.
echo "=== asserting the verify guard RAN+PASSED via the nmbl-init journal ===" >&2
if ! assert_journal_tag "${JOURNAL_TAG}" "${VERIFY_OK_PHRASE}"; then
  echo "FAIL: no '${VERIFY_OK_PHRASE}' line in the ${JOURNAL_TAG} journal — the" >&2
  echo "      enforce-mode verify guard did NOT run/pass (signing may have" >&2
  echo "      silently not engaged; the box would still boot but emit no marker)." >&2
  exit 1
fi
echo "=== PASS: '${VERIFY_OK_PHRASE}' present in the nmbl-init journal ===" >&2

echo "PASS: signed generation verified (journal) + kexec'd — system booted." >&2
exit 0
