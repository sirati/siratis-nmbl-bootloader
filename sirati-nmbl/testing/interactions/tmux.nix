# Interaction renderer: tmux pane hosting QEMU directly via the
# pane's pty.
#
# This is a thin wrapper around the existing serial-tmux harness
# (testing/serial-tmux-harness.nix) — it adds support for the new
# artefact fields:
#   - `startMode`        used in banner text + help
#   - `runtimeDisks`     if true, exposes a `--disks` CLI flag
#                        whose comma-separated argument is spliced
#                        into the per-disk QEMU `-drive` lines
#   - `diskAccess`       "copy" (default), "snapshot" (use QEMU
#                        `-snapshot`, no file mutation), or
#                        "readonly" (`readonly=on` per drive)
#
# Used today by `kvm-kexec-<target>-tmux` and
# `kvm-kexec-installed-<target>-tmux` apps. The legacy
# `tmux-serial-<configName>` apps are kept as aliases by compose.nix.
{
  nixpkgs,
  system ? "x86_64-linux",
}:

let
  pkgs = nixpkgs.legacyPackages.${system};
  lib = nixpkgs.lib;

  resizeWatcherScript = pkgs.writeShellApplication {
    name = "tmux-serial-resize-watcher";
    runtimeInputs = with pkgs; [
      tmux
      coreutils
      gawk
    ];
    text = builtins.readFile ../serial-tmux-harness/resize-watcher.sh;
  };

  mkRunner =
    {
      artefact,
      tmuxSession ? null,
      resizePollMs ? 250,
    }:
    let
      sessionName = if tmuxSession != null then tmuxSession else artefact.name;
      diskAccess = artefact.diskAccess or "copy";
      hasRuntimeDisks = artefact.runtimeDisks or false;

      # Disks that are present at evaluation time get staged via cp /
      # ln; runtime disks are spliced in from $RUNTIME_DISKS at launch.
      staticDiskCopyCommands = lib.concatStringsSep "\n" (
        lib.imap0
          (
            idx: d:
            if (d.path or null) == null then
              ""
            else
              let
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
          )
          artefact.disks
      );

      perDiskRoFlag = d: if (d.readOnly or false) || diskAccess == "readonly" then ",readonly=on" else "";

      staticDiskArgs = lib.concatStringsSep " " (
        lib.imap0
          (
            idx: d:
            if (d.path or null) == null then
              ""
            else
              let
                format = d.format or "qcow2";
                iface = d.iface or "virtio";
              in
              "-drive file=\"$WORK_DIR/disk-${toString idx}.${format}\",format=${format},if=${iface}${perDiskRoFlag d}"
          )
          artefact.disks
      );

      runtimeDiskShell =
        if !hasRuntimeDisks then
          ''RUNTIME_DRIVE_ARGS=""''
        else
          let
            ro = if diskAccess == "readonly" then ",readonly=on" else "";
          in
          ''
            # Splice runtime disk paths into the QEMU command line.
            # diskAccess=${diskAccess}: when "snapshot" the qemu
            # `-snapshot` flag below routes guest writes to a RAM-backed
            # overlay, so the original disks are not mutated.
            RUNTIME_DRIVE_ARGS=""
            if [ -n "''${RUNTIME_DISKS:-}" ]; then
                IFS=',' read -r -a _disks <<<"$RUNTIME_DISKS"
                for _d in "''${_disks[@]}"; do
                    if [ ! -f "$_d" ]; then
                        echo "[harness] runtime disk not found: $_d" >&2
                        tmux kill-session -t "$SESSION_NAME" 2>/dev/null || true
                        exit 1
                    fi
                    RUNTIME_DRIVE_ARGS+=" -drive file=$_d,format=qcow2,if=virtio${ro}"
                done
            else
                echo "[harness] startMode=${artefact.startMode} requires --disks PATH[,PATH,...]" >&2
                exit 1
            fi
          '';

      snapshotFlag = if diskAccess == "snapshot" then "-snapshot" else "";

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

      diskFlagDoc =
        if hasRuntimeDisks then
          ''
            --disks P1[,P2...] Comma-separated qcow2 paths to attach as -drive
                                (required for startMode=${artefact.startMode}).
          ''
        else
          "";

    in
    pkgs.writeShellApplication {
      name = "tmux-${artefact.name}";
      runtimeInputs = with pkgs; [
        tmux
        qemu_kvm
        coreutils
        gawk
        procps
        util-linux
        socat
      ];
      text = ''
        # ----------------------------------------------------------
        # tmux interaction renderer: ${artefact.name}
        #   startMode=${artefact.startMode}  bootMode=${artefact.bootMode}
        #   diskAccess=${diskAccess}
        # ----------------------------------------------------------
        set -euo pipefail

        WORK_DIR="''${TMUX_SERIAL_WORKDIR:-$PWD/.tmux-${artefact.name}}"
        SESSION_NAME="''${TMUX_SERIAL_SESSION:-${sessionName}}"
        RUNTIME_DISKS="''${RUNTIME_DISKS:-}"

        KEEP_PREVIOUS=false
        while [ $# -gt 0 ]; do
            case "$1" in
                --workdir)   WORK_DIR="$2"; shift 2 ;;
                --workdir=*) WORK_DIR="''${1#--workdir=}"; shift ;;
                --session)   SESSION_NAME="$2"; shift 2 ;;
                --session=*) SESSION_NAME="''${1#--session=}"; shift ;;
                --keep)      KEEP_PREVIOUS=true; shift ;;
                --disks)     RUNTIME_DISKS="$2"; shift 2 ;;
                --disks=*)   RUNTIME_DISKS="''${1#--disks=}"; shift ;;
                --help|-h)
                    cat <<EOF
        tmux-${artefact.name} — launch a tmux-attached VM
        Usage: tmux-${artefact.name} [--workdir PATH] [--session NAME] [--keep] [--disks PATH[,...]]
          --workdir PATH   Working directory (default: \$PWD/.tmux-${artefact.name})
          --session NAME   tmux session name (default: ${sessionName})
          --keep           Refuse to start if a QEMU from this workdir is still alive
          ${diskFlagDoc}
        EOF
                    exit 0
                    ;;
                *)
                    echo "tmux-${artefact.name}: unknown argument: $1" >&2
                    exit 2
                    ;;
            esac
        done

        echo "[harness] artefact:  ${artefact.name}"
        echo "[harness] startMode: ${artefact.startMode}"
        echo "[harness] bootMode:  ${artefact.bootMode}"
        echo "[harness] diskAccess:${diskAccess}"
        echo "[harness] workdir:   $WORK_DIR"
        echo "[harness] session:   $SESSION_NAME"
        mkdir -p "$WORK_DIR"

        ${kernelStage}
        ${ovmfStage}
        ${staticDiskCopyCommands}
        ${runtimeDiskShell}

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

        echo "[harness] creating tmux session..."
        tmux new-session -d -s "$SESSION_NAME" -x 200 -y 50 'sleep infinity'

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

        echo "[harness] starting QEMU bound to $PTS_PATH..."
        # shellcheck disable=SC2086
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
            ${snapshotFlag} \
            ${kernelInitrdArgs} \
            ${staticDiskArgs} \
            $RUNTIME_DRIVE_ARGS \
            ${ovmfArgs} \
            >"$QEMU_LOG" 2>&1 || {
                echo "[harness] QEMU launch failed; log:" >&2
                cat "$QEMU_LOG" >&2
                tmux kill-session -t "$SESSION_NAME" || true
                exit 1
            }

        if [ ! -f "$QEMU_PIDFILE" ]; then
            echo "[harness] QEMU failed to start; log:" >&2
            cat "$QEMU_LOG" >&2
            tmux kill-session -t "$SESSION_NAME" || true
            exit 1
        fi
        echo "[harness] QEMU pid=$(cat "$QEMU_PIDFILE")"

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
  inherit mkRunner resizeWatcherScript;
  # Back-compat: legacy callers used mkTmuxSerialRunner.
  mkTmuxSerialRunner = args: mkRunner args;
}
