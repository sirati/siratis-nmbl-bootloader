# Interaction renderer: vm-serial-man-rs in a detached GNU-screen
# session. This is the legacy `mkRunner` path, refactored to consume
# the artefact value type so it works with all four start modes.
#
# `vmSerialMan` is the vm-serial-man package; callers thread it in
# from the cross-flake import (same way the old test-runners.nix
# did).
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
      vmSerialMan,
    }:
    let
      diskAccess = artefact.diskAccess or "copy";
      hasRuntimeDisks = artefact.runtimeDisks or false;
      bootMode = artefact.bootMode;

      staticDiskCount = builtins.length (
        builtins.filter (d: (d.path or null) != null) artefact.disks
      );
      runtimeNeededCount = builtins.length (
        builtins.filter (d: (d.path or null) == null) artefact.disks
      );

      diskStagingShell =
        let
          eachStatic =
            lib.imap0
              (
                idx: d:
                if (d.path or null) == null then
                  ""
                else
                  let
                    format = d.format or "qcow2";
                    target = "$WORK_DIR/disk-${toString idx}.${format}";
                  in
                  ''
                    if [ ! -f "${target}" ]; then
                      cp "${d.path}" "${target}"
                      chmod 644 "${target}"
                    fi
                    STAGED_DISKS+=("${target}")
                  ''
              )
              artefact.disks;
          runtime =
            if hasRuntimeDisks then
              ''
                if [ -z "''${RUNTIME_DISKS:-}" ]; then
                  echo "[harness] startMode=${artefact.startMode} requires --disks PATH[,PATH,...]" >&2
                  exit 1
                fi
                IFS=',' read -r -a _disks <<<"$RUNTIME_DISKS"
                for _d in "''${_disks[@]}"; do
                  if [ ! -f "$_d" ]; then
                    echo "[harness] runtime disk not found: $_d" >&2
                    exit 1
                  fi
                  STAGED_DISKS+=("$_d")
                done
              ''
            else
              "";
        in
        lib.concatStringsSep "\n" (eachStatic ++ [ runtime ]);

      # The subcommand and its arguments selected from artefact.bootMode.
      # vm-serial-man expects: `manager --name N --memory M --cores C --disk D... [direct-kernel|uefi|bios] [...]`
      managerSubArgs =
        if bootMode == "direct-kernel" then
          ''direct-kernel --kernel "${artefact.kernel}/bzImage" --initrd "${artefact.initrd}/initrd" --kernel-args "${artefact.kernelArgs}"''
        else if bootMode == "uefi" then
          ''uefi --ovmf-code "${artefact.ovmfCode}" --ovmf-vars "$WORK_DIR/ovmf-vars.fd"''
        else if bootMode == "bios" then
          ''bios''
        else
          throw "qemu-serial-rs renderer: unsupported bootMode ${bootMode}";

      ovmfStage =
        if bootMode == "uefi" then
          ''
            if [ ! -f "$WORK_DIR/ovmf-vars.fd" ]; then
              cp "${artefact.ovmfVars}" "$WORK_DIR/ovmf-vars.fd"
              chmod 644 "$WORK_DIR/ovmf-vars.fd"
            fi
          ''
        else
          "";

      diskFlagDoc =
        if hasRuntimeDisks then
          ''
            --disks P1[,P2...] Comma-separated qcow2 paths to attach
                                (required for startMode=${artefact.startMode}).
          ''
        else
          "";

    in
    pkgs.writeShellApplication {
      name = "qemu-serial-rs-${artefact.name}";
      runtimeInputs = with pkgs; [
        screen
        coreutils
        vmSerialMan
      ];
      text = ''
        # ----------------------------------------------------------
        # qemu-serial-rs interaction: ${artefact.name}
        #   startMode=${artefact.startMode}  bootMode=${artefact.bootMode}
        # ----------------------------------------------------------
        set -euo pipefail

        WORK_DIR="''${VMSM_WORKDIR:-$PWD/.qemu-serial-rs-${artefact.name}}"
        SESSION_NAME="''${VMSM_SESSION:-${artefact.name}}"
        RUNTIME_DISKS="''${RUNTIME_DISKS:-}"

        while [ $# -gt 0 ]; do
            case "$1" in
                --workdir)   WORK_DIR="$2"; shift 2 ;;
                --workdir=*) WORK_DIR="''${1#--workdir=}"; shift ;;
                --session)   SESSION_NAME="$2"; shift 2 ;;
                --session=*) SESSION_NAME="''${1#--session=}"; shift ;;
                --disks)     RUNTIME_DISKS="$2"; shift 2 ;;
                --disks=*)   RUNTIME_DISKS="''${1#--disks=}"; shift ;;
                --help|-h)
                    cat <<EOF
        qemu-serial-rs-${artefact.name} — launch a vm-serial-man-rs + screen VM
        Usage: qemu-serial-rs-${artefact.name} [--workdir PATH] [--session NAME] [--disks PATH[,...]]
          --workdir PATH   Working directory (default: \$PWD/.qemu-serial-rs-${artefact.name})
          --session NAME   GNU screen session name (default: ${artefact.name})
          ${diskFlagDoc}
        EOF
                    exit 0
                    ;;
                *)
                    echo "qemu-serial-rs-${artefact.name}: unknown argument: $1" >&2
                    exit 2
                    ;;
            esac
        done

        echo "[harness] artefact:  ${artefact.name}"
        echo "[harness] startMode: ${artefact.startMode}"
        echo "[harness] bootMode:  ${artefact.bootMode}"
        echo "[harness] workdir:   $WORK_DIR"
        echo "[harness] session:   $SESSION_NAME"
        mkdir -p "$WORK_DIR"

        ${ovmfStage}

        STAGED_DISKS=()
        ${diskStagingShell}

        # Stop any previous instance.
        if ${vmSerialMan}/bin/vm-serial-man status 2>/dev/null | grep -q "Running"; then
          echo "[harness] previous vm-serial-man still alive; stopping..."
          ${vmSerialMan}/bin/vm-serial-man stop || true
        fi

        DISK_ARGS=()
        for d in "''${STAGED_DISKS[@]}"; do
          DISK_ARGS+=(--disk "$d")
        done

        # shellcheck disable=SC2086
        screen -dmS "$SESSION_NAME" ${vmSerialMan}/bin/vm-serial-man manager \
          --name "${artefact.name}" \
          --memory ${toString artefact.memoryMb} \
          --cores ${toString artefact.cores} \
          "''${DISK_ARGS[@]}" \
          ${managerSubArgs}

        echo "[harness] vm-serial-man started in screen session $SESSION_NAME"
        echo "[harness] attach with: screen -r $SESSION_NAME"
        echo "[harness] send commands: ${vmSerialMan}/bin/vm-serial-man send <text>"
      '';
    };
in
{
  inherit mkRunner;
}
