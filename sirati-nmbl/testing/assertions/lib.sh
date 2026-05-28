# NMBL VM assertion helpers.
#
# Thin bash wrappers around the vm-serial-man client so individual test
# scripts (log-import.sh today, a stateful test later) stay short and
# share one definition of "what counts as a pass". Source this file; do
# not execute it. The single running manager's control socket is found
# automatically by the client (it scans /tmp for vm-serial-man-*.sock and
# skips stale ones); set VM_SOCKET to pin a specific socket if more than
# one manager is alive, otherwise leave it empty.
#
# CAVEAT baked in here: `vm-serial-man trigger` exits 0 even on timeout,
# printing "=== Trigger Timeout ===" / "did not match". So wait_for treats
# the *text* as the source of truth: a pass requires the pattern to appear
# AND the timeout banner to be absent.

# Marker string trigger prints when it gives up. Matching this means FAIL.
NMBL_TRIGGER_TIMEOUT_RE='=== Trigger Timeout ===|did not match'

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

# wait_for <pattern> [match_timeout_seconds]
# Waits for <pattern> to show up in NEW console output. Returns 0 only if
# the pattern is seen and the timeout banner never appeared; 1 otherwise.
# Echoes the captured trigger output to stderr so failures are debuggable.
wait_for() {
  local pattern="$1"
  local match_timeout="${2:-30}"
  local out
  local -a sock_args
  _socket_args sock_args
  out="$(vm-serial-man trigger "$pattern" \
    --match-timeout "$match_timeout" \
    --line-timeout 5 \
    "${sock_args[@]}" 2>&1)"
  printf '%s\n' "$out" >&2
  if printf '%s' "$out" | grep -Eq "$NMBL_TRIGGER_TIMEOUT_RE"; then
    echo "wait_for: FAIL (timeout) waiting for: $pattern" >&2
    return 1
  fi
  if ! printf '%s' "$out" | grep -Eq "$pattern"; then
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
