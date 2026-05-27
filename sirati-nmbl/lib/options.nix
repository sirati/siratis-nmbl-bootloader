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
in
{
  imports = [
    ./modules/activation.nix
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

    timeoutSecs = lib.mkOption {
      type = lib.types.int;
      default = cfg.timeoutSeconds;
      defaultText = lib.literalMD "inherits from `boot.nmbl.timeoutSeconds`.";
      description = lib.mdDoc ''
        Alias for `boot.nmbl.timeoutSeconds` matching the snake_case
        `timeout_secs` key in the runtime TOML config consumed by
        nmbl-init-rs.
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
        )
      );
      defaultText = lib.literalMD ''
        union of `boot.nmbl.kernelModules`, `boot.initrd.kernelModules`,
        and filesystem driver modules derived from
        `config.fileSystems.*.fsType` (e.g. `ext4`, `vfat`, `btrfs`),
        with `boot.nmbl.blacklistedKernelModules` removed.
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
          Embedded into the initramfs at `/etc/splash/image.png`. The
          default is the cosmic-greeter project's background.jpg, fetched
          at Nix build time and converted to PNG via imagemagick.
        '';
      };

      fontPath = lib.mkOption {
        type = lib.types.path;
        default = "${pkgs.dejavu_fonts}/share/fonts/truetype/DejaVuSansMono.ttf";
        defaultText = lib.literalExpression
          ''"''${pkgs.dejavu_fonts}/share/fonts/truetype/DejaVuSansMono.ttf"'';
        description = lib.mdDoc ''
          TrueType font (monospaced) used to rasterize the splash menu.
          Embedded into the initramfs at `/etc/splash/font.ttf`.
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
