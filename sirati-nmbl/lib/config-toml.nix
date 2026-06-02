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
  # Absolute paths the initramfs stages as executables (the `symlink`
  # fields of the initrd `contents`, e.g. `/init`, `/bin/sh`, `/bin/blkid`).
  # Used by the target-aware build check to confirm `paths.shell` actually
  # resolves to a binary present in the initramfs — the environment the
  # emergency shell forks in (NMBL is PID 1, before any switch_root).
  # Defaults to the always-present set so older callers still evaluate.
  initrdExecutables ? [ "/init" "/bin/blkid" ],
  # Whether to assert `paths.shell` is staged in the initramfs. True for
  # `rescue.mode` embedded/external (the emergency menu's Raw Shell forks
  # it); false for `none`, where the emergency path halts with a banner
  # and ships no shell on purpose — asserting there would break that
  # intentional config.
  checkEmergencyShell ? true,
  # The external rescue squashfs derivation (or null when not in external
  # mode). When non-null the check additionally lists it with
  # `unsquashfs -l` (no FUSE — works in the nix sandbox) and confirms the
  # rescue handoff entrypoint exists inside, so an external-rescue build
  # is verified against its actual rescue target.
  rescueSfs ? null,
}:

let
  cfg = config.boot.nmbl;

  tomlFormat = pkgs.formats.toml { };

  # Absolute path the emergency shell forks at runtime (NMBL PID 1, in the
  # initramfs). Mirrors the Rust `paths.shell` / preflight_shell check.
  shellPath = toString cfg.paths.shell;

  # The rescue handoff entrypoint baked into the external sfs:
  # `/init` for the full recovery system, `/bin/sh` (busybox) for the flat
  # image. We confirm this exists inside the sfs so the
  # `rescue::dispatch` / force-on-boot switch_root has something to exec.
  rescueEntrypoint =
    if cfg.rescue.mode == "external" && cfg.rescue.fullSystem.enable then "/init" else "/bin/sh";

  tomlValue = {
    general = {
      verbosity = cfg.verbosity;
      # Selector auto-boot countdown in milliseconds (defaults to
      # timeoutSeconds * 1000; set timeoutMillis for sub-second delays).
      timeout_ms = cfg.timeoutMs;
      device_timeout_secs = cfg.deviceTimeoutSecs;
      panic_report_dir = toString cfg.panicReportDir;
      # cfg.serialConsole drives kernel cmdline tagging at the Nix
      # layer (see ../nixos-modules/* for the `console=` plumbing);
      # the Rust TUI now renders identically on serial and
      # framebuffer consoles, so the runtime no longer needs a
      # boolean toggle. We omit the (formerly required) field
      # entirely — `General::_serial_console_compat` accepts it as a
      # no-op if a legacy TOML still sets it.
    }
    # Emergency-screen auto-reboot override. Emitted only when the
    # operator set it so absent configs keep the Rust-side 30 s default.
    // lib.optionalAttrs (cfg.emergencyTimeoutSecs != null) {
      emergency_timeout_secs = cfg.emergencyTimeoutSecs;
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
      # (`x-*`, `nofail`, `noauto`, `_netdev`) — the kernel rejects them
      # with EINVAL because they are not real mount(2) flags. `noauto`
      # only tells systemd not to auto-mount the entry in the target's
      # stage-1; NMBL mounts every entry in its list unconditionally
      # pre-kexec, so dropping the token here is correct (it lets a
      # filesystem be NMBL-mounted yet target-stage-1-skipped).
      options = lib.concatStringsSep "," (
        builtins.filter (
          opt:
          !(lib.hasPrefix "x-" opt) && opt != "nofail" && opt != "noauto" && opt != "_netdev"
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
      }
      # The full recovery system bakes a bash PID-1 script at /init; the
      # Rust loader execve's `entrypoint` after switch_root instead of the
      # default /bin/sh. Omitted otherwise so the flat busybox image keeps
      # the Rust-side default.
      // lib.optionalAttrs (cfg.rescue.mode == "external" && cfg.rescue.fullSystem.enable) {
        entrypoint = "/init";
      }
      # Deterministic rescue trigger. Emitted only when set so the wire
      # shape stays unchanged for the common case; the Rust serde default
      # is `false`.
      // lib.optionalAttrs cfg.rescue.forceOnBoot {
        force_on_boot = true;
      };

    # Operator-curated list of extra `/dev/<tty>` paths the picker
    # dialog offers as multiplex targets for the emergency shell.
    # Empty by default — only `/dev/console` (the kernel-elected
    # primary interactive console) is offered. The Rust serde struct
    # is `EmergencyShellConfig`.
    emergency_shell = {
      extra_consoles = cfg.emergencyShell.extraConsoles;
    };

    # `[tpm]` measured-boot table consumed by the Rust `TpmConfig`
    # struct (#7). ALWAYS emitted (FIX-09): the struct is compiled into
    # every build regardless of the `secure-boot` Cargo feature, so the
    # table is part of the base wire shape. `requireTpm` is derived in
    # tpm.nix (true when measuring / secure boot is on — FIX-28).
    # `sealed_secrets` is omitted when empty so the common-case wire
    # shape stays minimal and the Rust serde default (`[]`) applies.
    tpm = {
      measure = cfg.tpm.measure;
      pcr_index = cfg.tpm.pcrIndex;
      require_tpm = cfg.tpm.requireTpm;
      device = toString cfg.tpm.device;
    }
    // lib.optionalAttrs (cfg.tpm.sealedSecrets != [ ]) {
      sealed_secrets = map (s: {
        name = s.name;
        sealed_path = s.sealedPath;
        unseal_to = toString s.unsealTo;
      }) cfg.tpm.sealedSecrets;
    };
  }
  # Splash rendering. Emitted only when the graphical splash is enabled
  # so the validator (`deny_unknown_fields`) accepts the TOML on builds
  # that don't pull in the Rust-side `splash` config struct. The
  # initramfs landing path for the font is fixed; the background path is
  # fixed too in `initrd` mode (`/etc/splash/image.png`). In
  # `boot-partition` mode the background is NOT embedded — the Rust /init
  # reads the FIXED sidecar basename (`nmblsplash.png`) from the boot
  # partition mountpoint, so we only emit `background_location` and leave
  # `background_image` at its embedded default (unused in that mode).
  # `background_location` is omitted when it matches the Rust-side
  # default (`initrd`) so the wire shape for embedded-background builds
  # stays unchanged. Mirrors how `rescue.sfs_path` is omitted at its
  # default basename.
  // lib.optionalAttrs cfg.splash.enable {
    splash =
      {
        enable           = true;
        background_image = "/etc/splash/image.png";
        font_path        = "/etc/splash/font.ttf";
        dri_path         = "/dev/dri/card0";
      }
      // lib.optionalAttrs (cfg.splash.backgroundLocation != "initrd") {
        background_location = cfg.splash.backgroundLocation;
      };
  }
  # Stateful boot tracking. Emitted only when enabled so builds without
  # the Rust-side `stateful` feature still pass `deny_unknown_fields`.
  // lib.optionalAttrs cfg.stateful.enable {
    stateful = {
      max_recovery_attempts = cfg.stateful.maxRecoveryAttempts;
      success_target = cfg.stateful.successTarget;
    };
  }
  # Driver-image group (#8): verified out-of-tree driver squashfs blobs.
  # Emitted only when enabled so non-driver builds keep the existing wire
  # shape (the Rust `DriverImagesConfig` serde default is the empty,
  # disabled config). The per-image `firmware` packages are a BUILD-TIME
  # input baked into the squashfs — they are NOT part of the runtime struct,
  # so they are deliberately not emitted here (`deny_unknown_fields` would
  # otherwise reject them). The image table mirrors the `filesystems` /
  # `activations` array-of-tables precedent (Rust field `images`).
  // lib.optionalAttrs cfg.driverImages.enable {
    driver_images = {
      enable = true;
      images = map (img: {
        path = img.path;
        sig_path = img.sigPath;
        modules = img.modules;
        blacklist = img.blacklist;
      }) (lib.attrValues cfg.driverImages.images);
    };
  };

  rawToml = tomlFormat.generate "nmbl-config.toml" tomlValue;

  # NixOS filesystem closure as `builtins.toJSON` of a LIST of per-fs
  # objects, the exact shape the Rust `--validate-nix-filesystem-closure`
  # struct (`NixFilesystem`) deserialises: mountPoint, device, fsType,
  # options (list), neededForBoot (bool), depends (list). The full
  # `config.fileSystems` is emitted (not just the boot subset) so the Rust
  # check can confirm the NMBL toml covers every root/neededForBoot fs and
  # contains no stray entry.
  fsClosureList = map (fs: {
    mountPoint = fs.mountPoint;
    device = fs.device;
    fsType = fs.fsType;
    options = fs.options;
    neededForBoot = fs.neededForBoot;
    depends = fs.depends;
  }) (lib.attrValues config.fileSystems);
  fsClosureJson = pkgs.writeText "nmbl-fs-closure.json" (builtins.toJSON fsClosureList);

  # Newline-joined absolute paths the initramfs provides as executables.
  # The check greps this set for `paths.shell`.
  initrdExecutableList = lib.concatStringsSep "\n" initrdExecutables;
in
pkgs.runCommand "nmbl-config.toml"
  {
    nativeBuildInputs = lib.optional (rescueSfs != null) pkgs.squashfsTools;
  }
  (
    ''
    # 1. Schema validation: the Rust binary parses the TOML against the
    #    runtime structs (`serde(deny_unknown_fields)`). A schema mismatch
    #    crashes `nix build` rather than surprising the operator at boot.
    ${nmblInit}/bin/nmbl-init --validate-config=${rawToml}

    # 1b. NixOS-closure correspondence: confirm the staged config.toml
    #     MATCHES the NixOS filesystem configuration — every root /
    #     neededForBoot NixOS fs is present with the same device, fsType
    #     and mountpoint, and the toml declares no filesystem the NixOS
    #     config does not. Runs against the SAME rawToml the bootloader
    #     ships, so a mismatch fails `nix build` before any install.
    ${nmblInit}/bin/nmbl-init --validate-nix-filesystem-closure=${fsClosureJson} --config-toml=${rawToml}
  '' + lib.optionalString checkEmergencyShell ''
    # 2. Target-aware emergency-shell check. `paths.shell` (${shellPath}) is
    #    execve'd by the emergency menu's Raw Shell while NMBL is PID 1 in
    #    the INITRAMFS (before any switch_root), so it must be a binary the
    #    initramfs actually stages. The initrd here provides:
    #      ${lib.concatStringsSep ", " initrdExecutables}
    #    Fail the build with an actionable message when the configured
    #    shell is not among them — this is exactly the external-rescue
    #    misconfiguration where the initramfs ships no /bin/sh. Skipped for
    #    rescue.mode=none, where no emergency shell is shipped on purpose.
    if ! printf '%s\n' "${initrdExecutableList}" | grep -qxF '${shellPath}'; then
      echo "nmbl: boot.nmbl.paths.shell = ${shellPath} is not staged in the initramfs." >&2
      echo "nmbl: the emergency shell forks this path while NMBL is PID 1 in the" >&2
      echo "nmbl: initramfs (before switch_root), so it must exist there." >&2
      echo "nmbl: rescue.mode = ${cfg.rescue.mode}. initramfs executables:" >&2
      printf '  %s\n' "${initrdExecutableList}" >&2
      echo "nmbl: set boot.nmbl.paths.shell to one of the above, or (for" >&2
      echo "nmbl: rescue.mode=external) keep the default /bin/sh which is" >&2
      echo "nmbl: now staged from busybox." >&2
      exit 1
    fi
    echo "nmbl: emergency shell ${shellPath} is present in the initramfs."
  '' + lib.optionalString (rescueSfs != null) ''
    # 3. External rescue: inspect the ACTUAL rescue squashfs without
    #    mounting it (the nix sandbox has no /dev/fuse, so squashfuse
    #    cannot mount — `unsquashfs -l` lists the contents instead).
    #    Confirm the rescue handoff entrypoint (${rescueEntrypoint}) the
    #    `rescue::dispatch` / force-on-boot switch_root execs is really in
    #    the image, so a built external rescue is verified end-to-end.
    echo "nmbl: listing rescue squashfs ${rescueSfs}"
    unsquashfs -l ${rescueSfs} > sfs-listing.txt
    if ! grep -qxF 'squashfs-root${rescueEntrypoint}' sfs-listing.txt; then
      echo "nmbl: rescue squashfs is missing its handoff entrypoint ${rescueEntrypoint}." >&2
      echo "nmbl: rescue.mode=external, fullSystem.enable=${lib.boolToString cfg.rescue.fullSystem.enable}." >&2
      echo "nmbl: the external rescue switch_root execs this path; the .sfs must provide it." >&2
      exit 1
    fi
    echo "nmbl: rescue squashfs provides its handoff entrypoint ${rescueEntrypoint}."
  '' + ''
    cp ${rawToml} $out
  ''
  )
