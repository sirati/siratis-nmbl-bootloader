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

  # Check for nmbl.shell kernel parameter to drop to emergency shell
  if ${pkgs.busybox}/bin/cat /proc/cmdline | ${pkgs.busybox}/bin/grep -q "nmbl.shell"; then
    echo "NMBL: Kernel parameter 'nmbl.shell' detected"
    echo "NMBL: Dropping to emergency shell as requested"
    echo ""
    echo "Available generations:"
    ${pkgs.busybox}/bin/cat /tmp/generations.txt | while IFS='|' read -r gen_id kernel initrd params; do
      echo "  Generation $gen_id: $kernel"
    done
    echo ""
    echo "Use 'exit' to continue boot process with first generation"
    echo ""
    ${pkgs.busybox}/bin/sh
    echo ""
    echo "NMBL: Continuing boot process..."
  fi

  # Count generations from our temp file
  GEN_COUNT=$(${pkgs.busybox}/bin/wc -l < /tmp/generations.txt)

  # Auto-select first generation with timeout countdown
  echo "NMBL: Auto-selecting first generation in ${toString cfg.timeoutSeconds} seconds..."
  echo "NMBL: (Press any key to drop to emergency shell)"

  # Countdown with visual feedback
  TIMEOUT=${toString cfg.timeoutSeconds}
  while [ $TIMEOUT -gt 0 ]; do
    echo "NMBL: Booting in $TIMEOUT..."

    # Sleep for 1 second
    ${pkgs.busybox}/bin/sleep 1

    TIMEOUT=$((TIMEOUT - 1))
  done

  echo ""

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
  # - Toggle passthrough params
  # - Edit kernel params
''
