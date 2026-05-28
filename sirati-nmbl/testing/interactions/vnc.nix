# Interaction renderer: QEMU exposes a VNC server on a chosen
# display, with a noVNC bridge listening on a chosen HTTP port.
# The operator connects via any vncviewer or via the browser.
#
# Useful for splash / framebuffer tests where the serial-only tmux
# renderer cannot show what NMBL is drawing on /dev/dri/card0.
{
  nixpkgs,
  system ? "x86_64-linux",
}:

let
  pkgs = nixpkgs.legacyPackages.${system};
  lib = nixpkgs.lib;

  mkRunner =
    {
      artefact,
      vncDisplay ? 1,
      novncPort ? 6080,
    }:
    let
      diskAccess = artefact.diskAccess or "copy";
      hasRuntimeDisks = artefact.runtimeDisks or false;
      bootMode = artefact.bootMode;

      perDiskRoFlag = d: if (d.readOnly or false) || diskAccess == "readonly" then ",readonly=on" else "";

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
            RUNTIME_DRIVE_ARGS=""
            if [ -n "''${RUNTIME_DISKS:-}" ]; then
                IFS=',' read -r -a _disks <<<"$RUNTIME_DISKS"
                for _d in "''${_disks[@]}"; do
                    if [ ! -f "$_d" ]; then
                        echo "[harness] runtime disk not found: $_d" >&2
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
        if bootMode == "direct-kernel" then
          ''-kernel "$WORK_DIR/kernel" -initrd "$WORK_DIR/initrd" -append "${artefact.kernelArgs} console=tty1"''
        else
          "";

      ovmfArgs =
        if bootMode == "uefi" then
          ''-drive if=pflash,format=raw,readonly=on,file="${artefact.ovmfCode}" -drive if=pflash,format=raw,file="$WORK_DIR/ovmf-vars.fd"''
        else
          "";

      ovmfStage =
        if bootMode == "uefi" then
          ''
            if [ ! -f "$WORK_DIR/ovmf-vars.fd" ]; then
              cp "${artefact.ovmfVars}" "$WORK_DIR/ovmf-vars.fd"
              chmod 644 "$WORK_DIR/ovmf-vars.fd"
            fi''
        else
          "";

      kernelStage =
        if bootMode == "direct-kernel" then
          ''
            ln -sf "${artefact.kernel}/bzImage" "$WORK_DIR/kernel"
            ln -sf "${artefact.initrd}/initrd" "$WORK_DIR/initrd"''
        else
          "";

      diskFlagDoc =
        if hasRuntimeDisks then
          ''
            --disks P1[,P2...]   Comma-separated qcow2 paths (required for
                                  startMode=${artefact.startMode}).
          ''
        else
          "";
    in
    pkgs.writeShellApplication {
      name = "vnc-${artefact.name}";
      runtimeInputs = with pkgs; [
        qemu_kvm
        coreutils
        socat
        novnc
        curl
      ];
      text = ''
        # ----------------------------------------------------------
        # vnc interaction: ${artefact.name}
        #   startMode=${artefact.startMode}  bootMode=${artefact.bootMode}
        # ----------------------------------------------------------
        set -euo pipefail

        WORK_DIR="''${VNC_WORKDIR:-$PWD/.vnc-${artefact.name}}"
        VNC_DISPLAY="''${VNC_DISPLAY:-${toString vncDisplay}}"
        NOVNC_PORT="''${NOVNC_PORT:-${toString novncPort}}"
        RUNTIME_DISKS="''${RUNTIME_DISKS:-}"

        while [ $# -gt 0 ]; do
            case "$1" in
                --workdir)     WORK_DIR="$2"; shift 2 ;;
                --workdir=*)   WORK_DIR="''${1#--workdir=}"; shift ;;
                --vnc-display) VNC_DISPLAY="$2"; shift 2 ;;
                --novnc-port)  NOVNC_PORT="$2"; shift 2 ;;
                --disks)       RUNTIME_DISKS="$2"; shift 2 ;;
                --disks=*)     RUNTIME_DISKS="''${1#--disks=}"; shift ;;
                --help|-h)
                    cat <<EOF
        vnc-${artefact.name} — launch a VNC-attached VM
        Usage: vnc-${artefact.name} [--workdir PATH] [--vnc-display N] [--novnc-port P] [--disks PATH[,...]]
          --workdir PATH      Working directory (default: \$PWD/.vnc-${artefact.name})
          --vnc-display N     QEMU vnc=:N → tcp port 5900+N (default ${toString vncDisplay})
          --novnc-port P      noVNC HTTP listen port (default ${toString novncPort})
          ${diskFlagDoc}
        EOF
                    exit 0
                    ;;
                *)
                    echo "vnc-${artefact.name}: unknown argument: $1" >&2
                    exit 2
                    ;;
            esac
        done

        VNC_PORT=$(( 5900 + VNC_DISPLAY ))

        echo "[harness] artefact:  ${artefact.name}"
        echo "[harness] startMode: ${artefact.startMode}"
        echo "[harness] bootMode:  ${artefact.bootMode}"
        echo "[harness] workdir:   $WORK_DIR"
        echo "[harness] vnc:       :$VNC_DISPLAY (tcp $VNC_PORT)"
        echo "[harness] novnc:     http://localhost:$NOVNC_PORT/"
        mkdir -p "$WORK_DIR"

        ${kernelStage}
        ${ovmfStage}
        ${staticDiskCopyCommands}
        ${runtimeDiskShell}

        QEMU_PIDFILE="$WORK_DIR/qemu.pid"
        QEMU_LOG="$WORK_DIR/qemu.log"
        SER_SOCK="$WORK_DIR/serial.sock"
        SER_LOG="$WORK_DIR/serial.log"
        NOVNC_PIDFILE="$WORK_DIR/novnc.pid"

        if [ -f "$QEMU_PIDFILE" ]; then
            old_pid="$(cat "$QEMU_PIDFILE" 2>/dev/null || true)"
            if [ -n "''${old_pid:-}" ] && kill -0 "$old_pid" 2>/dev/null; then
                echo "[harness] killing previous QEMU pid=$old_pid"
                kill "$old_pid" 2>/dev/null || true
                sleep 0.5
                kill -9 "$old_pid" 2>/dev/null || true
            fi
        fi
        if [ -f "$NOVNC_PIDFILE" ]; then
            old_pid="$(cat "$NOVNC_PIDFILE" 2>/dev/null || true)"
            if [ -n "''${old_pid:-}" ] && kill -0 "$old_pid" 2>/dev/null; then
                kill "$old_pid" 2>/dev/null || true
            fi
        fi
        rm -f "$QEMU_PIDFILE" "$NOVNC_PIDFILE" "$SER_SOCK" "$SER_LOG" "$QEMU_LOG"

        echo "[harness] starting QEMU..."
        # shellcheck disable=SC2086
        qemu-system-x86_64 \
            -machine q35,accel=kvm:tcg \
            -cpu max \
            -m ${toString artefact.memoryMb} \
            -smp ${toString artefact.cores} \
            -device "virtio-vga,xres=1920,yres=1080" \
            -display "vnc=:$VNC_DISPLAY" \
            -chardev "socket,id=ser0,path=$SER_SOCK,server=on,wait=off" \
            -serial chardev:ser0 \
            -nodefaults \
            -daemonize \
            -pidfile "$QEMU_PIDFILE" \
            ${snapshotFlag} \
            ${kernelInitrdArgs} \
            ${staticDiskArgs} \
            $RUNTIME_DRIVE_ARGS \
            ${ovmfArgs} \
            >"$QEMU_LOG" 2>&1

        if [ ! -f "$QEMU_PIDFILE" ]; then
            echo "[harness] QEMU failed to start; log:" >&2
            cat "$QEMU_LOG" >&2
            exit 1
        fi
        echo "[harness] QEMU pid=$(cat "$QEMU_PIDFILE")"

        ( socat -u "UNIX-CONNECT:$SER_SOCK" "OPEN:$SER_LOG,creat,append" >/dev/null 2>&1 & )

        novnc --listen "$NOVNC_PORT" --vnc "localhost:$VNC_PORT" \
            >"$WORK_DIR/novnc.log" 2>&1 &
        echo $! >"$NOVNC_PIDFILE"

        cat <<EOF

        [harness] READY.
          browser:   http://localhost:$NOVNC_PORT/vnc.html?autoconnect=1&resize=scale
          vnc:       localhost:$VNC_PORT
          serial:    socat - UNIX-CONNECT:$SER_SOCK
                     (mirror at $SER_LOG)
          logs:      $QEMU_LOG
          shutdown:  kill \$(cat $QEMU_PIDFILE)
        EOF
      '';
    };
in
{
  inherit mkRunner;
}
