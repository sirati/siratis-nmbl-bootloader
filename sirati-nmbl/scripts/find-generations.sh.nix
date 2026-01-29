# Find NixOS Generations Script
# Returns a shell script string for discovering available NixOS generations

{
  lib,
  pkgs,
  cfg,
}:

''
  # ============================================
  # Part 2: Find NixOS Generations
  # ============================================

  echo "NMBL: Discovering NixOS generations..."

  # Check if root filesystem is mounted
  if [ ! -d "${cfg.mountPrefix}" ]; then
    echo "ERROR: Mount prefix ${cfg.mountPrefix} not found!"
    exit 1
  fi

  # Find all generations
  GENERATIONS=()
  KERNELS=()
  INITRDS=()
  KERNEL_PARAMS=()

  # Parse NixOS system profiles (sorted by version, newest first)
  for system in $(${pkgs.busybox}/bin/ls -d ${cfg.mountPrefix}/nix/var/nix/profiles/system-*-link 2>/dev/null | ${pkgs.busybox}/bin/sort -V -r); do
    if [ -f "$system/kernel" ] && [ -f "$system/initrd" ]; then
      gen_num=$(${pkgs.busybox}/bin/basename "$system" | ${pkgs.busybox}/bin/sed 's/system-\(.*\)-link/\1/')
      GENERATIONS+=("$gen_num")
      KERNELS+=("$system/kernel")
      INITRDS+=("$system/initrd")

      # Extract kernel parameters
      if [ -f "$system/kernel-params" ]; then
        KERNEL_PARAMS+=("$(${pkgs.busybox}/bin/cat $system/kernel-params)")
      else
        KERNEL_PARAMS+=("")
      fi
    fi
  done

  # Also check current system
  if [ -L "${cfg.mountPrefix}/nix/var/nix/profiles/system" ]; then
    system="${cfg.mountPrefix}/nix/var/nix/profiles/system"
    if [ -f "$system/kernel" ] && [ -f "$system/initrd" ]; then
      GENERATIONS=("current" "''${GENERATIONS[@]}")
      KERNELS=("$system/kernel" "''${KERNELS[@]}")
      INITRDS=("$system/initrd" "''${INITRDS[@]}")
      if [ -f "$system/kernel-params" ]; then
        KERNEL_PARAMS=("$(${pkgs.busybox}/bin/cat $system/kernel-params)" "''${KERNEL_PARAMS[@]}")
      else
        KERNEL_PARAMS=("" "''${KERNEL_PARAMS[@]}")
      fi
    fi
  fi

  if [ ''${#GENERATIONS[@]} -eq 0 ]; then
    echo "ERROR: No NixOS generations found in ${cfg.mountPrefix}/nix/var/nix/profiles/"
    echo "Checked for system-*-link directories with kernel and initrd files."
    exit 1
  fi

  echo "Found ''${#GENERATIONS[@]} generation(s)"
''
