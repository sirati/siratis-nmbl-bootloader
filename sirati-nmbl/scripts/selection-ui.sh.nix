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
  CURRENT_PARAMS=$(${pkgs.busybox}/bin/cat /proc/cmdline)

  # Filter out NMBL-specific params for passthrough
  PASSTHROUGH_PARAMS=""
  ${lib.optionalString (cfg.kernelParams != [ ]) ''
    for param in $CURRENT_PARAMS; do
      skip=0
      ${lib.concatMapStringsSep "\n" (p: ''
        if ${pkgs.busybox}/bin/echo "$param" | ${pkgs.busybox}/bin/grep -q "^${lib.escapeShellArg (lib.head (lib.splitString "=" p))}"; then
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
    ${pkgs.busybox}/bin/clear
    ${pkgs.busybox}/bin/echo "=== NixOS Linux Bootloader ==="
    ${pkgs.busybox}/bin/echo ""
    ${pkgs.busybox}/bin/echo "Bootloader Kernel Params: $NMBL_PARAMS"
    ${pkgs.busybox}/bin/echo "Passthrough Params: $PASSTHROUGH_PARAMS"
    ${pkgs.busybox}/bin/echo ""

    if [ $EDIT_MODE -eq 1 ]; then
      ${pkgs.busybox}/bin/echo "[Custom params mode: $CUSTOM_PARAMS]"
      ${pkgs.busybox}/bin/echo ""
    fi

    ${pkgs.busybox}/bin/echo "Available Generations:"
    for i in $(${pkgs.busybox}/bin/seq 0 $((''${#GENERATIONS[@]} - 1))); do
      ${pkgs.busybox}/bin/echo "  [$i] Generation ''${GENERATIONS[$i]}"
    done
    ${pkgs.busybox}/bin/echo ""

    if [ $PASSTHROUGH_ENABLED -eq 1 ]; then
      ${pkgs.busybox}/bin/echo "[X] Passthrough kernel params (enabled)"
    else
      ${pkgs.busybox}/bin/echo "[ ] Passthrough kernel params (disabled)"
    fi
    ${pkgs.busybox}/bin/echo ""
    ${pkgs.busybox}/bin/echo "Commands:"
    ${pkgs.busybox}/bin/echo "  0-9: Select generation to boot"
    ${pkgs.busybox}/bin/echo "  p: Toggle passthrough kernel params"
    ${pkgs.busybox}/bin/echo "  e: Edit kernel params"
    ${pkgs.busybox}/bin/echo "  s: Drop to shell"
    ${pkgs.busybox}/bin/echo ""
    ${pkgs.busybox}/bin/echo -n "Select option (auto-boot 0 in ${toString cfg.timeoutSeconds}s): "

    # Read with timeout
    INPUT=""
    if ${pkgs.busybox}/bin/read -t ${toString cfg.timeoutSeconds} INPUT; then
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
          ${pkgs.busybox}/bin/echo ""
          ${pkgs.busybox}/bin/echo "Enter custom kernel parameters:"
          ${pkgs.busybox}/bin/read -e -i "$CUSTOM_PARAMS" CUSTOM_PARAMS
          EDIT_MODE=1
          continue
          ;;
        s|S)
          ${pkgs.busybox}/bin/echo ""
          ${pkgs.busybox}/bin/echo "Dropping into shell..."
          exec ${pkgs.bash}/bin/bash
          ;;
        [0-9])
          if [ "$INPUT" -ge 0 ] && [ "$INPUT" -lt "''${#GENERATIONS[@]}" ]; then
            SELECTED=$INPUT
            break
          else
            ${pkgs.busybox}/bin/echo "Invalid selection!"
            ${pkgs.busybox}/bin/sleep 1
            continue
          fi
          ;;
        *)
          ${pkgs.busybox}/bin/echo "Invalid input!"
          ${pkgs.busybox}/bin/sleep 1
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
