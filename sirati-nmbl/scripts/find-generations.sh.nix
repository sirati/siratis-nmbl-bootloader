# Find NixOS Generations Script
# Returns a shell script string for discovering available NixOS generations
# POSIX-compliant - works with busybox sh (no bash arrays)

{
  lib,
  pkgs,
  cfg,
}:

''
  # ============================================
  # Part 2: Find NixOS Generations
  # ============================================

  info "NMBL: Discovering NixOS generations..."

  # Check if root filesystem is mounted
  if [ ! -d "${cfg.mountPrefix}" ]; then
    echo "ERROR: Mount prefix ${cfg.mountPrefix} not found!"
    exit 1
  fi

  # Create temporary file to store generation info
  # Format: generation_id|kernel_path|initrd_path|kernel_params
  GEN_FILE="/tmp/generations.txt"
  > "$GEN_FILE"  # Clear/create file

  # Helper function to resolve symlinks with mount prefix
  resolve_system_path() {
    local link_path="$1"
    if [ -L "$link_path" ]; then
      # Read the symlink target
      local target=$(${pkgs.busybox}/bin/readlink "$link_path")
      # If target is absolute, prepend mount prefix
      case "$target" in
        /*)
          echo "${cfg.mountPrefix}$target"
          ;;
        *)
          # Relative path - resolve relative to link directory
          echo "$(${pkgs.busybox}/bin/dirname "$link_path")/$target"
          ;;
      esac
    else
      echo "$link_path"
    fi
  }

  # Parse NixOS system profiles (sorted by version, newest first)
  # Note: We use shell globbing directly because ls -d fails on broken symlinks
  for system_link in ${cfg.mountPrefix}/nix/var/nix/profiles/system-*-link; do
    # Skip if glob didn't match anything
    [ -e "$system_link" ] || [ -L "$system_link" ] || continue

    # Resolve the symlink to get actual system path
    system=$(resolve_system_path "$system_link")

    if [ -L "$system/kernel" ] && [ -L "$system/initrd" ]; then
      gen_num=$(${pkgs.busybox}/bin/basename "$system_link" | ${pkgs.busybox}/bin/sed 's/system-\(.*\)-link/\1/')
      kernel_path="$system/kernel"
      initrd_path="$system/initrd"

      # Extract kernel parameters
      if [ -f "$system/kernel-params" ]; then
        kernel_params=$(${pkgs.busybox}/bin/cat "$system/kernel-params")
      else
        kernel_params=""
      fi

      # Store generation info (use | as delimiter)
      echo "$gen_num|$kernel_path|$initrd_path|$kernel_params" >> "$GEN_FILE"
    fi
  done

  # Also check current system (add it at the beginning)
  if [ -L "${cfg.mountPrefix}/nix/var/nix/profiles/system" ]; then
    system_link="${cfg.mountPrefix}/nix/var/nix/profiles/system"
    # Resolve the symlink to get actual system path
    system=$(resolve_system_path "$system_link")

    if [ -L "$system/kernel" ] && [ -L "$system/initrd" ]; then
      kernel_path="$system/kernel"
      initrd_path="$system/initrd"

      if [ -f "$system/kernel-params" ]; then
        kernel_params=$(${pkgs.busybox}/bin/cat "$system/kernel-params")
      else
        kernel_params=""
      fi

      # Prepend current system to temp file
      TMP_FILE="/tmp/generations.tmp"
      echo "current|$kernel_path|$initrd_path|$kernel_params" > "$TMP_FILE"
      ${pkgs.busybox}/bin/cat "$GEN_FILE" >> "$TMP_FILE"
      ${pkgs.busybox}/bin/mv "$TMP_FILE" "$GEN_FILE"
    fi
  fi

  # Count generations
  GEN_COUNT=$(${pkgs.busybox}/bin/wc -l < "$GEN_FILE")

  if [ "$GEN_COUNT" -eq 0 ]; then
    echo "ERROR: No NixOS generations found in ${cfg.mountPrefix}/nix/var/nix/profiles/"
    echo "Checked for system-*-link directories with kernel and initrd files."
    exit 1
  fi

  info "Found $GEN_COUNT generation(s)"

  # Debug: show what we found
  info "NMBL: Available generations:"
  ${pkgs.busybox}/bin/cat "$GEN_FILE" | while IFS='|' read -r gen_id kernel initrd params; do
    info "  - Generation $gen_id: kernel=$kernel"
  done
''
