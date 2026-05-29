# NixOS Module Options for NMBL (NixOS Minimal BootLoader)
# This file defines all configuration options available for the bootloader

{ config, lib, pkgs, utils, ... }:

let
  cfg = config.boot.nmbl;

  # Filesystem-driver modules derived from `config.fileSystems.*.fsType`.
  # NMBL has no udev to auto-load drivers on mount(2), so anything that
  # gets mounted before kexec must appear in the explicit-load list.
  # Pure function over `lib` + `config`, so importing it here at the
  # let-binding level is safe (no extra module-system fixpoint).
  fsDerivedKernelModules = import ./modules/fs-modules.nix {
    inherit lib config;
  };

  # The cosmic-greeter repo stores assets via Git LFS, so the
  # raw.githubusercontent.com URL returns a pointer file. The
  # media.githubusercontent.com prefix transparently smudges LFS
  # content and gives us the actual JPEG.
  cosmicGreeterBackgroundPng = pkgs.runCommand "cosmic-greeter-background.png"
    { nativeBuildInputs = [ pkgs.imagemagick ]; }
    ''
      magick ${pkgs.fetchurl {
        url = "https://media.githubusercontent.com/media/pop-os/cosmic-greeter/master/res/background.jpg";
        sha256 = "sha256-dQD3AvBIjUqN8sWr63ypEHp8p5mOBEFyfLr3lGWwI4g=";
      }} -strip -define png:color-type=6 "$out"
    '';

  # Default splash font: Adobe Source Code Pro (SIL OFL 1.1, freely
  # redistributable). The upstream release tarball lays the faces out as
  # `OTF/SourceCodePro-Regular.otf` at its root, which is exactly the
  # `font.dir = "OTF"` / `font.variant = "SourceCodePro-Regular"`
  # defaults below. The nixpkgs `source-code-pro` attr instead uses the
  # XDG `share/fonts/opentype/...` layout, so we fetch the Adobe release
  # directly to keep the out-of-the-box defaults resolvable.
  sourceCodeProDefault = pkgs.fetchzip {
    url = "https://github.com/adobe-fonts/source-code-pro/archive/refs/tags/2.042R-u/1.062R-i/1.026R-vf.tar.gz";
    sha256 = "sha256-Pl7cuBFtbk9tPv421ejKnKFKdsW6oezMnAGCWKI3OVY=";
  };

  # Normalise a font.dir value: strip a leading `./` and any
  # leading/trailing slashes so callers may write `OTF`, `./OTF`,
  # `OTF/`, or `./OTF/` interchangeably.
  normalizeFontDir = dir:
    let
      noDotSlash = lib.removePrefix "./" dir;
      noLead = lib.removePrefix "/" noDotSlash;
    in
    lib.removeSuffix "/" noLead;

  # Resolve `font.package`/`font.dir`/`font.variant` into the concrete
  # `.otf` store path and VALIDATE at build time that it exists. The
  # validator `test -f`s the resolved file; on failure it `ls`es the
  # directory (or, if the dir itself is missing, the package root) so
  # the build log lists the available options, then exits non-zero.
  # Avoids import-from-derivation: the resolved font is emitted as the
  # derivation's output so it can be embedded into the initramfs
  # directly. Only OTF is supported, so `.otf` is implied.
  resolveSplashFont = font:
    let
      dir = normalizeFontDir font.dir;
      rel = "${dir}/${font.variant}.otf";
    in
    pkgs.runCommand "nmbl-splash-font.otf" { } ''
      pkg=${font.package}
      rel=${lib.escapeShellArg rel}
      src="$pkg/$rel"
      if [ ! -f "$src" ]; then
        echo "nmbl splash font not found: $rel" >&2
        echo "  resolved from package $pkg" >&2
        if [ -d "$pkg/${lib.escapeShellArg dir}" ]; then
          echo "  available faces in '${dir}':" >&2
          ls -1 "$pkg/${lib.escapeShellArg dir}" >&2 || true
        else
          echo "  directory '${dir}' does not exist in the package; package root:" >&2
          ls -1 "$pkg" >&2 || true
        fi
        exit 1
      fi
      cp "$src" "$out"
    '';

  # Auto-detected NIC driver modules from hardware-configuration.nix.
  # Pure function over `lib` + `config`; consumed only when
  # `boot.nmbl.rescue.network = true`.
  nicDetectedKernelModules = import ./modules/nic-modules.nix {
    inherit lib config;
  };
in
{
  imports = [
    ./modules/activation.nix
    ./modules/log-import.nix
    ./modules/stateful.nix
  ];

  options.boot.nmbl = {
    enable = lib.mkEnableOption "Linux-as-bootloader (NMBL)";

    bootstrapper = lib.mkOption {
      type = lib.types.submodule {
        options = {
          partition_table = lib.mkOption {
            type = lib.types.enum [ "gpt" ];
            default = "gpt";
            description = lib.mdDoc ''
              Partition table type for the bootloader.
              Currently only GPT is supported.
            '';
          };

          bootMode = lib.mkOption {
            type = lib.types.enum [
              "bios"
              "uefi"
              "qemu_kernel_invoke"
            ];
            default = "uefi";
            description = lib.mdDoc ''
              Boot mode for the system:
              - bios: Legacy BIOS boot with GPT partition table (requires BIOS boot partition)
              - uefi: UEFI boot with GPT partition table (requires ESP)
              - qemu_kernel_invoke: Direct kernel invocation by QEMU (bypasses bootloader installation)
            '';
          };

          loader = lib.mkOption {
            type = lib.types.nullOr (
              lib.types.enum [
                "grub"
                "systemd"
              ]
            );
            default = null;
            description = lib.mdDoc ''
              Bootloader to use:
              - grub: GRUB bootloader (supports both BIOS and UEFI)
              - systemd: systemd-boot (UEFI only, formerly gummiboot)
              - null: No loader (used for qemu_kernel_invoke mode)

              Defaults to "grub" for bios/uefi modes, null for qemu_kernel_invoke.
            '';
          };

          loader_extra_args = lib.mkOption {
            type = lib.types.nullOr (
              lib.types.submodule {
                options = {
                  timeout = lib.mkOption {
                    type = lib.types.int;
                    default = 0;
                    description = lib.mdDoc ''
                      Timeout in seconds before auto-selecting the default boot entry.
                      Set to 0 for immediate boot with no menu delay.
                    '';
                  };

                  canTouchEfiVariables = lib.mkOption {
                    type = lib.types.bool;
                    default = false;
                    description = lib.mdDoc ''
                      Whether the installation process is allowed to modify EFI boot variables.
                      Only applies to UEFI boot mode.
                    '';
                  };

                  efiInstallAsRemovable = lib.mkOption {
                    type = lib.types.bool;
                    default = false;
                    description = lib.mdDoc ''
                      Whether to install the bootloader as a removable device.
                      This installs to the fallback path (EFI/BOOT/BOOTX64.EFI) which
                      firmware looks for when no NVRAM entries exist.
                      Only applies to UEFI boot mode with GRUB.
                    '';
                  };

                  default = lib.mkOption {
                    type = lib.types.either lib.types.int lib.types.str;
                    default = "0";
                    apply = toString;
                    description = lib.mdDoc ''
                      Index of the default menu item to be booted.
                      Can also be set to "saved" for GRUB to remember the last selection.
                    '';
                  };

                  configurationLimit = lib.mkOption {
                    type = lib.types.int;
                    default = 100;
                    description = lib.mdDoc ''
                      Maximum number of configurations in boot menu.
                    '';
                  };

                  extraConfig = lib.mkOption {
                    type = lib.types.lines;
                    default = "";
                    example = ''
                      # GRUB example
                      set theme=$prefix/themes/starfield/theme.txt
                    '';
                    description = lib.mdDoc ''
                      Additional bootloader-specific configuration.
                      For GRUB: inserted before menu entries.
                      For systemd-boot: additional loader.conf settings.
                    '';
                  };

                  extraEntries = lib.mkOption {
                    type = lib.types.lines;
                    default = "";
                    example = ''
                      menuentry "Windows" {
                        chainloader (hd0,2)+1
                      }
                    '';
                    description = lib.mdDoc ''
                      Additional boot entries (GRUB-specific).
                    '';
                  };

                  theme = lib.mkOption {
                    type = lib.types.nullOr lib.types.path;
                    default = null;
                    example = lib.literalExpression ''"''${pkgs.kdePackages.breeze-grub}/grub/themes/breeze"'';
                    description = lib.mdDoc ''
                      Path to the bootloader theme (GRUB-specific).
                    '';
                  };
                };
              }
            );
            default = null;
            description = lib.mdDoc ''
              Extra arguments to pass to the bootloader configuration.
              These settings are merged with NMBL's bootloader configuration.
              The timeout is set to 0 by default for immediate boot.

              Set to null for qemu_kernel_invoke mode (no bootloader).
            '';
          };

          bootDisks = lib.mkOption {
            type = lib.types.listOf lib.types.str;
            default = [ ];
            example = [
              "/dev/nvme0n1"
              "/dev/nvme1n1"
            ];
            description = lib.mdDoc ''
              Block devices to install the bootloader to.

              When empty (the default), NMBL auto-discovers boot disks at
              install time:
              - BIOS: every whole disk with an EF02 partition gets
                `grub-install`, so each member of e.g. a RAID1 mirror is
                independently bootable.
              - UEFI: the single mounted /boot ESP is used; multi-ESP
                mirroring is currently a manual post-install step.

              Set this to pin the candidate set explicitly — e.g. when
              EF02 discovery would match disks you don't want touched, or
              when your layout doesn't expose EF02 partitions (rare).
            '';
          };
        };
      };
      default = { };
      description = lib.mdDoc ''
        Bootstrapper configuration for NMBL.
        Defines partition table, boot mode, loader type, and loader-specific settings.
      '';
    };

    kernelPackage = lib.mkOption {
      type = lib.types.package;
      default = pkgs.linux_6_6;
      defaultText = lib.literalExpression "pkgs.linux_6_6";
      description = lib.mdDoc ''
        Kernel package for the bootloader.
        It's recommended to use a pinned, stable kernel version (like linux_6_6)
        for the bootloader to ensure stability and predictability.
        The bootloader will automatically inherit the necessary kernel modules
        from your system's initrd configuration.
      '';
    };

    availableKernelModules = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ "crc32c" ];
      example = [
        "crc32c"
        "ext4"
        "virtio_blk"
      ];
      description = lib.mdDoc ''
        Base kernel modules available in the bootloader initramfs.
        These modules are always included and available for loading.
        The default includes crc32c which is required by ext4.
      '';
    };

    kernelModules = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [
        "nvme"
        "ahci"
        "sd_mod"
      ];
      description = lib.mdDoc ''
        Kernel modules to load explicitly in the bootloader initramfs.
        These are added to boot.initrd.kernelModules from your system configuration.
        The bootloader will also include all modules from boot.initrd.availableKernelModules
        in the initramfs (available but not loaded explicitly).
        Include modules needed for:
        - Your filesystem (ext4, btrfs, xfs, etc.)
        - Your storage controller (ahci, nvme, virtio_blk, etc.)

        Loaded in phase 2b, after the boot console is up so the operator
        sees per-module progress. For graphics drivers that must be in
        place BEFORE the splash console attaches (e.g. on
        `bootMode = "qemu_kernel_invoke"` where there is no kmod
        auto-load), use `boot.nmbl.earlyKernelModules` instead.
      '';
    };

    earlyKernelModules = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [
        "virtio_pci"
        "virtio_gpu"
        "simpledrm"
      ];
      description = lib.mdDoc ''
        Kernel modules loaded BEFORE the NMBL boot console is brought up.

        Reserved for graphics drivers that must populate
        `/dev/dri/card*` so the splash backend can attach. NMBL ships
        without udev / kmod auto-load — anything the splash needs to
        find at `open_console` time has to be listed here, otherwise
        the splash falls back to the tty console.

        Loaded in phase 2a, immediately after pseudo-filesystem mount
        and before the boot console open. Storage / filesystem drivers
        belong in `boot.nmbl.kernelModules` (phase 2b) so the operator
        sees their progress on the live console.
      '';
    };

    blacklistedKernelModules = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [
        "nouveau"
        "i915"
      ];
      description = lib.mdDoc ''
        List of kernel modules to blacklist in the bootloader initramfs.
        These modules will not be loaded even if requested.
        Useful for preventing problematic drivers from loading during boot.
      '';
    };

    mountPrefix = lib.mkOption {
      type = lib.types.str;
      default = "/mnt";
      example = "/mnt";
      description = lib.mdDoc ''
        Prefix path where filesystems will be mounted in the bootloader environment.
        For example, if set to "/mnt", the root filesystem (/) will be mounted at /mnt,
        /boot will be mounted at /mnt/boot, etc.

        This allows the bootloader to access all system filesystems read-only
        to find available NixOS generations for kexec.
      '';
    };

    kernelParams = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [
        "console=ttyS0,115200"
        "quiet"
      ];
      description = lib.mdDoc ''
        Extra kernel parameters for the NMBL bootloader kernel only.
        These parameters are used when booting the bootloader itself,
        not the target NixOS system.
      '';
    };

    timeoutSeconds = lib.mkOption {
      type = lib.types.int;
      default = 3;
      description = lib.mdDoc ''
        Timeout in seconds before auto-selecting the default boot entry.
        Set to 0 for no timeout (manual selection required).
        Note: This is for NMBL's own menu, not the underlying bootloader.
      '';
    };

    timeoutMillis = lib.mkOption {
      type = lib.types.int;
      default = cfg.timeoutSeconds * 1000;
      defaultText = lib.literalMD "`boot.nmbl.timeoutSeconds * 1000`.";
      description = lib.mdDoc ''
        Timeout in milliseconds before auto-selecting the default boot
        entry. Overrides `boot.nmbl.timeoutSeconds` at runtime, enabling
        sub-second auto-boot delays (e.g. `500` for a near-instant
        selector). Defaults to the whole-second `timeoutSeconds`
        converted to milliseconds, so leaving it unset preserves the
        historic behaviour.
        Note: This is for NMBL's own menu, not the underlying bootloader.
      '';
    };

    emergencyTimeoutSecs = lib.mkOption {
      type = lib.types.nullOr lib.types.ints.positive;
      default = null;
      description = lib.mdDoc ''
        Auto-reboot countdown (seconds) shown on NMBL's emergency/error
        screen when a boot phase fails and the operator is not present.
        When `null` (the default) the built-in 30 s budget applies; set
        a positive integer to override it (e.g. `1` for fast reboot
        loops on a headless test host).
      '';
    };

    deviceTimeoutSeconds = lib.mkOption {
      type = lib.types.ints.positive;
      default = 30;
      description = lib.mdDoc ''
        Per-device readiness budget (seconds) used while waiting for a
        `fileSystems.<name>.device` to appear during the system-root
        mount cascade, and while waiting for cryptsetup / LVM / mdraid
        activations to materialise the block devices they produce.

        Raise this on slow USB enclosures or hot-plug HBAs where a
        single device can take longer than the default 30 s to show up
        after its driver loads.
      '';
    };

    serialConsole = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "ttyS0,115200";
      description = lib.mdDoc ''
        Serial console configuration for input/output.
        Useful for headless systems or virtual machines.
        Format: device,baudrate (e.g., ttyS0,115200)
      '';
    };

    verbose = lib.mkOption {
      type = lib.types.nullOr lib.types.bool;
      default = null;
      description = lib.mdDoc ''
        Whether to show verbose messages during NMBL boot.
        When null (default), inherits the value from boot.initrd.verbose.
        Set to true for verbose output, false for silent boot (only critical messages will be shown).
      '';
    };

    ignoreMissingDiskModules = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = lib.mdDoc ''
        Whether to skip validation for missing storage driver kernel modules.

        NMBL validates that required storage drivers (like virtio_blk for /dev/vda*,
        nvme for NVMe drives, etc.) are available in boot.initrd.kernelModules or
        boot.initrd.availableKernelModules. This prevents boot failures where devices
        don't appear because drivers weren't loaded.

        Set to true to disable this validation if you know what you're doing or are
        using a custom kernel with built-in drivers.

        Default: false (validation enabled for safety)
      '';
    };

    # --- Runtime TOML options consumed by nmbl-init-rs -------------------
    # The fields below are surfaced to /etc/nmbl/config.toml via
    # lib/config-toml.nix. Their names follow Nix-side conventions
    # (camelCase); the emitter rewrites them to snake_case keys that match
    # the Rust serde structs in nmbl-init-rs/src/config.rs.

    verbosity = lib.mkOption {
      type = lib.types.enum [ "quiet" "info" "verbose" ];
      default =
        if cfg.verbose == true then "verbose"
        else if cfg.verbose == false then "quiet"
        else "info";
      defaultText = lib.literalMD ''derived from `boot.nmbl.verbose`: `true` → "verbose", `false` → "quiet", `null` → "info".'';
      description = lib.mdDoc ''
        Verbosity level for the NMBL /init runtime (mapped to the Rust
        `Verbosity` enum). Prefer this over the legacy nullable
        `boot.nmbl.verbose` option, which is kept for backwards
        compatibility.
      '';
    };

    timeoutMs = lib.mkOption {
      type = lib.types.int;
      default = cfg.timeoutMillis;
      defaultText = lib.literalMD "inherits from `boot.nmbl.timeoutMillis`.";
      description = lib.mdDoc ''
        Alias for `boot.nmbl.timeoutMillis` matching the snake_case
        `timeout_ms` key in the runtime TOML config consumed by
        nmbl-init-rs. This is the single selector countdown knob; set
        `boot.nmbl.timeoutSeconds` for whole seconds or
        `boot.nmbl.timeoutMillis` for sub-second delays.
      '';
    };

    deviceTimeoutSecs = lib.mkOption {
      type = lib.types.ints.positive;
      default = cfg.deviceTimeoutSeconds;
      defaultText = lib.literalMD "inherits from `boot.nmbl.deviceTimeoutSeconds`.";
      description = lib.mdDoc ''
        Alias for `boot.nmbl.deviceTimeoutSeconds` matching the
        snake_case `device_timeout_secs` key in the runtime TOML config
        consumed by nmbl-init-rs.
      '';
    };

    panicReportDir = lib.mkOption {
      type = lib.types.path;
      default = "/run";
      description = lib.mdDoc ''
        Directory inside the initramfs where the Rust /init writes panic
        reports before bailing into the emergency shell.
      '';
    };

    explicitKernelModules = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = lib.unique (
        lib.filter (m: !(lib.elem m cfg.blacklistedKernelModules)) (
          cfg.kernelModules
          ++ config.boot.initrd.kernelModules
          ++ fsDerivedKernelModules
          ++ lib.optionals (cfg.rescue.mode == "external") [
            # loop is needed by allocate_loop_device (LOOP_CTL_GET_FREE on
            # /dev/loop-control). squashfs is the filesystem type for the
            # rescue blob. Both must be loaded before rescue dispatch runs.
            "loop"
            "squashfs"
          ]
          ++ lib.optionals (cfg.rescue.network && cfg.rescue.mode == "external") (
            cfg.rescue.nicDrivers ++ nicDetectedKernelModules
            # af_packet is the kernel module that enables AF_PACKET sockets.
            # The DHCP client in rescue/net.rs opens socket(AF_PACKET, SOCK_DGRAM,
            # ETH_P_IP) to send/receive raw DHCP frames; without this module the
            # call fails with EAFNOSUPPORT before any packet leaves the NIC.
            ++ [ "af_packet" ]
          )
          # The full recovery system runs its own dhcpcd + sshd after
          # switch_root, so the NIC drivers must be loaded into NMBL's
          # kernel before handoff (the squashfs has no kmod-loadable
          # modules of its own for NMBL's kernel). modprobe inside /init
          # is then a no-op. Independent of rescue.network, which gates
          # NMBL's own in-initramfs DHCP/HTTP fetch path.
          #
          # overlay + ext4 back the full recovery system's writable
          # overlays: its /init formats an ext4 image on a loop device and
          # layers a writable overlay over the read-only squashfs store,
          # /etc and /var. NMBL has no udev to auto-load filesystem drivers
          # on mount(2), so both must be loaded into NMBL's kernel before
          # switch_root or the overlay/ext4 mounts fail with "unknown
          # filesystem type". ext4's deps (mbcache, jbd2) are pulled in
          # automatically by the loader's modules.dep resolution.
          # af_packet backs dhcpcd's AF_PACKET/BPF socket for the DHCPv4
          # exchange the recovery /init runs after switch_root; without it
          # dhcp_openbpf fails with EAFNOSUPPORT and no IPv4 lease is acquired.
          ++ lib.optionals (cfg.rescue.fullSystem.enable && cfg.rescue.mode == "external") (
            cfg.rescue.nicDrivers ++ nicDetectedKernelModules ++ [ "overlay" "ext4" "af_packet" ]
          )
        )
      );
      defaultText = lib.literalMD ''
        union of `boot.nmbl.kernelModules`, `boot.initrd.kernelModules`,
        and filesystem driver modules derived from
        `config.fileSystems.*.fsType` (e.g. `ext4`, `vfat`, `btrfs`),
        plus `boot.nmbl.rescue.nicDrivers` and any NIC modules detected
        from hardware-configuration when
        `boot.nmbl.rescue.network = true` and
        `boot.nmbl.rescue.mode = "external"`, with
        `boot.nmbl.blacklistedKernelModules` removed.
      '';
      description = lib.mdDoc ''
        Kernel modules the NMBL /init will load explicitly at startup
        (modprobe-style). Computed by default from
        `boot.nmbl.kernelModules` plus `boot.initrd.kernelModules`
        plus filesystem-driver modules derived from
        `config.fileSystems.*.fsType` (NMBL has no udev to auto-load
        them on mount(2)); set directly to override.
      '';
    };

    earlyExplicitKernelModules = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = lib.unique (
        lib.filter (m: !(lib.elem m cfg.blacklistedKernelModules))
          cfg.earlyKernelModules
      );
      defaultText = lib.literalMD ''
        `boot.nmbl.earlyKernelModules` with
        `boot.nmbl.blacklistedKernelModules` removed.
      '';
      description = lib.mdDoc ''
        Kernel modules the NMBL /init will load BEFORE bringing up the
        boot console. Computed by default from
        `boot.nmbl.earlyKernelModules` with the blacklist applied; set
        directly to override.

        Surfaced to the runtime TOML config as
        `kernel_modules.early`; the Rust side calls
        `modules::load_modules(_, ModuleSet::Early)` in phase 2a,
        immediately before `open_console`.
      '';
    };

    fileSystems = lib.mkOption {
      type = lib.types.attrsOf lib.types.attrs;
      internal = true;
      readOnly = true;
      default = lib.filterAttrs (_: utils.fsNeededForBoot) config.fileSystems;
      defaultText = lib.literalMD ''
        the subset of `config.fileSystems` matched by `utils.fsNeededForBoot`
        — i.e. `neededForBoot = true` plus stage-1's hardcoded
        `pathsNeededForBoot` set (`/`, `/nix`, `/var`, `/etc`, `/usr`).
      '';
      description = lib.mdDoc ''
        Filesystem set NMBL mounts before kexec'ing the target system.
        Uses the standard NixOS stage-1 filter `utils.fsNeededForBoot`
        directly, so the set matches what initrd-1 would mount, and is
        exposed as an attribute so `lib/config-toml.nix` can serialise
        it without re-importing `utils`.
      '';
    };

    tui = {
      enableEditor = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = lib.mdDoc ''
          Whether the TUI allows in-place editing of the kernel
          command line before kexec.
        '';
      };

      showKernelParams = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = lib.mdDoc ''
          Whether the TUI displays the resolved kernel command line for
          the highlighted generation.
        '';
      };
    };

    splash = {
      enable = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = lib.mdDoc ''
          Render the boot menu as a PNG-backed graphical splash via
          simpledrm. Falls back to the tty UI on any failure. Selecting
          this also switches `system.build.nmblInit` to the
          `nmbl-init-splash` package built with the `image-splash`
          cargo feature.
        '';
      };

      backgroundImage = lib.mkOption {
        type = lib.types.path;
        default = cosmicGreeterBackgroundPng;
        defaultText = lib.literalExpression "cosmic-greeter background.jpg converted to PNG";
        description = lib.mdDoc ''
          PNG to use as the splash background. Must be a real PNG, RGBA8.
          The default is the cosmic-greeter project's background.jpg,
          fetched at Nix build time and converted to PNG via imagemagick.

          Where this PNG lands is controlled by `backgroundLocation`:
          with `"initrd"` (the default) it is embedded into the
          initramfs at `/etc/splash/image.png`; with `"boot-partition"`
          it is staged on the boot partition next to the initrd as a
          sidecar (`nmblsplash.png`) and read at runtime, keeping the
          initramfs lean.
        '';
      };

      # --- Splash background location (sidecar on the boot partition) ---
      # Mirrors `rescue.mode`'s shape: `"initrd"` keeps today's embedded
      # behaviour (PNG baked into the initramfs); `"boot-partition"`
      # stages the PNG on the boot partition next to the initrd and the
      # Rust /init reads it at runtime from the Phase-0.5 mountpoint.
      # The sidecar basename is FIXED (`nmblsplash.png`) and not
      # configurable — it always sits next to `nmbl-initrd` at the boot
      # partition root.
      backgroundLocation = lib.mkOption {
        type = lib.types.enum [ "initrd" "boot-partition" ];
        default = "initrd";
        description = lib.mdDoc ''
          Where the splash background PNG lives:
          - `initrd`: embedded into the initramfs at
            `/etc/splash/image.png` (legacy behaviour; keeps the image
            available before any partition is mounted).
          - `boot-partition`: staged on the boot partition next to the
            initrd as a sidecar file (`nmblsplash.png`) and read at
            runtime, after the bootstrap stage (Phase 0.5) mounts the
            boot partition. This keeps the initramfs small. Requires
            `boot.nmbl.configLocation = "external"` so the boot
            partition is mounted before the splash comes up; if the
            sidecar is missing or unreadable NMBL renders the menu over
            a solid background instead of failing the boot. The sidecar
            filename is fixed and not configurable.
        '';
      };

      # --- Splash font (structured: package + dir + variant) ----------
      # The resolved face is `${font.package}/${font.dir}/${font.variant}.otf`
      # (dir normalised, `.otf` implied — only OTF is supported). The
      # resolved path is validated to EXIST at build time; a missing dir
      # or variant fails the build with the available options listed in
      # the build log. The validated face is embedded into the initramfs
      # at `/etc/splash/font.ttf` (path name kept for back-compat; the
      # Rust loader is format-agnostic and reads OTF fine).
      font = {
        package = lib.mkOption {
          type = lib.types.package;
          default = sourceCodeProDefault;
          defaultText = lib.literalExpression "Adobe Source Code Pro release (OFL)";
          description = lib.mdDoc ''
            Nix package (derivation) containing the splash font files.
            The face is resolved as
            `''${font.package}/''${font.dir}/''${font.variant}.otf`.

            The default is the Adobe Source Code Pro release, whose
            tarball lays faces out as `OTF/SourceCodePro-Regular.otf` at
            its root — matching the default `dir`/`variant`. Note the
            nixpkgs `source-code-pro` attr uses a different
            (`share/fonts/opentype/...`) layout, so if you point this at
            a nixpkgs font package you must adjust `dir`/`variant`
            accordingly.
          '';
        };

        dir = lib.mkOption {
          type = lib.types.str;
          default = "OTF";
          description = lib.mdDoc ''
            Subdirectory within `font.package` that holds the OTF faces.
            A leading `./` and/or surrounding slashes are normalised away,
            so `OTF`, `./OTF`, `OTF/`, and `./OTF/` are all equivalent.
          '';
        };

        variant = lib.mkOption {
          type = lib.types.str;
          default = "SourceCodePro-Regular";
          description = lib.mdDoc ''
            Face filename WITHOUT extension. Only OTF is supported, so the
            trailing `.otf` is implied. Resolved as
            `''${font.package}/''${font.dir}/''${font.variant}.otf`; if the
            file does not exist the build fails and lists the available
            faces in the build log.
          '';
        };
      };

      # Resolved + build-time-validated splash font face. Internal: it is
      # derived from `font.*` by `resolveSplashFont` and consumed by
      # `lib/config.nix` to embed the face into the initramfs at
      # `/etc/splash/font.ttf`. Forcing this build also runs the
      # existence validator (missing dir/variant fails with a listing).
      resolvedFontPath = lib.mkOption {
        type = lib.types.path;
        internal = true;
        readOnly = true;
        default = resolveSplashFont cfg.splash.font;
        defaultText = lib.literalMD
          "`font.package`/`font.dir`/`font.variant` resolved + validated to exist";
        description = lib.mdDoc ''
          The validated splash font face, embedded into the initramfs.
          Computed from `boot.nmbl.splash.font.*`; not set directly.
        '';
      };
    };

    # --- Bootstrap (Option 1: external config on the boot partition) ----
    # When `configLocation = "external"`, the initramfs only carries the
    # tiny bootstrap.toml emitted by `lib/bootstrap-toml.nix`; the full
    # runtime config lives on the boot partition and is loaded via Phase
    # 0.5 of the Rust /init. The default `"embedded"` keeps today's
    # behaviour (full config.toml embedded in the initramfs).
    configLocation = lib.mkOption {
      type = lib.types.enum [ "embedded" "external" ];
      default = "embedded";
      description = lib.mdDoc ''
        Where the NMBL runtime config lives: `"embedded"` ships the full
        `config.toml` inside the initramfs; `"external"` embeds only the
        bootstrap.toml descriptor and reads `config.toml` from the boot
        partition at runtime.
      '';
    };

    bootstrap = {
      configPath = lib.mkOption {
        # `types.str`, not `types.path`: this is a target-filesystem
        # path interpreted by the NMBL runtime, not a build-host path
        # Nix should resolve to the store.
        type = lib.types.str;
        default = "/nmbl/config.toml";
        description = lib.mdDoc ''
          Path to the full runtime `config.toml`, relative to
          `boot.nmbl.bootstrap.bootFs.mountpoint`, used when
          `configLocation = "external"`.
        '';
      };

      bootFs = {
        device = lib.mkOption {
          type = lib.types.str;
          default = "/dev/disk/by-partlabel/disk-main-ESP";
          example = "/dev/disk/by-partlabel/disk-main-ESP";
          description = lib.mdDoc ''
            Block device holding the boot partition that contains the
            external `config.toml`. Must be a `/dev/disk/by-*` symlink or
            a raw `/dev/...` path; short forms (`LABEL=`, `UUID=`,
            `PARTUUID=`) are rejected by the Rust loader.
          '';
        };

        fstype = lib.mkOption {
          type = lib.types.str;
          default = "vfat";
          description = lib.mdDoc ''
            Filesystem type of the boot partition (e.g. `vfat`, `ext4`).
          '';
        };

        options = lib.mkOption {
          type = lib.types.str;
          default = "ro";
          description = lib.mdDoc ''
            Comma-joined `mount(2)` options applied when the bootstrap
            stage mounts the boot partition. Defaults to read-only.
          '';
        };

        mountpoint = lib.mkOption {
          # `types.str`, not `types.path`: this is a target-filesystem
          # path interpreted by the NMBL runtime, not a build-host path
          # Nix should resolve to the store.
          type = lib.types.str;
          default = "/mnt/boot";
          description = lib.mdDoc ''
            Mountpoint inside the NMBL initramfs where the boot
            partition is mounted by the bootstrap stage.
          '';
        };
      };

      kernelModules = {
        explicit = lib.mkOption {
          type = lib.types.listOf lib.types.str;
          default = [ "vfat" "nls_cp437" "nls_iso8859_1" "ahci" "nvme" ];
          example = [ "vfat" "nls_cp437" "nls_iso8859_1" "ahci" "nvme" ];
          description = lib.mdDoc ''
            Kernel modules the bootstrap stage loads before mounting the
            boot partition. Must cover the boot filesystem driver and
            the storage controller drivers needed to expose its block
            device.
          '';
        };

        modulesDir = lib.mkOption {
          # `types.str`: target-fs path interpreted by the NMBL runtime,
          # not a build-host path Nix should resolve to the store.
          type = lib.types.str;
          default = "/lib/modules";
          description = lib.mdDoc ''
            Directory inside the NMBL initramfs that contains
            `modules.dep` for the bootstrap stage's module loader. Mirrors
            the analogous knob the full-config stage respects via the
            runtime `kernel_modules.modules_dir` key.
          '';
        };
      };

      rescue = {
        defaultUrl = lib.mkOption {
          type = lib.types.str;
          default = "";
          example = "https://example.invalid/rescue.cpio";
          description = lib.mdDoc ''
            Pre-filled URL for the rescue prompt (Option 2). Leave empty
            to omit; if set, `defaultSha256` must also be set — the Rust
            validator rejects half-configured rescue defaults.
          '';
        };

        defaultSha256 = lib.mkOption {
          type = lib.types.str;
          default = "";
          example = "deadbeef";
          description = lib.mdDoc ''
            Pre-filled SHA-256 for the rescue prompt (Option 2). Leave
            empty to omit; if set, `defaultUrl` must also be set.
          '';
        };
      };
    };

    # --- Rescue mode (Option 2: external rescue squashfs) ---------------
    # When `rescue.mode = "external"`, the initramfs no longer ships
    # busybox + storage activation tools. Instead, they live in a
    # squashfs blob (`nmbl-rescue.sfs`) on the boot partition, which
    # the Rust /init loop-mounts and switch_roots into when the
    # emergency shell is requested. The default `"embedded"` keeps the
    # legacy v1 behaviour so existing setups don't silently lose their
    # rescue path.
    rescue = {
      mode = lib.mkOption {
        type = lib.types.enum [ "embedded" "external" "none" ];
        default = "embedded";
        description = lib.mdDoc ''
          - `embedded`: busybox + activation tools live in the initramfs (legacy v1 behaviour).
          - `external`: tools live in `nmbl-rescue.sfs` on the boot partition; loop-mounted on demand.
          - `none`: no rescue tools shipped; PID 1 halts on the emergency path.
        '';
      };

      squashfsContents = lib.mkOption {
        type = lib.types.listOf lib.types.package;
        default = with pkgs; [ busybox-sandbox-shell cryptsetup lvm2 mdadm ];
        defaultText = lib.literalExpression "with pkgs; [ busybox-sandbox-shell cryptsetup lvm2 mdadm ]";
        description = lib.mdDoc ''
          Packages bundled into `nmbl-rescue.sfs` when
          `rescue.mode = "external"` and `rescue.fullSystem.enable =
          false`. The Rust loader expects `/bin/sh` to exist in the
          resulting tree (provided by busybox).
        '';
      };

      # --- Full recovery system (closure-store squashfs) ---------------
      # When `rescue.fullSystem.enable = true` (and `rescue.mode =
      # "external"`), the rescue squashfs is built with a real
      # /nix/store + nix-db instead of the flat busybox buildEnv tree.
      # The image then carries bash, btop, a root nix-daemon (flakes on),
      # and sshd — enough for `nix-shell -p` and remote recovery.
      fullSystem = {
        enable = lib.mkEnableOption "the closure-store minimal recovery system for external rescue";

        packages = lib.mkOption {
          type = lib.types.listOf lib.types.package;
          default = with pkgs; [
            bashInteractive
            btop
            nixVersions.stable
            openssh
            cacert
            btrfs-progs
            cryptsetup
            lvm2
            mdadm
            coreutils-full
            util-linux
            e2fsprogs
            iproute2
            dhcpcd
            gnugrep
            gnused
            gawk
            procps
            kmod
          ];
          defaultText = lib.literalExpression ''
            with pkgs; [ bashInteractive btop nixVersions.stable openssh
              cacert btrfs-progs cryptsetup lvm2 mdadm coreutils-full
              util-linux e2fsprogs iproute2 dhcpcd gnugrep gnused gawk procps kmod ]
          '';
          description = lib.mdDoc ''
            Packages whose closures are baked into the full recovery
            squashfs. Every store path in the combined closure is copied
            into the image's `/nix/store` and registered in the nix DB so
            `nix-shell -p` and `nix run` work offline against what is
            present (and online against substituters once DHCP is up).
          '';
        };

        sshdPort = lib.mkOption {
          type = lib.types.port;
          default = 22222;
          description = lib.mdDoc ''
            TCP port the recovery sshd listens on. Baked into the image's
            `/etc/ssh/sshd_config` as the `Port` directive.
          '';
        };

        rootAuthorizedKeys = lib.mkOption {
          type = lib.types.listOf lib.types.str;
          default = [ ];
          example = [ "ssh-ed25519 AAAA... operator@host" ];
          description = lib.mdDoc ''
            SSH public-key lines baked into the recovery image's
            `/root/.ssh/authorized_keys` (mode 600). With
            `PermitRootLogin prohibit-password` these are the only way to
            log in remotely as root.
          '';
        };
      };

      sfsPath = lib.mkOption {
        type = lib.types.str;
        default = "nmbl-rescue.sfs";
        description = lib.mdDoc ''
          Path on the boot partition, relative to the boot partition
          root, where the rescue squashfs is staged when
          `rescue.mode = "external"`. Leading slash is tolerated and
          stripped at install time and at runtime. The Rust disk-rescue
          path joins this against the runtime boot mountpoint
          (`bootstrap.bootFs.mountpoint` in bootstrap mode).
        '';
      };

      forceOnBoot = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = lib.mdDoc ''
          Test/recovery escape hatch. When true (and
          `rescue.mode = "external"`), NMBL skips the normal
          generation-boot flow and switch_roots straight into the rescue
          squashfs on every boot. The decision is made right after the
          boot partition is mounted and before any interactive console
          comes up, so it is a fully deterministic, no-input trigger —
          intended for automated rescue verification, not production.
          Leave `false` for normal installs.
        '';
      };

      network = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = lib.mdDoc ''
          Enable HTTP rescue fallback. When true, NMBL ships NIC
          drivers + DHCP + HTTP client in the initramfs so the rescue
          path can pull `nmbl-rescue.sfs` over the network when the
          disk copy is unavailable. Enables the Rust `network-rescue`
          Cargo feature in the built /init binary.
        '';
      };

      nicDrivers = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ "virtio_net" "e1000e" "igb" "r8169" ];
        description = lib.mdDoc ''
          Kernel modules bundled into the initramfs when
          `rescue.network = true`. NIC modules already recorded in
          `config.boot.initrd.availableKernelModules` or
          `config.boot.initrd.kernelModules` (typically by
          hardware-configuration.nix) are appended automatically and
          deduplicated.
        '';
      };

      defaultUrl = lib.mkOption {
        type = lib.types.str;
        default = "";
        example = "https://example.invalid/nmbl-rescue.sfs";
        description = lib.mdDoc ''
          Pre-fills the URL field in the network rescue prompt. Only
          emitted into the runtime TOML when `rescue.network = true`.
        '';
      };

      defaultSha256 = lib.mkOption {
        type = lib.types.str;
        default = "";
        example = "deadbeef";
        description = lib.mdDoc ''
          Pre-fills the SHA-256 field in the hash-confirm prompt. Only
          emitted into the runtime TOML when `rescue.network = true`.
        '';
      };
    };

    emergencyShell = {
      extraConsoles = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        example = [ "/dev/ttyS0" "/dev/tty1" ];
        description = lib.mdDoc ''
          Additional `/dev/<tty>` devices the operator may multiplex
          the emergency shell onto via the in-TUI picker dialog.

          Each entry MUST be a /dev path the operator considers
          acceptable to expose a root shell on. Untrusted serial
          access (IPMI SOL, server-room serial concentrators) becomes
          a privilege exposure when added here.

          Default: empty — only `/dev/console` (kernel-elected primary
          interactive console) gets the shell. The picker still lets
          the operator narrow the selection at runtime.
        '';
      };
    };

    paths = {
      nixProfilesDir = lib.mkOption {
        type = lib.types.path;
        default = "/mnt/system/nix/var/nix/profiles";
        description = lib.mdDoc ''
          Path (inside the NMBL initramfs, after mounting the target
          system) where NixOS system profile symlinks live.
        '';
      };

      systemRoot = lib.mkOption {
        type = lib.types.path;
        default = "/mnt/system";
        description = lib.mdDoc ''
          Mount point inside the NMBL initramfs where the target
          system's root filesystem is mounted.
        '';
      };

      shell = lib.mkOption {
        type = lib.types.path;
        default = "/bin/sh";
        description = lib.mdDoc ''
          Path inside the initramfs to the emergency shell binary
          (typically busybox `sh`) executed when /init bails out.
        '';
      };
    };
  };
}
