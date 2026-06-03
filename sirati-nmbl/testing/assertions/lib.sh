# NMBL VM assertion helpers.
#
# Thin bash wrappers around the vm-serial-man client so individual test
# scripts (log-import.sh today, a stateful test later) stay short and
# share one definition of "what counts as a pass". Source this file; do
# not execute it. The single running manager's control socket is found
# automatically by the client (it scans /tmp for vm-serial-man-*.sock and
# skips stale ones); set VM_SOCKET to pin a specific socket if more than
# one manager is alive, otherwise leave it empty. When a peer may be running
# OTHER managers concurrently (so auto-detection could latch onto a foreign
# VM), call `pin_vm_socket <config_name>` right after the runner starts the VM
# to pin VM_SOCKET to THIS scenario's own manager — see that helper below.
#
# CAVEAT baked in here: `vm-serial-man trigger` exits 0 even on timeout,
# printing "=== Trigger Timeout ===" / "did not match". So wait_for treats
# the *text* as the source of truth: a pass requires the pattern to appear
# AND the timeout banner to be absent.

# Marker string trigger prints when it gives up. Matching this means FAIL.
NMBL_TRIGGER_TIMEOUT_RE='=== Trigger Timeout ===|did not match'

# Console-marker matching is CASE-INSENSITIVE by design: firmware/NMBL banners
# vary in casing (OVMF prints "Verification failed"; NMBL "verify"/"Refusing"),
# and a scenario must not false-FAIL purely on letter case (audit F6). Both the
# vm-serial-man regex (Rust `regex`, via an inline `(?i)`) and the lib.sh grep
# confirmation (`grep -Ei`) honour it. The trigger-TIMEOUT banner above is
# matched case-sensitively (it is a fixed literal we emit, not guest output).
_ci_pattern() { printf '(?i)%s' "$1"; }

# Populate the named array with the optional `--socket <path>` argv.
# Empty VM_SOCKET → no args, so the client auto-detects the lone running
# manager. Usage: `local a; _socket_args a; cmd "${a[@]}"`.
_socket_args() {
  local -n _out="$1"
  _out=()
  if [ -n "${VM_SOCKET:-}" ]; then
    _out=(--socket "$VM_SOCKET")
  fi
}

# pin_vm_socket <config_name>
# Finds the vm-serial-man MANAGER process started for `--name <config_name>`,
# derives its control socket (/tmp/vm-serial-man-<pid>.sock — see manager/
# core.rs), waits for the socket to exist, and exports VM_SOCKET to it so every
# helper below (via _socket_args → `--socket <path>`) talks to THIS scenario's
# own VM rather than auto-detecting whichever lone manager happens to be alive.
#
# WHY: auto-detection only works when EXACTLY ONE manager is alive (see header).
# When a peer runs other VMs concurrently, each is a live manager whose serial
# buffer carries real NMBL output (`nmbl-init starting`, `phase 1:`), so an
# unpinned `find`/`trigger` can false-match a FOREIGN manager. Pinning the socket
# scopes every read+write to the scenario's own VM.
#
# DISAMBIGUATION (two traps avoided):
#   * the SCREEN wrapper — the manager runs as `screen -dmS <name> vm-serial-man
#     manager --name <name> …`, so BOTH the screen wrapper and the real manager
#     carry `--name <name>` in their argv. We only accept a process whose argv[0]
#     BASENAME is exactly `vm-serial-man` (the real manager); the wrapper's
#     argv[0] basename is `screen`/`SCREEN`, so it is skipped — and only the real
#     manager's PID names a socket anyway.
#   * the prefix trap — `test-secure-boot` must NOT match `test-secure-boot-
#     enroll`. We read /proc/<pid>/cmdline as NUL-separated argv and require an
#     argv ELEMENT equal to the WHOLE name (`--name` / `-n` then an exact-match
#     token), never a substring, so a longer config name can never be matched by
#     a shorter one.
# Errors loudly (return 1) unless EXACTLY ONE manager matches, so a real problem
# never silently falls back to auto-detecting a foreign manager.
pin_vm_socket() {
  local config_name="$1"
  if [ -z "$config_name" ]; then
    echo "pin_vm_socket: FAIL — empty config name" >&2
    return 1
  fi

  local pid cmd_file argv0 base
  local -a pids=()
  for cmd_file in /proc/[0-9]*/cmdline; do
    [ -r "$cmd_file" ] || continue
    # Read the NUL-separated argv into an array; exact-token matching below.
    local -a argv=()
    mapfile -d '' -t argv <"$cmd_file" 2>/dev/null || continue
    [ "${#argv[@]}" -gt 0 ] || continue
    # The REAL manager has argv[0] basename `vm-serial-man`; the screen wrapper
    # has `screen`/`SCREEN`, so this rejects the wrapper.
    argv0="${argv[0]}"
    base="${argv0##*/}"
    [ "$base" = "vm-serial-man" ] || continue
    # Must be the `manager` subcommand with `--name <exact>` / `-n <exact>`.
    local is_manager=false matched=false i
    for ((i = 1; i < ${#argv[@]}; i++)); do
      case "${argv[i]}" in
      manager) is_manager=true ;;
      --name | -n)
        if [ "${argv[i + 1]:-}" = "$config_name" ]; then
          matched=true
        fi
        ;;
      esac
    done
    if [ "$is_manager" = true ] && [ "$matched" = true ]; then
      pid="${cmd_file#/proc/}"
      pid="${pid%/cmdline}"
      pids+=("$pid")
    fi
  done

  if [ "${#pids[@]}" -eq 0 ]; then
    echo "pin_vm_socket: FAIL — no vm-serial-man manager found for --name '${config_name}'" >&2
    return 1
  fi
  if [ "${#pids[@]}" -gt 1 ]; then
    echo "pin_vm_socket: FAIL — ${#pids[@]} managers match --name '${config_name}' (pids: ${pids[*]})" >&2
    return 1
  fi

  local sock="/tmp/vm-serial-man-${pids[0]}.sock"
  # The manager binds its socket a moment after fork/exec; wait briefly for it
  # to appear AND be a connectable socket node before pinning.
  for _ in $(seq 1 50); do
    if [ -S "$sock" ]; then
      break
    fi
    sleep 0.2
  done
  if [ ! -S "$sock" ]; then
    echo "pin_vm_socket: FAIL — socket '${sock}' (manager pid ${pids[0]}) never appeared" >&2
    return 1
  fi

  export VM_SOCKET="$sock"
  echo "pin_vm_socket: pinned VM_SOCKET=${VM_SOCKET} (manager pid ${pids[0]} for '${config_name}')" >&2
  return 0
}

# wait_for <pattern> [match_timeout_seconds]
# Waits for <pattern> to show up in console output. Returns 0 only if the
# pattern is seen and the timeout banner never appeared; 1 otherwise.
# Echoes the captured trigger output to stderr so failures are debuggable.
#
# `trigger` only watches NEW output, which races a response that already
# landed in the preceding `send_cmd`'s own capture window. So on a trigger
# timeout we make one more pass over the captured HISTORY before declaring
# failure — this strictly *adds* a way to succeed (the pattern still has to
# be present somewhere), so the empty-journal negative case, which never
# emits the pattern at all, still fails as before.
wait_for() {
  local pattern="$1"
  local match_timeout="${2:-30}"
  local out
  local -a sock_args
  _socket_args sock_args
  out="$(vm-serial-man trigger "$(_ci_pattern "$pattern")" \
    --match-timeout "$match_timeout" \
    --line-timeout 5 \
    "${sock_args[@]}" 2>&1)"
  printf '%s\n' "$out" >&2
  if printf '%s' "$out" | grep -Eq "$NMBL_TRIGGER_TIMEOUT_RE"; then
    if seen_in_history "$pattern"; then
      return 0
    fi
    echo "wait_for: FAIL (timeout) waiting for: $pattern" >&2
    return 1
  fi
  # Drop the trigger's own banner line ("=== Waiting for trigger: <pattern> ===")
  # before confirming the pattern — it echoes the pattern, which would otherwise
  # always match even when only the banner (not a real serial line) is present.
  if ! printf '%s' "$out" | grep -v '^=== Waiting for trigger:' | grep -Eiq "$pattern"; then
    echo "wait_for: FAIL (pattern absent) waiting for: $pattern" >&2
    return 1
  fi
  return 0
}

# send_cmd <text>
# Types <text> into the console (vm-serial-man appends the newline).
send_cmd() {
  local -a sock_args
  _socket_args sock_args
  vm-serial-man send "$1" "${sock_args[@]}" >/dev/null 2>&1
}

# seen_in_history <pattern>
# Returns 0 if <pattern> already appears anywhere in the captured console
# HISTORY. Unlike wait_for (which arms a trigger and only sees NEW output),
# this catches a line that already scrolled past — e.g. an idle autologin
# prompt that rendered once and is now silent. Use it to detect a state
# that has been reached rather than to wait for one to occur.
seen_in_history() {
  local pattern="$1"
  local -a sock_args
  _socket_args sock_args
  # find DOES the regex match itself and prints a "--- Match #N (line L) ---"
  # header per hit. Detect a hit by that header — do NOT re-grep find's output
  # for the pattern: find echoes the pattern in its "=== Searching for pattern:
  # <p> ===" banner, so re-grepping would ALWAYS match the banner (a false
  # positive that flips every absence check the moment the manager is reachable).
  vm-serial-man find "$(_ci_pattern "$pattern")" "${sock_args[@]}" 2>/dev/null \
    | grep -qE '^--- Match #[0-9]'
}

# seen_count <pattern>
# Prints the number of HISTORY lines matching <pattern> (0 if none / on error).
# Used to count distinct repeated frames — e.g. cryptsetup's incrementing
# "Wrong password (attempt N)" re-prompts — so a caller can answer each NEW
# re-prompt exactly once instead of re-answering the same one repeatedly.
seen_count() {
  local pattern="$1"
  local -a sock_args
  _socket_args sock_args
  # Count the per-match "--- Match #N" headers, NOT lines matching the pattern:
  # find echoes the pattern in its search banner (see seen_in_history), which
  # would inflate the count by the banner lines.
  vm-serial-man find "$(_ci_pattern "$pattern")" "${sock_args[@]}" 2>/dev/null \
    | grep -cE '^--- Match #[0-9]' || true
}

# first_match_line <pattern>
# Prints the 1-indexed HISTORY line number of the FIRST line matching <pattern>,
# or nothing (empty) if the pattern never appears. `vm-serial-man find` prints
# one "--- Match #N (line L) ---" header per match (L already 1-indexed, see
# client/find.rs:113), in history order, so the first such header carries the
# earliest matching line. Used to ORDER two markers against each other — e.g. to
# tell a forbidden shell that booted BEFORE a refuse (a real security failure)
# apart from a legitimate shell that only appears AFTER the refuse fired.
first_match_line() {
  local pattern="$1"
  local -a sock_args
  _socket_args sock_args
  vm-serial-man find "$(_ci_pattern "$pattern")" --first 1 "${sock_args[@]}" 2>/dev/null \
    | sed -n 's/^--- Match #[0-9]* (line \([0-9]*\)) ---$/\1/p;T;q'
}

# assert_journal_tag <tag> [phase_match]
# Sends a journalctl query for <tag> and asserts a positive line count.
# The shell prints a sentinel "NMBL_LINES_<n>" we can match deterministically
# regardless of journalctl's own wrapping. When phase_match is given it must
# additionally appear in the tagged journal output. Returns 0 on success.
assert_journal_tag() {
  local tag="$1"
  local phase_match="${2:-}"

  # Print a count sentinel that wait_for can match exactly. Using a unique
  # token avoids confusing the echoed command with its output.
  send_cmd "journalctl -t ${tag} --no-pager | wc -l | sed 's/^/NMBL_LINES_/'"

  # A populated journal yields NMBL_LINES_<positive>. An empty journal
  # yields NMBL_LINES_0, which this pattern deliberately does NOT match,
  # so an absent log makes wait_for time out and fail.
  if ! wait_for 'NMBL_LINES_[1-9][0-9]*' 30; then
    echo "assert_journal_tag: FAIL — journalctl -t ${tag} produced no lines" >&2
    return 1
  fi
  echo "assert_journal_tag: PASS — journalctl -t ${tag} is non-empty" >&2

  if [ -n "$phase_match" ]; then
    send_cmd "journalctl -t ${tag} --no-pager | grep -c '${phase_match}' | sed 's/^/NMBL_PHASE_/'"
    if ! wait_for 'NMBL_PHASE_[1-9][0-9]*' 30; then
      echo "assert_journal_tag: FAIL — '${phase_match}' not found in ${tag} journal" >&2
      return 1
    fi
    echo "assert_journal_tag: PASS — '${phase_match}' present in ${tag} journal" >&2
  fi
  return 0
}
