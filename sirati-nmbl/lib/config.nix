# NixOS Module Config Implementation for NMBL
# This file contains the actual implementation of the bootloader module

{
  config,
  lib,
  pkgs,
  utils,
  ...
}:

let
  cfg = config.boot.nmbl;
  bootstrapper = cfg.bootstrapper;

  # Set default loader based on bootMode if not explicitly set
  actualLoader =
    if bootstrapper.loader != null then
      bootstrapper.loader
    else if bootstrapper.bootMode == "qemu_kernel_invoke" then
      null
    else
      "grub"; # Default for bios/uefi

  # Set default loader_extra_args if not explicitly set
  actualLoaderExtraArgs =
    if bootstrapper.loader_extra_args != null then
      bootstrapper.loader_extra_args
    else if bootstrapper.bootMode == "qemu_kernel_invoke" then
      null
    else
      { }; # Default empty set for bios/uefi

  # Get filesystems needed for boot
  # This includes filesystems marked with neededForBoot = true and those in critical paths
  fileSystems = builtins.filter utils.fsNeededForBoot (builtins.attrValues config.fileSystems);

  # All filesystems that NMBL needs to mount
  nmblFileSystems = fileSystems;

  # Import storage validation module
  storageValidation = import ./modules/storage-validation.nix { inherit lib; };

  # Import kernel modules management module
  kernelModulesManager = import ./modules/kernel-modules.nix {
    inherit
      lib
      pkgs
      config
      cfg
      ;
  };

  # Import assertions module
  assertionsModule = import ./modules/assertions.nix {
    inherit
      lib
      config
      cfg
      bootstrapper
      actualLoader
      actualLoaderExtraArgs
      storageValidation
      nmblFileSystems
      ;
  };

  # Import installation script module
  installScriptModule = import ./modules/install-script.nix {
    inherit
      lib
      pkgs
      config
      cfg
      bootstrapper
      actualLoader
      actualLoaderExtraArgs
      ;
  };

  # Determine actual verbose value (use boot.initrd.verbose if cfg.verbose is null)
  actualVerbose = if cfg.verbose == null then config.boot.initrd.verbose else cfg.verbose;

  # Build the complete init script by importing script.nix
  buildInitScript = import ../scripts/script.nix {
    inherit
      lib
      pkgs
      utils
      ;
    cfg = cfg // {
      verbose = actualVerbose;
    };
    fileSystems = nmblFileSystems;
    kernelModules = kernelModulesManager.explicitKernelModules;
  };

  # Determine legacy boot mode string for compatibility
  legacyBootMode =
    if bootstrapper.partition_table == "gpt" && bootstrapper.bootMode == "bios" then
      "gpt-bios"
    else if bootstrapper.partition_table == "gpt" && bootstrapper.bootMode == "uefi" then
      "gpt-uefi"
    else
      "gpt-${bootstrapper.bootMode}";

in
{
  config = lib.mkIf cfg.enable {
    # Mark boot partition as neededForBoot to ensure:
    # 1. Proper kernel modules are included (vfat, nls_cp437, nls_iso8859-1)
    # 2. Boot partition is treated as boot-critical by the system
    # 3. x-initrd.mount option is automatically added
    # Use mkOverride with priority 1000 (higher than default 1500) to ensure boot partition is marked as needed
    # This ensures vfat kernel modules are automatically included in the system initrd
    fileSystems."/boot".neededForBoot = lib.mkOverride 1000 true;

    # Import assertions from assertions module
    assertions = assertionsModule.assertions;

    # Force assertion checking - this will fail the build if any assertions are false
    # NixOS checks assertions in system.build.toplevel, but we need to ensure they're
    # checked even when building intermediate outputs like nmblInitramfs
    system.build.nmblAssertionCheck =
      let
        failedAssertions = lib.filter (x: !x.assertion) config.assertions;
        assertionMessages = lib.concatMapStringsSep "\n" (x: "- ${x.message}") failedAssertions;
      in
      if failedAssertions != [ ] then
        throw ''
          Failed assertions:
          ${assertionMessages}
        ''
      else
        pkgs.writeText "nmbl-assertions-ok" "All NMBL assertions passed\n";

    # Build the minimal initramfs
    system.build.nmblInitramfs =
      let
        initScript = buildInitScript;

        # Build minimal initramfs with only essential tools
        initramfs = pkgs.makeInitrd {
          contents = [
            {
              object = initScript;
              symlink = "/init";
            }
            {
              object = pkgs.busybox;
              symlink = "/bin/busybox";
            }
            {
              object = pkgs.bash;
              symlink = "/bin/bash";
            }
            {
              object = pkgs.kexec-tools;
              symlink = "/bin/kexec";
            }
            {
              object = pkgs.kmod;
              symlink = "/bin/kmod";
            }
            {
              # util-linux gives us blkid for populating /dev/disk/by-partlabel/
              # symlinks at boot (NMBL has no udev to do this automatically).
              object = pkgs.util-linux;
              symlink = "/bin/util-linux";
            }
            {
              # mdadm so the mount-and-kernel stage can `--assemble --scan`
              # any mdraid arrays present on disk before mounting.
              object = pkgs.mdadm;
              symlink = "/bin/mdadm";
            }
            {
              # btrfs-progs so mount-and-kernel can `btrfs device scan` for
              # multi-device btrfs filesystems before mounting (NMBL has no
              # udev to auto-discover the other members).
              object = pkgs.btrfs-progs;
              symlink = "/bin/btrfs-progs";
            }
            {
              object = "${kernelModulesManager.modulesClosure}/lib/modules";
              symlink = "/lib/modules";
            }
            {
              object = kernelModulesManager.modprobeConf;
              symlink = "/etc/modprobe.d/nixos.conf";
            }
          ];

          compressor = "gzip -9";
        };
      in
      # Force assertion checking before returning initramfs
      # builtins.seq forces evaluation of the first argument before returning the second
      builtins.seq config.system.build.nmblAssertionCheck initramfs;

    # Build the bootloader kernel
    system.build.nmblKernel = cfg.kernelPackage;

    # Debug output to verify module configuration
    system.build.nmblDebugInfo = pkgs.writeText "nmbl-debug-info" ''
      NMBL Bootloader Configuration Debug Info
      ========================================

      Filesystems to mount (neededForBoot):
      ${lib.concatMapStringsSep "\n" (
        fs: "  - ${fs.mountPoint}: ${fs.fsType} (${fs.device or "no device"})"
      ) nmblFileSystems}

      boot.initrd.supportedFilesystems:
      ${lib.concatStringsSep "\n" (
        lib.mapAttrsToList (
          fsType: enabled: "  - ${fsType}: ${if enabled then "true" else "false"}"
        ) config.boot.initrd.supportedFilesystems
      )}

      Kernel modules to load explicitly:
      ${lib.concatMapStringsSep "\n" (mod: "  - ${mod}") kernelModulesManager.explicitKernelModules}

      All kernel modules in initramfs (available):
      ${lib.concatMapStringsSep "\n" (mod: "  - ${mod}") kernelModulesManager.allKernelModules}

      Modules from config.boot.initrd.kernelModules:
      ${lib.concatMapStringsSep "\n" (mod: "  - ${mod}") config.boot.initrd.kernelModules}

      Modules from config.boot.initrd.availableKernelModules:
      ${lib.concatMapStringsSep "\n" (mod: "  - ${mod}") config.boot.initrd.availableKernelModules}

      Blacklisted modules:
      ${lib.concatMapStringsSep "\n" (mod: "  - ${mod}") cfg.blacklistedKernelModules}
    '';

    # Generate bootloader configuration based on boot mode
    system.build.nmblBootConfig =
      let
        kernel = config.system.build.nmblKernel;
        initrd = config.system.build.nmblInitramfs;
        kernelParams = lib.concatStringsSep " " (
          cfg.kernelParams ++ lib.optional (cfg.serialConsole != null) "console=${cfg.serialConsole}"
        );
      in
      pkgs.writeText "nmbl-boot-config" ''
        Partition Table: ${bootstrapper.partition_table}
        Boot Mode: ${bootstrapper.bootMode}
        Loader: ${if actualLoader == null then "none (qemu_kernel_invoke)" else actualLoader}
        Kernel: ${kernel}/bzImage
        Initrd: ${initrd}/initrd
        Kernel Parameters: ${kernelParams}
        Loader Timeout: ${
          if actualLoaderExtraArgs == null then "N/A" else toString actualLoaderExtraArgs.timeout
        }
      '';

    # Boot loader installation - disable standard bootloaders
    boot.loader.grub.enable = lib.mkDefault false;
    boot.loader.systemd-boot.enable = lib.mkDefault false;

    # Register NMBL as the active bootloader (required by NixOS)
    system.boot.loader.id = "nmbl";

    # NMBL supports initrd secrets since it has an initramfs
    boot.loader.supportsInitrdSecrets = true;

    # Populate boot.initrd.supportedFilesystems using the same logic as stage-1.nix
    # This triggers filesystem-specific modules (vfat.nix, ext.nix, etc.) to add their
    # kernel modules to boot.initrd.availableKernelModules and boot.initrd.kernelModules
    # which we then include in our bootloader's initramfs
    #
    # stage-1.nix does: boot.initrd.supportedFilesystems = map (fs: fs.fsType) fileSystems;
    # where fileSystems = filter utils.fsNeededForBoot config.system.build.fileSystems;
    #
    # We do the same but convert the list to an attrset as expected by filesystem modules
    boot.initrd.supportedFilesystems = lib.mkOptionDefault (
      lib.listToAttrs (map (fs: lib.nameValuePair fs.fsType true) nmblFileSystems)
    );

    # Hook for NixOS to install NMBL bootloader during VM builds and system installations
    system.build.installBootLoader = import ./install-bootloader.nix {
      inherit
        lib
        pkgs
        config
        cfg
        bootstrapper
        legacyBootMode
        ;
    };

    # Custom installation script (imported from module)
    system.build.installNmbl = installScriptModule.installNmbl;

    # Add kexec-tools and install-nmbl to system packages
    environment.systemPackages = [
      pkgs.kexec-tools
      installScriptModule.installNmbl
    ];
  };
}
