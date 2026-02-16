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

  # Build a direct kernel boot test runner
  mkDirectKernelRunner =
    {
      name,
      config,
      vmSerialMan,
    }:
    let
      kernel = config.config.system.build.nmblKernel;
      initrd = config.config.system.build.nmblInitramfs;
      vmDiskImage = config.config.system.build.vmDiskImage;
      testArtifacts = config.config.system.build.testArtifacts;
      diskName = "${name}.qcow2";
    in
    pkgs.writeShellScript "run-${name}-direct" ''
      set -e

      echo "=== NMBL Direct Kernel Boot Test: ${name} ==="
      echo

      WORK_DIR="$PWD/.nmbl-test-${name}"
      mkdir -p "$WORK_DIR"

      # Link kernel and initrd
      echo "[1/4] Preparing kernel and initrd..."
      ln -sf ${kernel}/bzImage "$WORK_DIR/kernel"
      ln -sf ${initrd}/initrd "$WORK_DIR/initrd"
      echo "✓ Kernel: $(du -h "$WORK_DIR/kernel" | cut -f1)"
      echo "✓ Initrd: $(du -h "$WORK_DIR/initrd" | cut -f1)"

      # Use pre-built VM disk image from NixOS configuration
      echo "[2/4] Preparing disk image..."
      if [ ! -f "${diskName}" ]; then
        echo "Copying VM disk image from Nix store..."
        echo "Source: ${vmDiskImage}"
        cp "${vmDiskImage}/nixos.qcow2" "${diskName}"
        chmod 644 "${diskName}"
        echo "✓ Disk image copied successfully"
      else
        echo "✓ Using existing disk: ${diskName}"
      fi

      # Create convenience link to vm-serial-man
      echo "[3/4] Linking vm-serial-man..."
      ln -sf ${vmSerialMan}/bin/vm-serial-man "$WORK_DIR/vm-serial-man"
      echo "✓ vm-serial-man available at: $WORK_DIR/vm-serial-man"

      # All test artifacts available at: ${testArtifacts}
      echo
      echo "Test artifacts:"
      echo "  Kernel:        $WORK_DIR/kernel"
      echo "  Initrd:        $WORK_DIR/initrd"
      echo "  Disk:          ${diskName}"
      echo "  VM Manager:    $WORK_DIR/vm-serial-man"
      echo "  All artifacts: ${testArtifacts}"
      echo

      # Launch VM in screen
      echo "[4/4] Starting VM with vm-serial-man in screen session..."
      echo
      echo "VM will boot with direct kernel boot (no bootloader)"
      echo "Manager running in screen session '${name}-direct'"
      echo "Use 'vm-serial-man send <command>' to interact"
      echo "Use 'screen -r ${name}-direct' to attach to VM console"
      echo

      ${startVMAndWait {
        inherit name vmSerialMan;
        screenCommand = "${vmSerialMan}/bin/vm-serial-man manager-direct-kernel --name \"${name}-direct\" --disk \"${diskName}\" --kernel \"$WORK_DIR/kernel\" --initrd \"$WORK_DIR/initrd\" --kernel-args \"console=ttyS0,115200 earlyprintk=serial,ttyS0,115200\" --memory 2048 --cores 4";
        sessionName = "${name}-direct";
      }}
    '';

  # Build a UEFI boot test runner
  mkUefiRunner =
    {
      name,
      config,
      diskName ? "${name}.qcow2",
      vmSerialMan,
    }:
    let
      testArtifacts = config.config.system.build.testArtifacts;
    in
    pkgs.writeShellScript "run-${name}-uefi" ''
      set -e

      echo "=== NMBL UEFI Boot Test: ${name} ==="
      echo

      if [ ! -f "${diskName}" ]; then
        echo "Error: Disk image ${diskName} not found"
        echo "Run the direct kernel boot test first to create it"
        exit 1
      fi

      WORK_DIR="$PWD/.nmbl-test-${name}"
      mkdir -p "$WORK_DIR"

      # Create convenience link to vm-serial-man
      ln -sf ${vmSerialMan}/bin/vm-serial-man "$WORK_DIR/vm-serial-man"

      # Find OVMF
      OVMF_CODE="${pkgs.OVMF.fd}/FV/OVMF_CODE.fd"
      OVMF_VARS="${name}_OVMF_VARS.fd"

      if [ ! -f "$OVMF_VARS" ]; then
        echo "Creating OVMF_VARS..."
        cp "${pkgs.OVMF.fd}/FV/OVMF_VARS.fd" "$OVMF_VARS"
        chmod 644 "$OVMF_VARS"
      fi

      echo
      echo "Test artifacts:"
      echo "  Disk:          ${diskName}"
      echo "  OVMF Code:     $OVMF_CODE"
      echo "  OVMF Vars:     $OVMF_VARS"
      echo "  VM Manager:    $WORK_DIR/vm-serial-man"
      echo "  All artifacts: ${testArtifacts}"
      echo

      echo "Starting VM with UEFI boot in screen session..."
      echo "Manager running in screen session '${name}-uefi'"
      echo "Use 'vm-serial-man send <command>' to interact"
      echo "Use 'screen -r ${name}-uefi' to attach to VM console"
      echo

      ${startVMAndWait {
        inherit name vmSerialMan;
        screenCommand = "${vmSerialMan}/bin/vm-serial-man manager-uefi --name \"${name}-uefi\" --disk \"${diskName}\" --ovmf-code \"$OVMF_CODE\" --ovmf-vars \"$OVMF_VARS\" --memory 2048 --cores 4";
        sessionName = "${name}-uefi";
      }}
    '';

in
{
  inherit mkDirectKernelRunner mkUefiRunner;
}

