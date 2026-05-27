# Kernel Module Management Module
# Handles kernel module loading, filtering, and closure creation for NMBL

{
  lib,
  pkgs,
  config,
  cfg,
  # Caller-supplied modules to merge into the explicit-load list
  # (e.g. NIC drivers added when boot.nmbl.rescue.network = true).
  # Defaults to [] so the existing call sites stay unchanged.
  extraExplicitModules ? [ ],
}:

rec {
  # Modules to load explicitly during NMBL boot
  # These are modules that must be loaded explicitly (not on-demand by udev)
  # We only load boot.initrd.kernelModules, not availableKernelModules
  # Filter out blacklisted modules
  explicitKernelModules = lib.unique (
    lib.filter (m: !(lib.elem m cfg.blacklistedKernelModules)) (
      cfg.kernelModules ++ config.boot.initrd.kernelModules ++ extraExplicitModules
    )
  );

  # Pre-console graphics drivers. Loaded by NMBL in phase 2a, before the
  # splash backend attaches to /dev/dri/card*. Kept disjoint from
  # `explicitKernelModules` so the operator's split between
  # `boot.nmbl.earlyKernelModules` and `boot.nmbl.kernelModules` is
  # preserved on the wire.
  earlyKernelModules = lib.unique (
    lib.filter (m: !(lib.elem m cfg.blacklistedKernelModules))
      cfg.earlyKernelModules
  );

  # Modules to include in the initrd (available but not necessarily loaded)
  # Include both explicit and available modules so they can be loaded on-demand if needed.
  # `activation.extraKernelModules` is contributed by ./activation.nix for storage
  # activation (LUKS / LVM / mdraid / zfs); read defensively so this still
  # evaluates if the activation module isn't imported.
  activationExtraKernelModules = (cfg.activation or { }).extraKernelModules or [ ];
  allKernelModules = lib.unique (
    lib.filter (m: !(lib.elem m cfg.blacklistedKernelModules)) (
      cfg.availableKernelModules
      ++ explicitKernelModules
      ++ earlyKernelModules
      ++ config.boot.initrd.availableKernelModules
      ++ activationExtraKernelModules
    )
  );

  # Create modprobe.d configuration for blacklisted modules
  modprobeConf = pkgs.writeText "nmbl-modprobe.conf" (
    lib.concatMapStringsSep "\n" (mod: "blacklist ${mod}") cfg.blacklistedKernelModules
  );

  # Create modules tree for our bootloader kernel (same as system.modulesTree in kernel.nix)
  # This gets the "modules" output from the kernel package
  bootloaderModulesTree = pkgs.aggregateModules [
    (lib.getOutput "modules" cfg.kernelPackage)
  ];

  # Create modules closure with kernel modules for our bootloader kernel
  # Note: If modules aren't found, they may be built-in to the kernel
  modulesClosure = pkgs.makeModulesClosure {
    rootModules = allKernelModules;
    kernel = bootloaderModulesTree;
    # Inherit firmware from system configuration (same as stage-1)
    firmware = config.hardware.firmware;
    allowMissing = true;
  };
}
