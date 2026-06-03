# Test runner script builders for NMBL testing
# Creates scripts that build artifacts and launch vm-serial-man

{
  nixpkgs,
  system ? "x86_64-linux",
}:

let
  pkgs = nixpkgs.legacyPackages.${system};
  lib = nixpkgs.lib;

  # Common help display function
  printHelp = vmSerialMan: ''
    echo "========================================"
    ${vmSerialMan}/bin/vm-serial-man --help
    echo
    echo "========================================"
    ${vmSerialMan}/bin/vm-serial-man send --help
    echo
    echo "========================================"
    ${vmSerialMan}/bin/vm-serial-man find --help
    echo
    echo "========================================"
    ${vmSerialMan}/bin/vm-serial-man trigger --help
    echo
    echo "========================================"
    ${vmSerialMan}/bin/vm-serial-man lines --help
  '';

  # Common VM startup logic
  startVMAndWait =
    {
      name,
      vmSerialMan,
      screenCommand,
      sessionName,
      showHelp ? true,
    }:
    ''
      # MULTI-VM-SAFE manager lifecycle (no GLOBAL `vm-serial-man status|stop`).
      #
      # `vm-serial-man status`/`stop` WITHOUT `--socket` auto-detect across EVERY
      # manager on the host, so when a peer runs another VM concurrently the old
      # global stop could tear down the PEER's manager (or our own just-started
      # one) and the global status check could latch onto a foreign manager. Both
      # are scoped to THIS runner's own `--name ${sessionName}` manager here: we
      # find its control socket the way the assertion helpers' `pin_vm_socket`
      # does — the real `vm-serial-man manager` process whose argv carries our
      # exact `--name`, then `/tmp/vm-serial-man-<pid>.sock` — and act ONLY on it.

      # Print the control socket of OUR manager (the `vm-serial-man manager`
      # process — NOT the screen wrapper — whose argv has `--name <sessionName>`
      # as an exact token), or nothing if it is not up yet.
      _own_sock() {
        local cmd_file argv0 base i pid sock
        for cmd_file in /proc/[0-9]*/cmdline; do
          [ -r "$cmd_file" ] || continue
          local -a argv=()
          mapfile -d "" -t argv <"$cmd_file" 2>/dev/null || continue
          [ "''${#argv[@]}" -gt 0 ] || continue
          argv0="''${argv[0]}"; base="''${argv0##*/}"
          [ "$base" = "vm-serial-man" ] || continue
          local is_manager=false matched=false
          for ((i = 1; i < ''${#argv[@]}; i++)); do
            case "''${argv[i]}" in
              manager) is_manager=true ;;
              --name|-n) [ "''${argv[i + 1]:-}" = "${sessionName}" ] && matched=true ;;
            esac
          done
          if [ "$is_manager" = true ] && [ "$matched" = true ]; then
            pid="''${cmd_file#/proc/}"; pid="''${pid%/cmdline}"
            sock="/tmp/vm-serial-man-''${pid}.sock"
            [ -S "$sock" ] && { printf '%s\n' "$sock"; return 0; }
          fi
        done
        return 1
      }

      # Stop ONLY a previous instance of OUR OWN session (scoped by --socket), so
      # a concurrent peer manager is never touched.
      VM_WAS_RUNNING=false
      OWN_SOCK="$(_own_sock || true)"
      if [ -n "$OWN_SOCK" ]; then
        echo "Previous instance of '${sessionName}' running. Stopping just that one."
        ${vmSerialMan}/bin/vm-serial-man stop --socket "$OWN_SOCK" >/dev/null 2>&1 || true
        VM_WAS_RUNNING=true
        # Wait for its socket to disappear before re-launching under the same name.
        for _ in $(seq 1 25); do _own_sock >/dev/null 2>&1 || break; sleep 0.2; done
      fi

      # Start vm-serial-man in detached screen session
      ${pkgs.screen}/bin/screen -dmS "${sessionName}" ${screenCommand}

      # Wait for OUR manager to be ready (its own socket to appear), not a global
      # status that a foreign manager could satisfy.
      echo "Waiting for VM manager '${sessionName}' to be ready..."
      for i in {1..50}; do
        if _own_sock >/dev/null 2>&1; then
          echo "✓ VM manager ready!"
          echo

          # Only show help if this is a fresh start (no previous VM was running)
          if [ "$VM_WAS_RUNNING" = "false" ]; then
            ${printHelp vmSerialMan}
          fi
          exit 0
        fi
        sleep 0.2
      done
      echo "Warning: VM manager '${sessionName}' may still be starting..."
      echo "Run 'vm-serial-man status' to check"
    '';

  # Build a unified test runner that supports BIOS, UEFI, and direct kernel boot
  mkRunner =
    {
      name,
      config,
      vmSerialMan,
      bootMode ? null, # Optional: "gpt-bios", "gpt-uefi", or "direct-kernel" (derived from config if null)
      # ADDITIVE measured-/secure-boot toggles (R-10), default OFF so an
      # unset runner is byte-identical to before:
      #   tpm        : null | "tis" | "crb" — attach an swtpm-backed TPM 2.0.
      #                The per-run state dir is created under $WORK_DIR.
      #   secureBoot : false — when true, boot under a Secure-Boot-enforcing
      #                OVMFFull firmware (`smm=on` + db-enrolled VARS). A fresh
      #                writable VARS copy is made per run.
      #   dbCert     : null — only meaningful with secureBoot = true. When a
      #                cert PATH is given, the enforcing VARS template is the
      #                Microsoft `OVMF_VARS.ms.fd` with THIS cert ADDITIONALLY
      #                enrolled in `db` (via virt-fw-vars). The firmware still
      #                ENFORCES (MS db intact) but now ALSO TRUSTS a UKI signed
      #                by this cert — so an NMBL UKI sbsign'd with the matching
      #                key boots, while an unsigned UKI is still refused (F1).
      #                null ⇒ MS-only db (refuses anything not MS-signed), used
      #                by the unsigned-UKI smoke test.
      tpm ? null,
      secureBoot ? false,
      dbCert ? null,
      # When true (only meaningful with `tpm != null`), the swtpm STATE dir is
      # PERSISTED across this manager's lifetime: it is not wiped on start nor
      # removed on stop. Two successive runs of the runner against the same
      # $WORK_DIR therefore share one TPM, so a secret a first (enroll) boot
      # seals survives into a second (unseal) boot — the measured-boot
      # seal/unseal ROUNDTRIP. A fresh QEMU power-on still issues
      # TPM2_Startup(CLEAR), so PCRs reset and NMBL re-extends the same
      # deterministic event sequence, reproducing the sealed PCR value. The
      # caller (the roundtrip assertion) owns deleting $WORK_DIR/swtpm-state
      # when the roundtrip ends.
      tpmPersist ? false,
    }:
    let
      # Derive bootMode from config if not explicitly provided (must be first)
      actualBootMode =
        if bootMode != null then
          bootMode
        else if config.config.boot.nmbl ? bootstrapper then
          let
            bs = config.config.boot.nmbl.bootstrapper;
          in
          if bs.bootMode == "bios" then
            "gpt-bios"
          else if bs.bootMode == "uefi" then
            "gpt-uefi"
          else if bs.bootMode == "qemu_kernel_invoke" then
            "direct-kernel"
          else
            throw "Unknown bootstrapper bootMode: ${bs.bootMode}"
        else
          "direct-kernel"; # Fallback for configs without bootstrapper

      vmDiskImage = config.config.system.build.vmDiskImage;
      testArtifacts = config.config.system.build.testArtifacts;
      diskName = "${name}.qcow2";

      # Only needed for direct kernel boot
      kernel = if actualBootMode == "direct-kernel" then config.config.system.build.nmblKernel else null;
      initrd =
        if actualBootMode == "direct-kernel" then config.config.system.build.nmblInitramfs else null;

      # Determine firmware type based on bootMode
      isBios = actualBootMode == "gpt-bios";
      isUefi = actualBootMode == "gpt-uefi";
      isDirectKernel = actualBootMode == "direct-kernel";

      bootModeLabel =
        if isDirectKernel then
          "Direct Kernel"
        else if isBios then
          "BIOS"
        else if isUefi then
          "UEFI"
        else
          "Unknown";

      # ---- ADDITIVE TPM / Secure-Boot wiring (R-10) -----------------------
      # The swtpm-backed TPM: a per-run state directory under $WORK_DIR and the
      # `--tpm`/`--tpm-kind` flags. Empty string when no TPM is requested, so
      # the launch command is unchanged.
      # The swtpm state dir. Defaults to per-run under $WORK_DIR, but an
      # NMBL_SWTPM_STATE env override lets a multi-phase roundtrip point two
      # successive runner invocations (enroll, then unseal) at ONE shared,
      # persisted state dir so the sealed object survives the power-cycle.
      tpmStateExpr = "\${NMBL_SWTPM_STATE:-$WORK_DIR/swtpm-state}";
      tpmArgs =
        if tpm == null then
          ""
        else
          " --tpm \"${tpmStateExpr}\" --tpm-kind ${tpm}"
          + lib.optionalString tpmPersist " --tpm-persist";

      # Secure-Boot OVMFFull: the SB-built code firmware (read-only) and a
      # PER-RUN writable copy of the db-enrolled VARS (`OVMF_VARS.ms.fd`, which
      # ships Microsoft's KEK/db so the firmware ENFORCES — it refuses an
      # unsigned EFI binary). The `--sb-code`/`--sb-vars` flags flip the
      # qemu seam to `-machine …,smm=on` + secure pflash.
      sbCode = "${pkgs.OVMFFull.fd}/FV/OVMF_CODE.fd";
      msVarsTemplate = "${pkgs.OVMFFull.fd}/FV/OVMF_VARS.ms.fd";

      # When a test `db` cert is supplied, derive an ENFORCING VARS that ALSO
      # trusts a UKI signed by it: start from the Microsoft VARS (KEK/db/PK +
      # SecureBoot already enrolled, so it still ENFORCES) and ADD our cert to
      # `db` with virt-fw-vars. The result refuses an unsigned UKI exactly like
      # the MS-only VARS, but ACCEPTS the NMBL UKI sbsign'd with the matching
      # key — which is what lets NMBL actually run under enforcing SB (F1).
      # A fixed owner GUID labels the enrolled entry (cosmetic only).
      testDbVars =
        pkgs.runCommand "ovmf-vars-ms-plus-test-db.fd"
          {
            nativeBuildInputs = [ pkgs.python3Packages.virt-firmware ];
          }
          ''
            virt-fw-vars \
              --input ${msVarsTemplate} \
              --add-db 605dab50-e046-4300-abb6-3dd810dd8b23 ${dbCert} \
              --output "$out"
          '';

      sbVarsTemplate = if dbCert != null then "${testDbVars}" else msVarsTemplate;
      sbArgs =
        if secureBoot then
          " --sb-code \"${sbCode}\" --sb-vars \"$WORK_DIR/sb-OVMF_VARS.fd\""
        else
          "";
    in
    pkgs.writeShellScript "run-${name}" ''
      set -e

      ${
        if isDirectKernel then
          ''
            # Parse arguments (only for direct kernel boot)
            DEBUG_SHELL=false
            while [[ $# -gt 0 ]]; do
              case $1 in
                --debug-shell)
                  DEBUG_SHELL=true
                  shift
                  ;;
                *)
                  echo "Unknown option: $1"
                  echo "Usage: $0 [--debug-shell]"
                  echo "  --debug-shell: Boot into emergency shell before kexec"
                  exit 1
                  ;;
              esac
            done

            # Build kernel arguments
            KERNEL_ARGS="console=ttyS0,115200 earlyprintk=serial,ttyS0,115200"
            if [ "$DEBUG_SHELL" = "true" ]; then
              KERNEL_ARGS="$KERNEL_ARGS nmbl.shell"
              echo "DEBUG MODE: Will drop to emergency shell before kexec"
              echo
            fi
          ''
        else
          ""
      }

      echo "=== NMBL ${bootModeLabel} Boot Test: ${name} ==="
      echo

      WORK_DIR="$PWD/.nmbl-test-${name}"
      mkdir -p "$WORK_DIR"

      ${
        if isDirectKernel then
          ''
            # Link kernel and initrd
            echo "[1/4] Preparing kernel and initrd..."
            ln -sf ${kernel}/bzImage "$WORK_DIR/kernel"
            ln -sf ${initrd}/initrd "$WORK_DIR/initrd"
            echo "✓ Kernel: $(du -h "$WORK_DIR/kernel" | cut -f1)"
            echo "✓ Initrd: $(du -h "$WORK_DIR/initrd" | cut -f1)"

            # Use pre-built VM disk image from NixOS configuration
            echo "[2/4] Preparing disk image..."
          ''
        else
          ''
            # Use pre-built VM disk image from NixOS configuration
            echo "[1/3] Preparing disk image..."
          ''
      }
      if [ ! -f "${diskName}" ]; then
        # NMBL_DISK_IMAGE overrides the disk source with a RUNTIME path (a qcow2
        # produced at install time, e.g. by the secure-boot nixos-anywhere
        # installer). This is how the secure-boot scenarios boot a disk SIGNED
        # at install runtime instead of a build-time `vmDiskImage` store path —
        # the signing key is never an input to a derivation. Falls back to the
        # config's `vmDiskImage` (the normal non-secure-boot path) when unset.
        if [ -n "''${NMBL_DISK_IMAGE:-}" ]; then
          echo "Copying VM disk image from runtime path..."
          echo "Source: ''${NMBL_DISK_IMAGE}"
          cp "''${NMBL_DISK_IMAGE}" "${diskName}"
        else
          echo "Copying VM disk image from Nix store..."
          echo "Source: ${vmDiskImage}"
          cp "${vmDiskImage}/nixos.qcow2" "${diskName}"
        fi
        chmod 644 "${diskName}"
        echo "✓ Disk image copied successfully"
      else
        echo "✓ Using existing disk: ${diskName}"
      fi
      echo

      # Create convenience link to vm-serial-man
      ${
        if isDirectKernel then
          ''echo "[3/4] Linking vm-serial-man..."''
        else
          ''echo "[2/3] Linking vm-serial-man..."''
      }
      ln -sf ${vmSerialMan}/bin/vm-serial-man "$WORK_DIR/vm-serial-man"
      echo "✓ vm-serial-man available at: $WORK_DIR/vm-serial-man"

      ${
        if isUefi then
          ''
            # Find OVMF
            OVMF_CODE="${pkgs.OVMF.fd}/FV/OVMF_CODE.fd"
            OVMF_VARS="${name}_OVMF_VARS.fd"

            if [ ! -f "$OVMF_VARS" ]; then
              echo "Creating OVMF_VARS..."
              cp "${pkgs.OVMF.fd}/FV/OVMF_VARS.fd" "$OVMF_VARS"
              chmod 644 "$OVMF_VARS"
            fi
          ''
        else
          ""
      }

      ${lib.optionalString secureBoot ''
        # Secure-Boot OVMFFull: a FRESH writable copy of the db-enrolled VARS
        # per run (so each run starts from the same enforcing variable store and
        # the firmware refuses unsigned binaries). The qemu seam adds smm=on.
        echo "Preparing Secure-Boot OVMF VARS (db-enrolled, enforcing)..."
        cp "${sbVarsTemplate}" "$WORK_DIR/sb-OVMF_VARS.fd"
        chmod 644 "$WORK_DIR/sb-OVMF_VARS.fd"
        echo "✓ SB code: ${sbCode}"
        echo "✓ SB vars: $WORK_DIR/sb-OVMF_VARS.fd"
      ''}

      echo
      echo "Test artifacts:"
      ${
        if isDirectKernel then
          ''
            echo "  Kernel:        $WORK_DIR/kernel"
            echo "  Initrd:        $WORK_DIR/initrd"
          ''
        else
          ""
      }
      echo "  Disk:          ${diskName}"
      ${
        if isUefi then
          ''
            echo "  OVMF Code:     $OVMF_CODE"
            echo "  OVMF Vars:     $OVMF_VARS"
          ''
        else
          ""
      }
      echo "  VM Manager:    $WORK_DIR/vm-serial-man"
      echo "  All artifacts: ${testArtifacts}"
      echo

      # Launch VM in screen
      ${
        if isDirectKernel then
          ''echo "[4/4] Starting VM with vm-serial-man in screen session..."''
        else
          ''echo "[3/3] Starting VM with vm-serial-man in screen session..."''
      }
      echo
      echo "VM will boot with ${bootModeLabel} boot mode"
      echo "Manager running in screen session '${name}'"
      echo "Use 'vm-serial-man send <command>' to interact"
      echo "Use 'screen -r ${name}' to attach to VM console"
      echo

      ${
        if isDirectKernel then
          startVMAndWait {
            inherit name vmSerialMan;
            screenCommand = "${vmSerialMan}/bin/vm-serial-man manager --name \"${name}\" --disk \"${diskName}\" --memory 2048 --cores 4${tpmArgs}${sbArgs} direct-kernel --kernel \"$WORK_DIR/kernel\" --initrd \"$WORK_DIR/initrd\" --kernel-args \"$KERNEL_ARGS\"";
            sessionName = name;
          }
        else if isUefi then
          startVMAndWait {
            inherit name vmSerialMan;
            screenCommand = "${vmSerialMan}/bin/vm-serial-man manager --name \"${name}\" --disk \"${diskName}\" --memory 2048 --cores 4${tpmArgs}${sbArgs} uefi --ovmf-code \"$OVMF_CODE\" --ovmf-vars \"$OVMF_VARS\"";
            sessionName = name;
          }
        else if isBios then
          startVMAndWait {
            inherit name vmSerialMan;
            screenCommand = "${vmSerialMan}/bin/vm-serial-man manager --name \"${name}\" --disk \"${diskName}\" --memory 2048 --cores 4${tpmArgs}${sbArgs} bios";
            sessionName = name;
          }
        else
          throw "Unknown boot mode: ${actualBootMode}"
      }
    '';

  # Backwards compatibility: direct kernel boot runner
  mkDirectKernelRunner =
    {
      name,
      config,
      vmSerialMan,
    }:
    mkRunner {
      inherit name config vmSerialMan;
      bootMode = "direct-kernel";
    };

  # Backwards compatibility: UEFI boot runner
  mkUefiRunner =
    {
      name,
      config,
      vmSerialMan,
      diskName ? "${name}.qcow2",
    }:
    mkRunner {
      inherit name config vmSerialMan;
      bootMode = null; # Will be derived from config
    };

in
{
  inherit mkRunner mkDirectKernelRunner mkUefiRunner;
}
