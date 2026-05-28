#!/usr/bin/env bash
# Watch a tmux pane's size and, on every change, emit the xterm
# "report size" CSI sequence (ESC[8;ROWS;COLSt) into the pane's
# input stream via `tmux send-keys -H`.
#
# The pane is QEMU (or the pty owner connected to QEMU's UART
# backend), so bytes we send into the pane reach the guest's UART.
# A ratatui application reading from /dev/console (= ttyS0 on a
# serial boot) sees the CSI sequence in its input stream and can
# update its sense of (cols, rows) accordingly.
#
# Inputs (env):
#   TMUX_TARGET   — `tmux send-keys -t` target (session:window.pane)
#   POLL_MS       — Polling interval in milliseconds (default 250)

set -eu

: "${TMUX_TARGET:?TMUX_TARGET must be set}"
: "${POLL_MS:=250}"

# Helper: print the hex byte value (00..ff) of a single ASCII char.
# Bash's `printf "%02x" "'C"` (apostrophe-prefixed argument) yields
# the character's numeric value; the `'` is consumed by printf.
ascii_hex() {
    LC_ALL=C printf '%02x' "'$1"
}

last_size=""

while :; do
    current="$(tmux display -t "${TMUX_TARGET}" -p '#{pane_width}x#{pane_height}' 2>/dev/null || true)"
    if [ -z "${current}" ]; then
        # Pane gone (session killed, QEMU exited). Quiet exit.
        break
    fi
    if [ "${current}" != "${last_size}" ]; then
        cols="${current%x*}"
        rows="${current#*x}"

        # Build the hex byte list for ESC [ 8 ; ROWS ; COLS t in a
        # plain bash array — much less fragile than nested $() with
        # single-quote-prefixed printf arguments.
        bytes=("1b") # ESC

        for c_part in '[' '8' ';'; do
            bytes+=("$(ascii_hex "${c_part}")")
        done

        for ((i=0; i<${#rows}; i++)); do
            bytes+=("$(ascii_hex "${rows:i:1}")")
        done

        bytes+=("$(ascii_hex ';')")

        for ((i=0; i<${#cols}; i++)); do
            bytes+=("$(ascii_hex "${cols:i:1}")")
        done

        bytes+=("$(ascii_hex 't')")

        # `-H` makes send-keys interpret each whitespace-separated
        # argument as a hex byte. Any failure (pane gone mid-flight)
        # is non-fatal — we will notice on the next poll.
        if ! tmux send-keys -t "${TMUX_TARGET}" -H "${bytes[@]}" 2>/dev/null; then
            break
        fi

        last_size="${current}"
    fi
    sleep "$(awk -v ms="${POLL_MS}" 'BEGIN { printf "%.3f", ms/1000 }')"
done
