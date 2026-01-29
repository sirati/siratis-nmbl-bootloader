# Main Init Script Builder
# This file combines all the script parts into one complete init script

{
  lib,
  pkgs,
  cfg,
}:

let
  # Import all script components
  mountAndKernelScript = import ./mount-and-kernel.sh.nix { inherit lib pkgs cfg; };
  findGenerationsScript = import ./find-generations.sh.nix { inherit lib pkgs cfg; };
  selectionUIScript = import ./selection-ui.sh.nix { inherit lib pkgs cfg; };
  kexecBootScript = import ./kexec-boot.sh.nix { inherit lib pkgs cfg; };

in
pkgs.writeScript "init" ''
  #!${pkgs.busybox}/bin/sh
  set -e

  ${mountAndKernelScript}

  ${findGenerationsScript}

  ${selectionUIScript}

  ${kexecBootScript}
''
