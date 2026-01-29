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

  BOOT_DIR="/mnt-root/boot"
  if [ ! -d "$BOOT_DIR" ]; then
    echo "Error: $BOOT_DIR not found"
    echo "Dropping into shell..."
    exec ${pkgs.bash}/bin/bash
  fi

  # Find all generations
  GENERATIONS=()
  KERNELS=()
  INITRDS=()
  KERNEL_PARAMS=()

  # Parse NixOS system profiles
  for system in $(ls -d /mnt-root/nix/var/nix/profiles/system-*-link 2>/dev/null | sort -V -r); do
    if [ -f "$system/kernel" ] && [ -f "$system/initrd" ]; then
      gen_num=$(basename "$system" | sed 's/system-\(.*\)-link/\1/')
      GENERATIONS+=("$gen_num")
      KERNELS+=("$system/kernel")
      INITRDS+=("$system/initrd")

      # Extract kernel parameters
      if [ -f "$system/kernel-params" ]; then
        KERNEL_PARAMS+=("$(cat $system/kernel-params)")
      else
        KERNEL_PARAMS+=("")
      fi
    fi
  done

  # Also check current system
  if [ -L "/mnt-root/nix/var/nix/profiles/system" ]; then
    system="/mnt-root/nix/var/nix/profiles/system"
    if [ -f "$system/kernel" ] && [ -f "$system/initrd" ]; then
      GENERATIONS=("current" "''${GENERATIONS[@]}")
      KERNELS=("$system/kernel" "''${KERNELS[@]}")
      INITRDS=("$system/initrd" "''${INITRDS[@]}")
      if [ -f "$system/kernel-params" ]; then
        KERNEL_PARAMS=("$(cat $system/kernel-params)" "''${KERNEL_PARAMS[@]}")
      else
        KERNEL_PARAMS=("" "''${KERNEL_PARAMS[@]}")
      fi
    fi
  fi

  if [ ''${#GENERATIONS[@]} -eq 0 ]; then
    echo "No NixOS generations found!"
    echo "Dropping into shell..."
    exec ${pkgs.bash}/bin/bash
  fi
''
