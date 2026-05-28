# Emits /etc/nmbl/config.toml consumed by nmbl-init-rs at PID 1.
#
# Schema mirrors the Rust structs in
# `sirati-nmbl/nmbl-init-rs/src/config.rs` (which uses
# `serde(deny_unknown_fields)`, so the wire shape must match exactly:
# top-level tables `general`, `kernel_modules`, `tui`, `paths` and
# arrays-of-tables `filesystems`, `activations`).
#
# Used as a pure function: callers `import` this file and apply it
# with `{ pkgs, lib, config, nmblInit }` to get a derivation that has
# already been parse-validated by the Rust binary at build time. A
# schema mismatch crashes `nix build` rather than surprising the
# operator at boot.

{
  pkgs,
  lib,
  config,
  nmblInit,
}:

let
  cfg = config.boot.nmbl;

  tomlFormat = pkgs.formats.toml { };

  tomlValue = {
    general = {
      verbosity = cfg.verbosity;
      timeout_secs = cfg.timeoutSecs;
      device_timeout_secs = cfg.deviceTimeoutSecs;
      panic_report_dir = toString cfg.panicReportDir;
      # cfg.serialConsole is the legacy nullable string ("ttyS0,115200" or
      # null); the Rust struct wants a plain bool. Coerce here so users
      # keep configuring a single option.
      serial_console = cfg.serialConsole != null;
    };

    kernel_modules = {
      # Pre-console graphics drivers (phase 2a). Loaded before
      # `open_console` so the splash backend has a DRM card to attach
      # to in `qemu_kernel_invoke` mode (no kmod auto-load).
      early = cfg.earlyExplicitKernelModules;
      # Activation modules load FIRST: LUKS needs AES + cipher modes
      # registered with the kernel crypto API before encrypted_keys can
      # init successfully (it calls alloc_cipher("ecb(aes)") at module-
      # load time). The base explicit list typically contains dm-crypt
      # pulled in via boot.initrd.kernelModules, whose dep walk pulls
      # in encrypted_keys — so AES + ecb must be live before then.
      explicit = cfg.activation.extraKernelModules ++ cfg.explicitKernelModules;
      blacklist = cfg.blacklistedKernelModules;
      modules_dir = "/lib/modules";
    };

    filesystems = map (fs: {
      device = fs.device;
      mountpoint = fs.mountPoint;
      fstype = fs.fsType;
      # The Rust `FilesystemEntry.options` is a single comma-joined
      # String, not a Vec<String>. Strip fstab/systemd pseudo-options
      # (`x-*`, `nofail`, `_netdev`) — the kernel rejects them with
      # EINVAL because they are not real mount(2) flags.
      options = lib.concatStringsSep "," (
        builtins.filter (
          opt: !(lib.hasPrefix "x-" opt) && opt != "nofail" && opt != "_netdev"
        ) fs.options
      );
      is_root = fs.mountPoint == "/";
    }) (lib.attrValues cfg.fileSystems);

    # Rust field is `activations` (plural). Sibling F.3 produces the
    # list of pre-shaped blocks already matching the
    # `Activation` struct (kind, required_modules, binary, argv,
    # produces_devices, description, prompt_label).
    #
    # TOML has no `null`; `Option<String> + serde(default)` on the
    # Rust side accepts an *absent* key as None. Strip any null
    # attrs from each block so F.3 can naively set `prompt_label =
    # null` without breaking TOML emission.
    activations = map (lib.filterAttrs (_: v: v != null)) cfg.activation.activationBlocks;

    tui = {
      enable_editor = cfg.tui.enableEditor;
      show_kernel_params = cfg.tui.showKernelParams;
    };

    paths = {
      nix_profiles_dir = toString cfg.paths.nixProfilesDir;
      system_root = toString cfg.paths.systemRoot;
      shell = toString cfg.paths.shell;
    };

    # Rescue config consumed by the Rust `RescueConfig` struct (C.1).
    # `mode` maps to the `RescueMode` enum (`embedded` | `external` |
    # `none`); `sfs_path` is a boot-partition-relative path the Rust
    # side joins against the runtime boot mountpoint, so it is emitted
    # verbatim (no rewrite) and omitted when it matches the Rust-side
    # default basename (`nmbl-rescue.sfs`).
    #
    # Network-rescue keys (`network`, `default_url`, `default_sha256`)
    # mirror E.1's additions to RescueConfig. They are omitted entirely
    # when `cfg.rescue.network = false` so the Rust serde defaults take
    # effect — the wire shape for non-network builds stays unchanged.
    rescue =
      {
        mode = cfg.rescue.mode;
      }
      // lib.optionalAttrs (cfg.rescue.sfsPath != "nmbl-rescue.sfs") {
        sfs_path = cfg.rescue.sfsPath;
      }
      // lib.optionalAttrs cfg.rescue.network {
        network = true;
        default_url = cfg.rescue.defaultUrl;
        default_sha256 = cfg.rescue.defaultSha256;
      };

    # Operator-curated list of extra `/dev/<tty>` paths the picker
    # dialog offers as multiplex targets for the emergency shell.
    # Empty by default — only `/dev/console` (the kernel-elected
    # primary interactive console) is offered. The Rust serde struct
    # is `EmergencyShellConfig`.
    emergency_shell = {
      extra_consoles = cfg.emergencyShell.extraConsoles;
    };
  }
  # Splash rendering. Emitted only when the graphical splash is enabled
  # so the validator (`deny_unknown_fields`) accepts the TOML on builds
  # that don't pull in the Rust-side `splash` config struct. Runtime
  # paths are fixed because the initramfs landing locations (see
  # lib/config.nix `splashContents`) are fixed.
  // lib.optionalAttrs cfg.splash.enable {
    splash = {
      enable           = true;
      background_image = "/etc/splash/image.png";
      font_path        = "/etc/splash/font.ttf";
      dri_path         = "/dev/dri/card0";
    };
  };

  rawToml = tomlFormat.generate "nmbl-config.toml" tomlValue;
in
pkgs.runCommand "nmbl-config.toml" { } ''
  ${nmblInit}/bin/nmbl-init --validate-config=${rawToml}
  cp ${rawToml} $out
''
