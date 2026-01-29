# Interactive Selection UI Script
# Returns a shell script string for the user interface to select NixOS generations
# POSIX-compliant - works with busybox sh (no bash arrays)

{
  lib,
  pkgs,
  cfg,
}:

''
  # ============================================
  # Part 3: Selection UI
  # ============================================

  echo "NMBL: Preparing boot selection interface..."

  # Count generations from our temp file
  GEN_COUNT=$(${pkgs.busybox}/bin/wc -l < /tmp/generations.txt)

  # Simple selection for now - just boot the first (newest/current) generation
  echo "NMBL: Auto-selecting first generation (timeout: ${toString cfg.timeoutSeconds}s)"

  # Read first line from generations file
  SELECTED_LINE=$(${pkgs.busybox}/bin/head -n 1 /tmp/generations.txt)

  # Parse the line (format: gen_id|kernel|initrd|params)
  SELECTED_GEN=$(echo "$SELECTED_LINE" | ${pkgs.busybox}/bin/cut -d'|' -f1)
  SELECTED_KERNEL=$(echo "$SELECTED_LINE" | ${pkgs.busybox}/bin/cut -d'|' -f2)
  SELECTED_INITRD=$(echo "$SELECTED_LINE" | ${pkgs.busybox}/bin/cut -d'|' -f3)
  SELECTED_PARAMS=$(echo "$SELECTED_LINE" | ${pkgs.busybox}/bin/cut -d'|' -f4)

  echo "NMBL: Selected generation: $SELECTED_GEN"
  echo "NMBL:   Kernel: $SELECTED_KERNEL"
  echo "NMBL:   Initrd: $SELECTED_INITRD"
  echo "NMBL:   Params: $SELECTED_PARAMS"

  # TODO: Implement full interactive menu with:
  # - List all generations with numbered menu
  # - Keyboard input for selection
  # - Timeout with countdown
  # - Toggle passthrough params
  # - Edit kernel params
  # For now, we auto-select the first generation for testing
''
