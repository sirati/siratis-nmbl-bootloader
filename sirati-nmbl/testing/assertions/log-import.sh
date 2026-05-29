#!/usr/bin/env bash
# Automated test: NMBL's pre-kexec log reaches the BOOTED system's journal.
#
# Flow:
#   1. Launch the test-gpt-qemu-kernel-invoke runner. That copies the VM
#      disk image, then starts vm-serial-man in a detached screen session
#      and exits once the control socket is up. This is the fastest path:
#      QEMU direct-kernel boots the real NMBL Rust init, which runs phases
#      1-6, flushes its log ring to /nmbl-log/nmbl.log, and kexecs into a
#      booted NixOS whose stage-1 nmbl-log-import oneshot replays every
#      line via `systemd-cat -t nmbl-init`.
#   2. Wait for the post-kexec root shell (autologin on ttyS0).
#   3. Assert `journalctl -t nmbl-init` is non-empty AND contains a NMBL
#      phase marker — proving the handoff actually carried real content.
#   4. Tear the VM down and clean up everything THIS script started.
#
# Exit 0 on success, 1 on any failure. Never kills screen sessions it did
# not create.
#
# Required on PATH: vm-serial-man, the runner ($NMBL_RUNNER or `run-test-
# gpt-qemu-kernel-invoke` resolved via $PATH), screen, coreutils, gnugrep.

set -uo pipefail

CONFIG_NAME="test-gpt-qemu-kernel-invoke"
JOURNAL_TAG="nmbl-init"
PHASE_MARKER="phase 1"
# Per-attempt wait (seconds) for the post-kexec shell after pressing Enter
# in the selector. Covers kexec + NixOS stage-1 (incl. nmbl-log-import) +
# stage-2 + autologin. The flake app's wall-clock cap is the hard backstop.
SHELL_TIMEOUT="${NMBL_SHELL_TIMEOUT:-90}"

# Resolve this script's directory so we can source the shared helpers
# regardless of the cwd nix run drops us in.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "${SCRIPT_DIR}/lib.sh"

# Per-run scratch dir. The runner writes its qcow2 + work dir relative to
# PWD, so confine all of that to a tempdir we own and remove on exit.
RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/nmbl-log-import.XXXXXX")"

cleanup() {
  local rc=$?
  echo "=== cleanup ===" >&2
  # Stop only the manager we started (auto-detect, single VM). Best effort.
  vm-serial-man stop >/dev/null 2>&1 || true
  # The runner names its screen session after the config; quit only that
  # one if it is still alive AND we are sure it is ours (we just stopped
  # the manager, so the session is now idle). Never touch other sessions.
  if screen -ls 2>/dev/null | grep -q "\.${CONFIG_NAME}\b"; then
    screen -S "${CONFIG_NAME}" -X quit >/dev/null 2>&1 || true
  fi
  rm -rf "${RUN_DIR}" >/dev/null 2>&1 || true
  exit "$rc"
}
trap cleanup EXIT INT TERM

# Locate the runner. nix puts it on PATH as run-<config>; allow override.
RUNNER="${NMBL_RUNNER:-run-${CONFIG_NAME}}"
if ! command -v "$RUNNER" >/dev/null 2>&1 && [ ! -x "$RUNNER" ]; then
  echo "FAIL: runner '${RUNNER}' not found on PATH" >&2
  exit 1
fi

echo "=== launching VM via ${RUNNER} (workdir ${RUN_DIR}) ===" >&2
cd "${RUN_DIR}"
# The runner builds artifacts and backgrounds QEMU in a screen session,
# then exits 0 once the manager socket is live.
if ! "$RUNNER"; then
  echo "FAIL: runner exited non-zero" >&2
  exit 1
fi

# NMBL stops at the phase-5 TUI generation selector. On timeout this
# config defaults to *reboot* (not boot), so left alone the VM cycles
# (selector -> 5s timeout -> reboot -> selector ...) and never kexecs.
# Each cycle re-renders the selector ("Generations" title / "Enter boot"
# hint), so loop: catch the selector, press Enter to boot the default
# (top) generation, then check whether the post-kexec shell came up. A
# bare send emits just the newline vm-serial-man appends = an Enter key.
# Bounded by attempts so a genuinely wedged VM still fails instead of
# hanging (the outer wall-clock cap in the flake app is the backstop).
echo "=== driving NMBL phase-5 generation selector (Enter = boot) ===" >&2
booted=false
for attempt in 1 2 3 4 5 6; do
  if wait_for "Generations|Enter boot" 45; then
    echo "=== selector up (attempt ${attempt}) — pressing Enter to boot ===" >&2
    send_cmd ""
  fi
  # Did pressing Enter get us into the booted NixOS root shell?
  if wait_for "root@${CONFIG_NAME}" "${SHELL_TIMEOUT}"; then
    booted=true
    break
  fi
  echo "WARN: no shell after attempt ${attempt}; retrying selector" >&2
done
if [ "$booted" != true ]; then
  echo "FAIL: never reached the booted root shell" >&2
  exit 1
fi

# Settle the prompt with a sentinel round-trip before issuing journalctl,
# so our command is not swallowed by late boot chatter.
send_cmd "echo NMBL_SHELL_READY"
if ! wait_for "NMBL_SHELL_READY" 30; then
  echo "FAIL: shell did not echo readiness sentinel" >&2
  exit 1
fi

echo "=== asserting journalctl -t ${JOURNAL_TAG} carries NMBL's pre-kexec log ===" >&2
if ! assert_journal_tag "${JOURNAL_TAG}" "${PHASE_MARKER}"; then
  echo "FAIL: NMBL pre-kexec log did not reach the booted journal" >&2
  exit 1
fi

echo "PASS: NMBL pre-kexec log reached the booted journal under tag ${JOURNAL_TAG}" >&2
exit 0
