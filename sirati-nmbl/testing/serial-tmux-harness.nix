# Modular building block: tmux-rendered serial harness for VMs.
#
# Renderer that takes a `testArtefact` (see `test-artefact.nix`) and
# produces a `writeShellApplication` runner. The runner:
#
#   1. Creates a tmux session of the caller-chosen name with a single
#      pane whose command keeps the pane's pty alive (`sleep infinity`).
#      tmux allocates a pty pair for that pane internally.
#
#   2. Asks tmux for the pane's pty slave path:
#        tmux display -t NAME -p '#{pane_tty}'
#      yielding e.g. `/dev/pts/47`.
#
#   3. Launches QEMU directly against that pty with `-serial /dev/pts/47`.
#      QEMU opens the slave as its serial backend; the emulated UART
#      reads from / writes to it. tmux's pane master is on the other
#      side, so:
#        - guest UART output → /dev/pts/47 → tmux pane master → operator
#        - operator keystrokes → tmux pane master → /dev/pts/47 → guest UART
#      with no broker process, no Unix socket, no socat.
#
#   4. A `resize-watcher.sh` companion polls the tmux pane size and
#      emits the xterm `CSI 8;rows;colst` sequence via `tmux send-keys
#      -H` on every change. A guest-side TUI that parses the sequence
#      (NMBL's ratatui passphrase modal does not yet — that is a
#      follow-up) can adapt its layout to operator-driven pane resizes.
#      UART hardware has no winsize concept, so kernel TIOCSWINSZ
#      propagation is not available; the ANSI reply is the only path.
#
# This is a *renderer*: it does not know which test produced the
# artefact (direct-kernel kexec, full BIOS, UEFI, LUKS, btrfs, …).
# Composability is achieved via `test-artefact.nix`; orthogonal axes
# (which test × which display) keep N+M instead of N×M complexity.

{
  nixpkgs,
  system ? "x86_64-linux",
}:

let
  pkgs = nixpkgs.legacyPackages.${system};

  resizeWatcherScript = pkgs.writeShellApplication {
    name = "tmux-serial-resize-watcher";
    runtimeInputs = with pkgs; [
      tmux
      coreutils
      gawk
    ];
    text = builtins.readFile ./serial-tmux-harness/resize-watcher.sh;
  };

  mkTmuxSerialRunner =
    {
      artefact,
      tmuxSession ? null,
      resizePollMs ? 250,
    }:
    let
      sessionName = if tmuxSession != null then tmuxSession else artefact.name;

      diskCopyCommands = builtins.concatStringsSep "\n" (
        builtins.genList (
          idx:
          let
            d = builtins.elemAt artefact.disks idx;
            format = d.format or "qcow2";
            copy = d.copyOnLaunch or true;
            target = "\"$WORK_DIR/disk-${toString idx}.${format}\"";
          in
          if copy then
            ''
              if [ ! -f ${target} ]; then
                cp "${d.path}" ${target}
                chmod 644 ${target}
              fi''
          else
            ''
              ln -sf "${d.path}" ${target}''
        ) (builtins.length artefact.disks)
      );

      diskArgs = builtins.concatStringsSep " " (
        builtins.genList (
          idx:
          let
            d = builtins.elemAt artefact.disks idx;
            format = d.format or "qcow2";
            iface = d.iface or "virtio";
          in
          "-drive file=\"$WORK_DIR/disk-${toString idx}.${format}\",format=${format},if=${iface}"
        ) (builtins.length artefact.disks)
      );

      kernelInitrdArgs =
        if artefact.bootMode == "direct-kernel" then
          ''-kernel "$WORK_DIR/kernel" -initrd "$WORK_DIR/initrd" -append "${artefact.kernelArgs}"''
        else
          "";

      ovmfArgs =
        if artefact.bootMode == "uefi" then
          ''-drive if=pflash,format=raw,readonly=on,file="${artefact.ovmfCode}" -drive if=pflash,format=raw,file="$WORK_DIR/ovmf-vars.fd"''
        else
          "";

      ovmfStage =
        if artefact.bootMode == "uefi" then
          ''
            if [ ! -f "$WORK_DIR/ovmf-vars.fd" ]; then
              cp "${artefact.ovmfVars}" "$WORK_DIR/ovmf-vars.fd"
              chmod 644 "$WORK_DIR/ovmf-vars.fd"
            fi''
        else
          "";

      kernelStage =
        if artefact.bootMode == "direct-kernel" then
          ''
            ln -sf "${artefact.kernel}/bzImage" "$WORK_DIR/kernel"
            ln -sf "${artefact.initrd}/initrd" "$WORK_DIR/initrd"''
        else
          "";

    in
    pkgs.writeShellApplication {
      name = "tmux-serial-${artefact.name}";
      runtimeInputs = with pkgs; [
        tmux
        qemu_kvm
        coreutils
        gawk
        procps
        util-linux
      ];
      text = ''
        # ----------------------------------------------------------
        # tmux-serial harness: ${artefact.name} (${artefact.bootMode})
        # QEMU's `-serial /dev/pts/N` writes directly to the tmux
        # pane's pty slave. No socat, no mon:stdio multiplexing, no
        # auxiliary serial socket — just one pty, two ends.
        # ----------------------------------------------------------
        set -euo pipefail

        WORK_DIR="''${TMUX_SERIAL_WORKDIR:-$PWD/.tmux-serial-${artefact.name}}"
        SESSION_NAME="''${TMUX_SERIAL_SESSION:-${sessionName}}"

        KEEP_PREVIOUS=false
        while [ $# -gt 0 ]; do
            case "$1" in
                --workdir)   WORK_DIR="$2"; shift 2 ;;
                --workdir=*) WORK_DIR="''${1#--workdir=}"; shift ;;
                --session)   SESSION_NAME="$2"; shift 2 ;;
                --session=*) SESSION_NAME="''${1#--session=}"; shift ;;
                --keep)      KEEP_PREVIOUS=true; shift ;;
                --help|-h)
                    cat <<EOF
        tmux-serial-${artefact.name} — launch a tmux-attached VM
        Usage: tmux-serial-${artefact.name} [--workdir PATH] [--session NAME] [--keep]
          --workdir PATH   Working directory (default: \$PWD/.tmux-serial-${artefact.name})
          --session NAME   tmux session name (default: ${sessionName})
          --keep           Refuse to start if a QEMU from this workdir is still alive

        Architecture:
          - The runner creates a tmux session whose pane runs
            \`sleep infinity\` (just to keep the pane's pty alive).
          - It asks tmux for the pane's pty slave path with
            \`tmux display -p '#{pane_tty}'\` (e.g. /dev/pts/47).
          - QEMU is launched with \`-serial /dev/pts/47\`, so its
            UART backend opens the tmux pane's pty directly. No
            socat, no broker process, no Unix-domain serial socket.
          - QEMU's monitor still goes to its own Unix socket so
            scripted shutdown works without typing into the pane.
          - A background resize-watcher emits ESC[8;rows;colst into
            the pane on tmux resize for guest TUIs that parse it.

        Driving the session:
          attach:    tmux attach -t \$SESSION_NAME
          send keys: tmux send-keys -t \$SESSION_NAME 'test' Enter
          capture:   tmux capture-pane -t \$SESSION_NAME -p
          shutdown:  echo system_powerdown | \\
                       socat - UNIX-CONNECT:\$WORK_DIR/monitor.sock
                     tmux kill-session -t \$SESSION_NAME
        EOF
                    exit 0
                    ;;
                *)
                    echo "tmux-serial-${artefact.name}: unknown argument: $1" >&2
                    exit 2
                    ;;
            esac
        done

        echo "[harness] artefact: ${artefact.name}"
        echo "[harness] bootMode: ${artefact.bootMode}"
        echo "[harness] workdir:  $WORK_DIR"
        echo "[harness] session:  $SESSION_NAME"
        mkdir -p "$WORK_DIR"

        ${kernelStage}
        ${ovmfStage}
        ${diskCopyCommands}

        QEMU_PIDFILE="$WORK_DIR/qemu.pid"
        QEMU_LOG="$WORK_DIR/qemu.log"
        MONITOR_SOCK="$WORK_DIR/monitor.sock"
        if [ -f "$QEMU_PIDFILE" ]; then
            old_pid="$(cat "$QEMU_PIDFILE" 2>/dev/null || true)"
            if [ -n "''${old_pid:-}" ] && kill -0 "$old_pid" 2>/dev/null; then
                if [ "$KEEP_PREVIOUS" = "true" ]; then
                    echo "[harness] previous QEMU pid=$old_pid alive; --keep set, exiting"
                    exit 1
                fi
                echo "[harness] killing previous QEMU pid=$old_pid"
                kill "$old_pid" 2>/dev/null || true
                sleep 0.5
                kill -9 "$old_pid" 2>/dev/null || true
            fi
        fi
        if tmux has-session -t "$SESSION_NAME" 2>/dev/null; then
            echo "[harness] killing previous tmux session $SESSION_NAME"
            tmux kill-session -t "$SESSION_NAME" || true
        fi
        rm -f "$QEMU_PIDFILE" "$MONITOR_SOCK" "$QEMU_LOG"

        # 1. Create the tmux session. The pane's only job is to own
        #    its pty so /dev/pts/N stays open for QEMU to use as a
        #    serial backend. `sleep infinity` is the lightest sensible
        #    placeholder (no extra echo/buffering shenanigans).
        echo "[harness] creating tmux session..."
        tmux new-session -d -s "$SESSION_NAME" -x 200 -y 50 'sleep infinity'

        # 2. Discover the pane's pty slave path. `#{pane_tty}` is the
        #    documented tmux format key for "the absolute path to the
        #    pty backing this pane".
        PTS_PATH=""
        for _ in 1 2 3 4 5 6 7 8 9 10; do
            PTS_PATH="$(tmux display -t "$SESSION_NAME:0.0" -p '#{pane_tty}' 2>/dev/null || true)"
            if [ -n "''${PTS_PATH:-}" ] && [ -e "$PTS_PATH" ]; then break; fi
            sleep 0.1
        done
        if [ -z "''${PTS_PATH:-}" ] || [ ! -e "$PTS_PATH" ]; then
            echo "[harness] could not resolve tmux pane pty path (got '''$PTS_PATH''')" >&2
            tmux kill-session -t "$SESSION_NAME" || true
            exit 1
        fi
        echo "[harness] tmux pane pty: $PTS_PATH"
        echo "$PTS_PATH" >"$WORK_DIR/pty.path"

        # 3. Launch QEMU detached with -serial pointing at the pty.
        #    QEMU opens that path with O_RDWR and runs the emulated
        #    UART against it. Operator keystrokes flow from the tmux
        #    master end through the slave into QEMU; UART output goes
        #    the other direction. No broker.
        echo "[harness] starting QEMU bound to $PTS_PATH..."
        qemu-system-x86_64 \
            -machine q35,accel=kvm:tcg \
            -cpu max \
            -m ${toString artefact.memoryMb} \
            -smp ${toString artefact.cores} \
            -display none \
            -serial "$PTS_PATH" \
            -monitor "unix:$MONITOR_SOCK,server,nowait" \
            -daemonize \
            -pidfile "$QEMU_PIDFILE" \
            ${kernelInitrdArgs} \
            ${diskArgs} \
            ${ovmfArgs} \
            >"$QEMU_LOG" 2>&1

        if [ ! -f "$QEMU_PIDFILE" ]; then
            echo "[harness] QEMU failed to start; log:" >&2
            cat "$QEMU_LOG" >&2
            tmux kill-session -t "$SESSION_NAME" || true
            exit 1
        fi
        echo "[harness] QEMU pid=$(cat "$QEMU_PIDFILE")"

        # 4. Background resize watcher.
        TMUX_TARGET="$SESSION_NAME:0.0" \
        POLL_MS=${toString resizePollMs} \
            ${resizeWatcherScript}/bin/tmux-serial-resize-watcher \
            >"$WORK_DIR/resize-watcher.log" 2>&1 &
        WATCHER_PID=$!
        echo "$WATCHER_PID" >"$WORK_DIR/resize-watcher.pid"

        cat <<EOF

        [harness] READY.
          attach:    tmux attach -t $SESSION_NAME
          send keys: tmux send-keys -t $SESSION_NAME 'test' Enter
          capture:   tmux capture-pane -t $SESSION_NAME -p
          shutdown:  echo system_powerdown | ${pkgs.socat}/bin/socat - UNIX-CONNECT:$MONITOR_SOCK
                     tmux kill-session -t $SESSION_NAME
          pty:       $PTS_PATH
          logs:      $WORK_DIR/qemu.log
                     $WORK_DIR/resize-watcher.log
        EOF
      '';
    };
in
{
  inherit
    mkTmuxSerialRunner
    resizeWatcherScript
    ;
}
