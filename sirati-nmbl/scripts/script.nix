# Main Init Script Builder
# This file combines all the script parts into one complete init script

{
  lib,
  pkgs,
  cfg,
  fileSystems,
  utils,
}:

let
  # Import all script components
  mountAndKernelScript = import ./mount-and-kernel.sh.nix {
    inherit
      lib
      pkgs
      cfg
      fileSystems
      utils
      ;
  };
  findGenerationsScript = import ./find-generations.sh.nix { inherit lib pkgs cfg; };
  selectionUIScript = import ./selection-ui.sh.nix { inherit lib pkgs cfg; };
  kexecBootScript = import ./kexec-boot.sh.nix {
    inherit
      lib
      pkgs
      cfg
      fileSystems
      utils
      ;
  };

in
pkgs.writeScript "init" ''
  #!${pkgs.busybox}/bin/sh

  # Fallback shell function for debugging
  fallback_shell() {
    echo ""
    echo "=========================================="
    echo "ERROR: Boot process failed!"
    echo "=========================================="
    echo "Dropping to emergency shell for debugging."
    echo "You can inspect /proc, /sys, /dev, etc."
    echo ""
    exec ${pkgs.busybox}/bin/sh
  }

  # Trap errors and drop to shell
  trap 'fallback_shell' EXIT

  ${mountAndKernelScript}

  ${findGenerationsScript}

  ${selectionUIScript}

  ${kexecBootScript}

  # If we get here successfully, disable the trap
  trap - EXIT
''
