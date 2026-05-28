# NixOS Module Config Implementation for NMBL
# This file contains the actual implementation of the bootloader module

{
  config,
  lib,
  pkgs,
  utils,
  nmblInit,
  nmblInitSplash,
  # Builder form supplied by flake.nix. When extra Cargo features are
  # requested (e.g. `network-rescue` when `boot.nmbl.rescue.network`
  # is enabled, combined with `image-splash` when both are on) we
  # re-build the binary with those features. Defaults to ignoring
  # features and returning the prebuilt `nmblInit` if the host flake
  # is older.
  mkNmblInit ? (_: nmblInit),
  ...
}:

let
  cfg = config.boot.nmbl;
  bootstrapper = cfg.bootstrapper;

  # Cargo features to enable in the /init binary. Gated on splash and
  # rescue options so feature-free builds (default) stay byte-identical
  # to today's binary. When only `image-splash` is requested we prefer
  # the prebuilt `nmblInitSplash` to keep the existing CI cache hot;
  # when only `network-rescue` is requested we use `mkNmblInit`. When
  # both are requested we build a combined binary via `mkNmblInit`.
  nmblFeatures =
    lib.optional cfg.splash.enable "image-splash"
    ++ lib.optional cfg.rescue.network "network-rescue"
    ++ lib.optional cfg.stateful.enable "stateful";

  # Resolved /init binary used by the initramfs builder. Identity-equal
  # to the prebuilt `nmblInit` / `nmblInitSplash` in the single-feature
  # cases so Nix's store-path dedup keeps the existing CI cache hot.
  selectedNmblInit =
    if nmblFeatures == [ ] then
      nmblInit
    else if nmblFeatures == [ "image-splash" ] then
      nmblInitSplash
    else if nmblFeatures == [ "stateful" ] then
      mkNmblInit { features = [ "stateful" ]; }
    else
      mkNmblInit { features = nmblFeatures; };

  # Activation options are contributed by ./modules/activation.nix. Read
  # defensively so this file still evaluates if that module hasn't been
  # imported yet (e.g. during sibling-subtask staggered merges). The
  # activationBlocks list itself is consumed by ./config-toml.nix, not here.
  # Activation assertions are written to top-level `assertions` by the
  # activation module itself, so we deliberately do not re-append them
  # here (doing so would duplicate every activation assertion).
  activationCfg = cfg.activation or { };
  activationExtraContents = activationCfg.extraContents or [ ];

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

  # Auto-detected NIC drivers from hardware-configuration.nix
  # (boot.initrd.{kernelModules,availableKernelModules}). Only consumed
  # when the rescue network path is enabled; otherwise the list is
  # built but never referenced.
  detectedNicModules = import ./modules/nic-modules.nix {
    inherit lib config;
  };

  # Modules NMBL must load + ship for the network rescue fallback. Empty
  # unless rescue.network is on AND the rescue lives off-initramfs
  # (external mode), so the default build stays byte-identical.
  rescueNicModules =
    if cfg.rescue.network && cfg.rescue.mode == "external" then
      lib.unique (cfg.rescue.nicDrivers ++ detectedNicModules)
    else
      [ ];

  # loop + squashfs are required for the external-rescue disk path: NMBL
  # calls LOOP_CTL_GET_FREE on /dev/loop-control (needs the loop driver)
  # and then mounts the .sfs as squashfs. Both modules must be in the
  # initramfs so they can be insmod'd on demand before allocate_loop_device.
  rescueDiskModules =
    if cfg.rescue.mode == "external" then [ "loop" "squashfs" ] else [ ];

  # af_packet is required by the DHCP client (socket(AF_PACKET, SOCK_DGRAM,
  # ETH_P_IP)). Without this module the raw-socket DHCP exchange fails with
  # EAFNOSUPPORT even though the NIC driver is loaded. Include it whenever
  # the network-rescue path is compiled in.
  rescuePacketModule =
    if cfg.rescue.network && cfg.rescue.mode == "external" then [ "af_packet" ] else [ ];

  # Import kernel modules management module
  kernelModulesManager = import ./modules/kernel-modules.nix {
    inherit
      lib
      pkgs
      config
      cfg
      ;
    extraExplicitModules = lib.unique (rescueNicModules ++ rescueDiskModules ++ rescuePacketModule);
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

  # Render the runtime configuration that the Rust /init reads at startup.
  # All previously string-interpolated state (filesystems, modules, timeouts,
  # serial console, verbosity, activation blocks) lives in this TOML file.
  # The Rust binary used for `--validate-config` must match the one shipped
  # in the initramfs, otherwise we could validate against a different schema
  # than what actually runs at boot.
  nmblConfigToml = import ./config-toml.nix {
    inherit pkgs lib config;
    nmblInit = selectedNmblInit;
  };

  # Render the embedded bootstrap TOML used in external-config mode.
  # The bootstrap file points at /boot's device + fs + the relative path
  # to the full config.toml on the boot partition. Helper owned by B.2.
  nmblBootstrapToml = import ./bootstrap-toml.nix {
    inherit pkgs lib config;
  };

  # Build the external rescue squashfs from `cfg.rescue.squashfsContents`.
  # The derivation is always evaluated (cheap when nothing references it)
  # but only staged onto the boot partition when `cfg.rescue.mode ==
  # "external"`. Embedded / none modes keep today's behaviour.
  nmblRescueSquashfs = import ./rescue-sfs.nix {
    inherit pkgs lib;
    contents = cfg.rescue.squashfsContents;
  };

  # Where the runtime config TOML lives at boot. In embedded mode it
  # ships inside the initramfs; in external mode the initramfs only
  # carries the bootstrap TOML and the full config is staged onto the
  # boot partition by install-bootloader.nix. Default mirrors the Rust
  # bootstrap default (`/nmbl/config.toml`) so the field is defensible
  # if B.2's option tree hasn't been merged yet.
  configLocation = cfg.configLocation or "embedded";

  # Operator input device baseline. NMBL replaces systemd-stage-1, so
  # NixOS's automatic input-driver inclusion does not apply. Any
  # interactive screen (LUKS passphrase, emergency menu, wrong-password
  # modal, console picker) is unusable without these — applied to both
  # earlyKernelModules (so they are live before the first prompt) and
  # availableKernelModules (so they ship in the initramfs).
  defaultKeyboardDrivers = [
    "i8042"
    "atkbd"
    "usbhid"
    "hid_generic"
    "xhci_pci"
    "ehci_pci"
  ];

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

    # Import assertions from assertions module. Activation-module
    # assertions are written to `assertions` by ./modules/activation.nix
    # itself; do not re-append them here or every activation assertion
    # would fire twice.
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

    # Build the minimal initramfs around the Rust /init binary.
    #
    # Contents are deliberately small:
    #   - /init                       : the static-musl Rust binary (PID 1)
    #   - /etc/nmbl/config.toml       : runtime config (embedded mode only)
    #   - /etc/nmbl/bootstrap.toml    : minimal bootstrap config used to
    #                                   locate the full config on /boot
    #                                   (external mode only)
    #   - /bin/sh                     : busybox, used ONLY for the emergency
    #                                   shell on failure (never by /init
    #                                   itself); staged only when
    #                                   `rescue.mode = "embedded"`. For the
    #                                   "external" / "none" modes the
    #                                   emergency path either pivots into
    #                                   the rescue squashfs (which carries
    #                                   its own /bin/sh) or halts with a
    #                                   structured banner — no in-initramfs
    #                                   shell is reachable.
    #   - /bin/blkid                  : util-linux's blkid, called by the
    #                                   Rust /init to populate /dev/disk/by-*
    #                                   symlinks (udev-less stage-0).
    #   - /lib/modules                : kernel modules closure
    #   - /etc/modprobe.d/nixos.conf  : blacklist config
    #
    # Storage-activation tooling (cryptsetup / lvm2 / mdadm / zfs) is added
    # conditionally via cfg.activation.extraContents — populated by
    # ./modules/activation.nix only when fileSystems require it.
    system.build.nmblInitramfs =
      let
        # External-config mode embeds ONLY bootstrap.toml; the full
        # config.toml is staged onto /boot by install-bootloader.nix.
        # That separation is the whole point of external mode: edits to
        # the runtime config no longer require an initramfs rebuild.
        configContents =
          if configLocation == "external" then
            [
              {
                object = nmblBootstrapToml;
                symlink = "/etc/nmbl/bootstrap.toml";
              }
            ]
          else
            [
              {
                object = nmblConfigToml;
                symlink = "/etc/nmbl/config.toml";
              }
            ];

        # Busybox is only needed in the initramfs when the emergency
        # path execs `cfg.paths.shell` directly (embedded mode). In
        # "external" mode the rescue squashfs carries its own /bin/sh
        # and is loop-mounted before any shell exec; in "none" mode the
        # rescue dispatcher halts via `halt_with_banner` without ever
        # touching a shell. Keeping busybox out of the initramfs for
        # those two modes is the bulk of the F.6 size delta.
        shellContents = lib.optional (cfg.rescue.mode == "embedded") {
          object = "${pkgs.busybox}/bin/busybox";
          symlink = "/bin/sh";
        };

        baseContents = [
          {
            object = "${selectedNmblInit}/bin/nmbl-init";
            symlink = "/init";
          }
        ] ++ configContents ++ shellContents ++ [
          {
            # util-linux's blkid is used by nmbl-init to populate
            # /dev/disk/by-{partlabel,label,uuid,partuuid}/ symlinks
            # at boot time, since NMBL ships without udev. The Rust
            # crate shells out via the activation runner (run_capture)
            # to read blkid -o export output for every block device
            # the kernel knows about. Mirrors the bash bootloader's
            # approach (commit 534fe5d).
            object = "${pkgs.util-linux}/bin/blkid";
            symlink = "/bin/blkid";
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

        # When the splash UI is enabled, ship the background image and
        # the menu font at the fixed paths the Rust /init expects (see
        # lib/config-toml.nix `splash.background_image`/`splash.font_path`).
        splashContents = lib.optionals cfg.splash.enable [
          {
            object = cfg.splash.backgroundImage;
            symlink = "/etc/splash/image.png";
          }
          {
            object = cfg.splash.fontPath;
            symlink = "/etc/splash/font.ttf";
          }
        ];

        initramfs = pkgs.makeInitrd {
          contents = baseContents ++ splashContents ++ activationExtraContents;
          compressor = "gzip -9";
        };
      in
      # Force assertion checking before returning initramfs
      # builtins.seq forces evaluation of the first argument before returning the second
      builtins.seq config.system.build.nmblAssertionCheck initramfs;

    # Build the bootloader kernel
    system.build.nmblKernel = cfg.kernelPackage;

    # Expose the actually-selected /init binary for downstream tooling
    # (debug scripts, manual nix builds). Mirrors nmblKernel/nmblInitramfs.
    system.build.nmblInit = selectedNmblInit;

    # Expose the rendered runtime config TOML so it can be inspected
    # (and validated) independently of the initramfs build. Used by
    # the C.2 validation step (`nix eval ... --raw`) and by the
    # external-config staging path inside install-bootloader.nix.
    system.build.nmblConfigToml = nmblConfigToml;

    # Expose the external rescue squashfs derivation. Always evaluable
    # (the underlying derivation is built from `cfg.rescue.squashfsContents`
    # regardless of mode), but only staged onto the boot partition when
    # `cfg.rescue.mode == "external"` — see install-bootloader.nix.
    system.build.nmblRescueSquashfs = nmblRescueSquashfs;

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

    boot.nmbl.earlyKernelModules = defaultKeyboardDrivers;
    boot.nmbl.availableKernelModules = defaultKeyboardDrivers;

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
        configLocation
        nmblConfigToml
        nmblRescueSquashfs
        ;
    };

    # Custom installation script (imported from module)
    system.build.installNmbl = installScriptModule.installNmbl;

    # Add install-nmbl to system packages. kexec-tools is no longer required:
    # the Rust /init drives kexec via the kexec_file_load(2) syscall directly.
    environment.systemPackages = [
      installScriptModule.installNmbl
    ];
  };
}
