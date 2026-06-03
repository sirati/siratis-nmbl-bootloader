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
  # The host `nmbl-sign` ML-DSA image signer, used by the driver-image build
  # (lib/modules/driver-image.nix) to sign each squashfs at install time.
  # `null` on an older host flake; driver-image.nix only errors WHEN driver
  # images are enabled.
  nmblSign ? null,
  ...
}:

let
  cfg = config.boot.nmbl;
  bootstrapper = cfg.bootstrapper;

  # UKI build wiring + /init binary selection, extracted into
  # ./signing-build.nix. Produces the SAME `system.build.nmblUki` /
  # `system.build.nmblInit` derivations as before — `selectedNmblInit`
  # is the resolved /init binary, `nmblUki` the EFI-stub PE.
  signingBuild = import ./signing-build.nix {
    inherit
      pkgs
      lib
      config
      cfg
      nmblInit
      nmblInitSplash
      mkNmblInit
      ;
  };
  selectedNmblInit = signingBuild.selectedNmblInit;

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

  # loop + squashfs + overlay are required by NMBL ITSELF for the
  # external-rescue disk path: NMBL calls LOOP_CTL_GET_FREE on
  # /dev/loop-control (loop), mounts the .sfs as squashfs (squashfs), then
  # stacks a live-CD writable `/rescue` overlay (overlay) over the
  # read-only squashfs lower with a tmpfs upper — all BEFORE switch_root,
  # in NMBL's own kernel (the rescue /init runs too late to supply
  # overlay). They are NOT eagerly loaded on every boot (absent from
  # options.nix `explicitKernelModules`); the Rust dispatch loads them ON
  # DEMAND right before the loop-mount + overlay dance. But their .ko must
  # still SHIP in the initramfs so on-demand load can find them, so they
  # flow into the staged closure here (via `extraExplicitModules` ->
  # `allKernelModules` -> `modulesClosure`) without entering NMBL's runtime
  # eager-load list.
  rescueDiskModules =
    if cfg.rescue.mode == "external" then [ "loop" "squashfs" "overlay" ] else [ ];

  # af_packet is required by the DHCP client (socket(AF_PACKET, SOCK_DGRAM,
  # ETH_P_IP)). Without this module the raw-socket DHCP exchange fails with
  # EAFNOSUPPORT even though the NIC driver is loaded. Include it whenever
  # the network-rescue path is compiled in.
  rescuePacketModule =
    if cfg.rescue.network && cfg.rescue.mode == "external" then [ "af_packet" ] else [ ];

  # Modules the FULL RECOVERY SYSTEM needs after switch_root: its NIC
  # drivers (for dhcpcd), overlay + ext4 (the writable-overlay scratch),
  # and af_packet (dhcpcd's AF_PACKET/BPF socket). These are NOT loaded
  # into NMBL's own kernel/initramfs — NMBL does not need them. Instead
  # they are built into a SEPARATE module closure against NMBL's exact
  # kernel (`rescueModuleClosure` below) and shipped inside the rescue
  # squashfs, where the rescue `/init` modprobes them itself after
  # switch_root (the running kernel is still NMBL's, so `uname -r`
  # matches the staged tree). ext4's deps (mbcache, jbd2) and each NIC
  # driver's transport deps are pulled into the closure automatically.
  rescueFullSystemModules =
    if cfg.rescue.fullSystem.enable && cfg.rescue.mode == "external" then
      lib.unique (
        lib.filter (m: !(lib.elem m cfg.blacklistedKernelModules)) (
          cfg.rescue.nicDrivers ++ detectedNicModules ++ [ "overlay" "ext4" "af_packet" ]
        )
      )
    else
      [ ];

  # The kernel's module tree, for both NMBL's own closure and the rescue
  # closure. Same derivation as kernelModulesManager.bootloaderModulesTree.
  rescueModulesTree = pkgs.aggregateModules [
    (lib.getOutput "modules" cfg.kernelPackage)
  ];

  # Module closure for the rescue squashfs, built against NMBL's exact
  # kernel so `uname -r` after switch_root matches `/lib/modules/<kver>`.
  # `firmware` pulls in only the blobs the staged modules reference
  # (makeModulesClosure extracts per-module firmware requests), so passing
  # a big package like linux-firmware stays scoped. `allowMissing` keeps
  # the build green if a named driver is built into the kernel. Only built
  # when there are rescue modules to stage.
  #
  # De-duplicated (#25b) onto the shared `module-closure.nix` factor (#25a)
  # the driver-image build also uses. The factor joins the firmware list into
  # the SINGLE store path makeModulesClosure expects (the `nmbl-rescue-
  # firmware` buildEnv — exactly NixOS's `hardware.firmware` pattern), unique-s
  # the root modules (already unique here), and returns `null` when there is
  # nothing to stage — so this is store-path-identical to the prior inline
  # `makeModulesClosure` call (verified byte-for-byte against
  # `system.build.nmblRescueSquashfs` per FIX-37). The EXPLICIT `firmwareName`
  # keeps the rescue closure's firmware env from colliding with a driver-image
  # one (FIX-36).
  rescueModuleClosure = import ./modules/module-closure.nix { inherit pkgs lib; } {
    rootModules = rescueFullSystemModules;
    kernel = rescueModulesTree;
    firmware = cfg.rescue.fullSystem.firmware;
    firmwareName = "nmbl-rescue-firmware";
    allowMissing = true;
  };

  # Import kernel modules management module. The rescue full-system
  # modules are deliberately NOT in extraExplicitModules: NMBL must not
  # load or stage them (see rescueModuleClosure above). Only NMBL's own
  # needs (loop/squashfs for the loop-mount, and the network-rescue NIC
  # set when rescue.network is on) flow in here.
  kernelModulesManager = import ./modules/kernel-modules.nix {
    inherit
      lib
      pkgs
      config
      cfg
      ;
    extraExplicitModules = lib.unique (
      rescueNicModules ++ rescueDiskModules ++ rescuePacketModule
    );
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
    fullSystem = {
      inherit (cfg.rescue.fullSystem) enable packages sshdPort rootAuthorizedKeys;
      # NIC drivers the recovery /init modprobes ITSELF after switch_root.
      # NMBL no longer preloads them — the .ko + firmware ship in the
      # squashfs (moduleClosure below), and the running kernel is still
      # NMBL's, so `uname -r` matches the staged tree.
      nicDrivers = lib.unique (cfg.rescue.nicDrivers ++ detectedNicModules);
      # Filesystem / packet modules the recovery /init loads before the
      # overlay + network setup. Loaded from the staged tree, not NMBL's.
      coreModules = [ "overlay" "ext4" "af_packet" ];
      # The makeModulesClosure result (its /lib/modules + /lib/firmware are
      # staged into the squashfs root). null when there is nothing to load
      # (fullSystem disabled or non-external), in which case rescue-sfs.nix
      # skips the modprobe + staging.
      moduleClosure = rescueModuleClosure;
    };
  };

  # Optional signed driver-image squashfs build (#25a). ADDITIVE: builds each
  # `boot.nmbl.driverImages.images.<name>` into a pure squashfs (out-of-tree
  # .ko + firmware, via the explicit-`firmwareName` module-closure factor —
  # FIX-36) and emits the install-time `nmbl-sign --domain driver-image`
  # shell. The rescue closure above is built inline and is left untouched
  # (byte-identical store path — FIX-37); only the NEW images use the factor.
  driverImageBuild = import ./modules/driver-image.nix {
    inherit pkgs lib config cfg nmblSign rescueModulesTree;
  };

  # The emergency menu's "Raw Shell" forks `cfg.paths.shell` while NMBL is
  # PID 1 in the INITRAMFS (before any switch_root — see the Rust
  # `sys::pty::preflight_shell` and `shell::drop_to_emergency`). So a usable
  # shell binary must be staged into the initramfs whenever a rescue path
  # can drop the operator to that menu. We ship busybox as `/bin/sh` for
  # BOTH `embedded` and `external` modes: external used to omit it (the
  # rescue squashfs carries its own /bin/sh, reached only via the heavier
  # `rescue::dispatch` switch_root, NOT via the emergency menu), which left
  # the menu's Raw Shell with nothing to execve — the real bug this fixes.
  # `none` mode halts with a banner and never reaches the menu shell, so it
  # stays busybox-free.
  emergencyShellContents =
    lib.optional (cfg.rescue.mode == "embedded" || cfg.rescue.mode == "external") {
      object = "${pkgs.busybox}/bin/busybox";
      symlink = "/bin/sh";
    };

  # Absolute paths the initramfs stages as executables. The build check in
  # config-toml.nix asserts `cfg.paths.shell` is among these so an
  # external-rescue misconfiguration (shell pointing at a binary absent
  # from the initramfs) fails the build instead of dying silently at the
  # emergency menu. Kept in sync with `baseContents` below.
  initrdExecutablePaths =
    [ "/init" "/bin/blkid" ] ++ map (c: c.symlink) emergencyShellContents;

  # Render the runtime configuration that the Rust /init reads at startup.
  # All previously string-interpolated state (filesystems, modules, timeouts,
  # serial console, verbosity, activation blocks) lives in this TOML file.
  # The Rust binary used for `--validate-config` must match the one shipped
  # in the initramfs, otherwise we could validate against a different schema
  # than what actually runs at boot. The check is target-aware: it confirms
  # `paths.shell` resolves to a binary the initramfs stages, and — in
  # external mode — lists the rescue squashfs (`unsquashfs -l`, no FUSE) to
  # verify its switch_root handoff entrypoint exists.
  nmblConfigToml = import ./config-toml.nix {
    inherit pkgs lib config;
    nmblInit = selectedNmblInit;
    initrdExecutables = initrdExecutablePaths;
    rescueSfs = if cfg.rescue.mode == "external" then nmblRescueSquashfs else null;
    # `none` mode ships no emergency shell on purpose (the emergency path
    # halts with a banner), so do not assert paths.shell is staged there.
    checkEmergencyShell = cfg.rescue.mode != "none";
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
    #                                   menu's Raw Shell on failure (never by
    #                                   /init itself); staged for the
    #                                   "embedded" AND "external" modes so
    #                                   the menu has a binary to execve while
    #                                   NMBL is PID 1 in the initramfs. The
    #                                   "none" mode halts with a structured
    #                                   banner and never reaches the menu
    #                                   shell, so it ships no /bin/sh.
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

        # Busybox `/bin/sh` for the emergency menu's Raw Shell. Shared
        # with the build check via the top-level `emergencyShellContents`
        # so the initramfs and the check agree on exactly what is staged.
        # Present for `embedded` AND `external` (the latter previously
        # omitted it, breaking the menu's Raw Shell — the rescue squashfs
        # /bin/sh is only reachable via the heavier `rescue::dispatch`
        # switch_root, not the menu). `none` mode halts with a banner and
        # never reaches the menu shell, so it stays busybox-free.
        shellContents = emergencyShellContents;

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

        # When the splash UI is enabled, ship the menu font at the fixed
        # path the Rust /init expects (see lib/config-toml.nix
        # `splash.font_path`). The background image is embedded too ONLY
        # in `backgroundLocation = "initrd"` mode; in `"boot-partition"`
        # mode it is staged on the boot partition by
        # install-bootloader.nix and read at runtime, so we keep it OUT
        # of the initramfs to stay lean.
        splashBackgroundContents =
          lib.optionals (cfg.splash.enable && cfg.splash.backgroundLocation == "initrd") [
            {
              object = cfg.splash.backgroundImage;
              symlink = "/etc/splash/image.png";
            }
          ];
        splashContents = lib.optionals cfg.splash.enable [
          {
            object = cfg.splash.resolvedFontPath;
            symlink = "/etc/splash/font.ttf";
          }
        ] ++ splashBackgroundContents;

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

    # Build the NMBL UKI (Unified Kernel Image): the NMBL kernel + initrd
    # spliced into ONE EFI-stub PE via systemd's `ukify`. This is what the
    # `loader = "efi-stub"` install path drops at EFI/BOOT/BOOTX64.EFI so
    # the ESP holds ONLY NMBL (no GRUB/systemd-boot binary, no separate
    # kernel/initrd files — both live inside the PE's `.linux`/`.initrd`
    # sections, which systemd-stub hands to the kernel at boot).
    #
    # The cmdline matches `nmblBootConfig` (kernelParams + the optional
    # serial console=). x86_64 bzImage is already an EFI-stub-capable PE;
    # systemd-stub reliably passes the embedded `.initrd` section, so no
    # on-disk initrd is needed. Always evaluable (cheap when unreferenced);
    # only built when the efi-stub install path consumes it.
    system.build.nmblUki = signingBuild.nmblUki;

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

    # Expose the pure driver-image squashfs derivations (#25a) for store-path
    # introspection. Each record is `{ name; sfs; destPath; sigDest; }`; the
    # `.sfs` is the unsigned, pure blob (signing happens at install).
    system.build.nmblDriverImages = driverImageBuild.driverImages;

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
          if actualLoaderExtraArgs == null || !(actualLoaderExtraArgs ? timeout) then
            "N/A"
          else
            toString actualLoaderExtraArgs.timeout
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
      nmblUki = config.system.build.nmblUki;
      # Install-time driver-image staging + `nmbl-sign` signing shell (#25a).
      # Empty string when no driver images are enabled.
      driverImageInstallShell = driverImageBuild.driverImageInstallShell;
      # The host-platform `nmbl-sign` signer, threaded through to
      # install-signing.nix for per-generation kernel/initrd signing (#53).
      inherit nmblSign;
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
