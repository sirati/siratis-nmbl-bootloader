# Interactive Selection UI Script
# Returns a shell script string for the user interface to select NixOS generations

{
  lib,
  pkgs,
  cfg,
}:

''
  # ============================================
  # Part 3: Selection UI
  # ============================================

  # Get current kernel parameters
  CURRENT_PARAMS=$(cat /proc/cmdline)

  # Filter out NMBL-specific params for passthrough
  PASSTHROUGH_PARAMS=""
  ${lib.optionalString (cfg.kernelParams != [ ]) ''
    for param in $CURRENT_PARAMS; do
      skip=0
      ${lib.concatMapStringsSep "\n" (p: ''
        if echo "$param" | grep -q "^${lib.escapeShellArg (lib.head (lib.splitString "=" p))}"; then
          skip=1
        fi
      '') cfg.kernelParams}
      if [ $skip -eq 0 ]; then
        PASSTHROUGH_PARAMS="$PASSTHROUGH_PARAMS $param"
      fi
    done
  ''}
  ${lib.optionalString (cfg.kernelParams == [ ]) ''
    PASSTHROUGH_PARAMS="$CURRENT_PARAMS"
  ''}

  # NMBL kernel params
  NMBL_PARAMS="${lib.concatStringsSep " " cfg.kernelParams}"

  # Main menu loop
  PASSTHROUGH_ENABLED=1
  CUSTOM_PARAMS=""
  EDIT_MODE=0

  while true; do
    clear
    echo "=== NixOS Linux Bootloader ==="
    echo ""
    echo "Bootloader Kernel Params: $NMBL_PARAMS"
    echo "Passthrough Params: $PASSTHROUGH_PARAMS"
    echo ""

    if [ $EDIT_MODE -eq 1 ]; then
      echo "[Custom params mode: $CUSTOM_PARAMS]"
      echo ""
    fi

    echo "Available Generations:"
    for i in $(seq 0 $((''${#GENERATIONS[@]} - 1))); do
      echo "  [$i] Generation ''${GENERATIONS[$i]}"
    done
    echo ""

    if [ $PASSTHROUGH_ENABLED -eq 1 ]; then
      echo "[X] Passthrough kernel params (enabled)"
    else
      echo "[ ] Passthrough kernel params (disabled)"
    fi
    echo ""
    echo "Commands:"
    echo "  0-9: Select generation to boot"
    echo "  p: Toggle passthrough kernel params"
    echo "  e: Edit kernel params"
    echo "  s: Drop to shell"
    echo ""
    echo -n "Select option (auto-boot 0 in ${toString cfg.timeoutSeconds}s): "

    # Read with timeout
    INPUT=""
    if read -t ${toString cfg.timeoutSeconds} INPUT; then
      # Process input
      case "$INPUT" in
        p|P)
          if [ $PASSTHROUGH_ENABLED -eq 1 ]; then
            PASSTHROUGH_ENABLED=0
          else
            PASSTHROUGH_ENABLED=1
          fi
          continue
          ;;
        e|E)
          echo ""
          echo "Enter custom kernel parameters:"
          read -e -i "$CUSTOM_PARAMS" CUSTOM_PARAMS
          EDIT_MODE=1
          continue
          ;;
        s|S)
          echo ""
          echo "Dropping into shell..."
          exec ${pkgs.bash}/bin/bash
          ;;
        [0-9])
          if [ "$INPUT" -ge 0 ] && [ "$INPUT" -lt "''${#GENERATIONS[@]}" ]; then
            SELECTED=$INPUT
            break
          else
            echo "Invalid selection!"
            sleep 1
            continue
          fi
          ;;
        *)
          echo "Invalid input!"
          sleep 1
          continue
          ;;
      esac
    else
      # Timeout, boot default
      SELECTED=0
      break
    fi
  done
''
