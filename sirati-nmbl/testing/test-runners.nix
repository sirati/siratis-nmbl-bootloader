# Test runner script builders for NMBL testing
# Creates scripts that build artifacts and launch vm-serial-man

{
  nixpkgs,
  system ? "x86_64-linux",
}:

let
  pkgs = nixpkgs.legacyPackages.${system};

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
      diskName = "${name}.qcow2";
    in
    pkgs.writeShellScript "run-${name}-direct" ''
      set -e

      echo "=== NMBL Direct Kernel Boot Test: ${name} ==="
      echo

      WORK_DIR="$PWD/.nmbl-test-${name}"
      mkdir -p "$WORK_DIR"

      # Link kernel and initrd
      echo "[1/3] Preparing kernel and initrd..."
      ln -sf ${kernel}/bzImage "$WORK_DIR/kernel"
      ln -sf ${initrd}/initrd "$WORK_DIR/initrd"
      echo "✓ Kernel: $(du -h "$WORK_DIR/kernel" | cut -f1)"
      echo "✓ Initrd: $(du -h "$WORK_DIR/initrd" | cut -f1)"

      # Create disk if needed
      echo "[2/3] Preparing disk image..."
      if [ ! -f "${diskName}" ]; then
        echo "Creating ${diskName}..."
        ${pkgs.qemu}/bin/qemu-img create -f qcow2 "${diskName}" 2G
      fi
      echo "✓ Disk: ${diskName}"

      # Launch VM in screen
      echo "[3/3] Starting VM with vm-serial-man in screen session..."
      echo
      echo "VM will boot with direct kernel boot (no bootloader)"
      echo "Manager running in screen session '${name}-direct'"
      echo "Use 'vm-serial-man send <command>' to interact"
      echo "Use 'screen -r ${name}-direct' to attach to VM console"
      echo

      # Start vm-serial-man in detached screen session
      ${pkgs.screen}/bin/screen -dmS "${name}-direct" ${vmSerialMan}/bin/vm-serial-man manager-direct-kernel \
        --name "${name}-direct" \
        --disk "${diskName}" \
        --kernel "$WORK_DIR/kernel" \
        --initrd "$WORK_DIR/initrd" \
        --kernel-args "console=ttyS0,115200 earlyprintk=serial,ttyS0,115200" \
        --memory 2048 \
        --cores 4

      # Wait for VM manager to be ready (socket to appear)
      echo "Waiting for VM manager to be ready..."
      for i in {1..10}; do
        if ${vmSerialMan}/bin/vm-serial-man status | grep -q "Running"; then
          echo "✓ VM manager ready!"
          echo
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
          exit 0
        fi
        sleep 0.2
      done
      echo "Warning: VM manager may still be starting..."
      echo "Run 'vm-serial-man status' to check"
    '';

  # Build a UEFI boot test runner
  mkUefiRunner =
    {
      name,
      diskName ? "${name}.qcow2",
      vmSerialMan,
    }:
    pkgs.writeShellScript "run-${name}-uefi" ''
      set -e

      echo "=== NMBL UEFI Boot Test: ${name} ==="
      echo

      if [ ! -f "${diskName}" ]; then
        echo "Error: Disk image ${diskName} not found"
        echo "Run the direct kernel boot test first to create it"
        exit 1
      fi

      # Find OVMF
      OVMF_CODE="${pkgs.OVMF.fd}/FV/OVMF_CODE.fd"
      OVMF_VARS="${name}_OVMF_VARS.fd"

      if [ ! -f "$OVMF_VARS" ]; then
        echo "Creating OVMF_VARS..."
        cp "${pkgs.OVMF.fd}/FV/OVMF_VARS.fd" "$OVMF_VARS"
        chmod 644 "$OVMF_VARS"
      fi

      echo "Starting VM with UEFI boot in screen session..."
      echo "Manager running in screen session '${name}-uefi'"
      echo "Use 'vm-serial-man send <command>' to interact"
      echo "Use 'screen -r ${name}-uefi' to attach to VM console"
      echo

      # Start vm-serial-man in detached screen session
      ${pkgs.screen}/bin/screen -dmS "${name}-uefi" ${vmSerialMan}/bin/vm-serial-man manager-uefi \
        --name "${name}-uefi" \
        --disk "${diskName}" \
        --ovmf-code "$OVMF_CODE" \
        --ovmf-vars "$OVMF_VARS" \
        --memory 2048 \
        --cores 4

      # Wait for VM manager to be ready (socket to appear)
      echo "Waiting for VM manager to be ready..."
      for i in {1..10}; do
        if ${vmSerialMan}/bin/vm-serial-man status | grep -q "Running"; then
          echo "✓ VM manager ready!"
          echo
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
          exit 0
        fi
        sleep 0.2
      done
      echo "Warning: VM manager may still be starting..."
      echo "Run 'vm-serial-man status' to check"
    '';

in
{
  inherit mkDirectKernelRunner mkUefiRunner;
}
