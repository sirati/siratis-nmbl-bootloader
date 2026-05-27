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
