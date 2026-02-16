# Test runner script builders for NMBL testing
# Creates scripts that build artifacts and launch vm-serial-man

{
  nixpkgs,
  system ? "x86_64-linux",
}:

let
  pkgs = nixpkgs.legacyPackages.${system};

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
      # Check if a previous instance is running
      VM_WAS_RUNNING=false
      if ${vmSerialMan}/bin/vm-serial-man status | grep -q "Running"; then
        echo "Previous instance still running. Automatically issuing stop command"
        ${vmSerialMan}/bin/vm-serial-man stop
        VM_WAS_RUNNING=true
      fi

      # Start vm-serial-man in detached screen session
      ${pkgs.screen}/bin/screen -dmS "${sessionName}" ${screenCommand}

      # Wait for VM manager to be ready (socket to appear)
      echo "Waiting for VM manager to be ready..."
      for i in {1..10}; do
        if ${vmSerialMan}/bin/vm-serial-man status | grep -q "Running"; then
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
      echo "Warning: VM manager may still be starting..."
      echo "Run 'vm-serial-man status' to check"
    '';

  # Build a unified test runner that supports BIOS, UEFI, and direct kernel boot
  mkRunner =
    {
      name,
      config,
      vmSerialMan,
      bootMode, # "mbr", "gpt-bios", "gpt-uefi", or "direct-kernel"
    }:
    let
      vmDiskImage = config.config.system.build.vmDiskImage;
      testArtifacts = config.config.system.build.testArtifacts;
      diskName = "${name}.qcow2";

      # Only needed for direct kernel boot
      kernel = if bootMode == "direct-kernel" then config.config.system.build.nmblKernel else null;
      initrd = if bootMode == "direct-kernel" then config.config.system.build.nmblInitramfs else null;

      # Determine firmware type based on bootMode
      isBios = bootMode == "mbr" || bootMode == "gpt-bios";
      isUefi = bootMode == "gpt-uefi";
      isDirectKernel = bootMode == "direct-kernel";

      bootModeLabel =
        if isDirectKernel then
          "Direct Kernel"
        else if isBios then
          "BIOS"
        else if isUefi then
          "UEFI"
        else
          "Unknown";
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
        echo "Copying VM disk image from Nix store..."
        echo "Source: ${vmDiskImage}"
        cp "${vmDiskImage}/nixos.qcow2" "${diskName}"
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
            screenCommand = "${vmSerialMan}/bin/vm-serial-man manager --name \"${name}\" --disk \"${diskName}\" --memory 2048 --cores 4 direct-kernel --kernel \"$WORK_DIR/kernel\" --initrd \"$WORK_DIR/initrd\" --kernel-args \"$KERNEL_ARGS\"";
            sessionName = name;
          }
        else if isUefi then
          startVMAndWait {
            inherit name vmSerialMan;
            screenCommand = "${vmSerialMan}/bin/vm-serial-man manager --name \"${name}\" --disk \"${diskName}\" --memory 2048 --cores 4 uefi --ovmf-code \"$OVMF_CODE\" --ovmf-vars \"$OVMF_VARS\"";
            sessionName = name;
          }
        else if isBios then
          startVMAndWait {
            inherit name vmSerialMan;
            screenCommand = "${vmSerialMan}/bin/vm-serial-man manager --name \"${name}\" --disk \"${diskName}\" --memory 2048 --cores 4 bios";
            sessionName = name;
          }
        else
          throw "Unknown boot mode: ${bootMode}"
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
      bootMode = "gpt-uefi";
    };

in
{
  inherit mkRunner mkDirectKernelRunner mkUefiRunner;
}
