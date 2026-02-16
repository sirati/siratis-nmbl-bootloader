# Main Init Script Builder
# This file combines all the script parts into one complete init script

{
  lib,
  pkgs,
  cfg,
  fileSystems,
  utils,
  kernelModules,
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
      kernelModules
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

  # Set PATH early so all tools are available
  export PATH="${pkgs.busybox}/bin:${pkgs.kmod}/bin:${pkgs.kexec-tools}/bin:/sbin:/usr/sbin:/bin:/usr/bin"

  # Verbose output control (like NixOS stage-1)
  verbose="${toString cfg.verbose}"
  info() {
    if [ -n "$verbose" ]; then
      echo "$@"
    fi
  }

  # Fallback shell function for debugging
  fallback_shell() {
    echo ""
    echo "=========================================="
    echo "ERROR: Boot process failed!"
    echo "=========================================="
    echo "Dropping to emergency shell for debugging."
    echo "You can inspect /proc, /sys, /dev, etc."
    echo ""
    echo "PATH is set to: $PATH"
    echo ""
    echo "Debug Info:"
    echo "  Kernel: $(uname -r)"
    echo "  Available modules: $(ls /lib/modules 2>/dev/null || echo 'none')"
    echo "  Block devices: $(ls /dev/sd* /dev/vd* 2>/dev/null | tr '\n' ' ' || echo 'none')"
    echo ""
    echo "Useful commands:"
    echo "  modprobe <module>  - Load a kernel module"
    echo "  lsmod              - List loaded modules"
    echo "  ls /lib/modules/   - See available module versions"
    echo "  mount              - Show mounted filesystems"
    echo ""
    exec ${pkgs.busybox}/bin/sh
  }

  # Trap errors and drop to shell
  trap 'fallback_shell' EXIT

  # Print banner in light green (like NixOS does)
  echo -e "\033[1;32m<<< NixOS Stage 0 - sirati's nmbl >>>\033[0m"
  echo ""

  ${mountAndKernelScript}

  ${findGenerationsScript}

  ${selectionUIScript}

  ${kexecBootScript}

  # If we get here successfully, disable the trap
  trap - EXIT
''
