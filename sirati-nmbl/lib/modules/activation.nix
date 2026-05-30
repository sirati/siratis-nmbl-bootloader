# NMBL activation hook module
#
# Inspects config.fileSystems plus explicit boot.nmbl.activation.* options to
# work out which userspace activation steps (LVM, mdraid, LUKS, ZFS) the
# initramfs needs to perform before mounting the root filesystem, and exports
# the result as four attrs consumed by lib/config.nix / lib/config-toml.nix:
#
#   cfg.activation.activationBlocks   - [[activation]] TOML schema rows
#   cfg.activation.extraKernelModules - module names to add to allKernelModules
#   cfg.activation.extraContents      - makeInitrd content entries
#   cfg.activation.assertions         - NixOS assertions

{ config, lib, pkgs, ... }:

let
  act = config.boot.nmbl.activation;
  fileSystems = builtins.attrValues config.fileSystems;

  # --- heuristic detection from config.fileSystems --------------------------

  anyFs = pred: lib.any pred fileSystems;
  hasMapper = anyFs (fs: fs.device != null && lib.hasPrefix "/dev/mapper/" fs.device);
  hasMd = anyFs (fs: fs.device != null && lib.hasPrefix "/dev/md" fs.device);
  hasZfs = anyFs (fs: (fs.fsType or "") == "zfs");

  # /dev/mapper/* is ambiguous (LVM vs LUKS); if any explicit luks entry exists
  # we assume the user covered it there, otherwise default to LVM activation.
  hasExplicitLuks = act.luks != [ ];
  lvmAutoDetected = hasMapper && !hasExplicitLuks;

  # LUKS-backed filesystems NMBL must mount but won't unlock.
  # /dev/mapper/* alone is ambiguous (LVM vs LUKS), so we key off
  # config.boot.initrd.luks.devices — the canonical NixOS LUKS
  # declaration. NMBL replaces stage-1, so any LUKS mapping backing a
  # filesystem NMBL mounts MUST also be declared in
  # boot.nmbl.activation.luks or NMBL can never open it.
  # NOTE: stacking limitation — LUKS-under-LVM (fs device =
  # /dev/mapper/<vg>-<lv>, whose name differs from the luks name) is
  # NOT caught by this direct-device check; acceptable for now.
  nixosLuksNames = lib.attrNames (config.boot.initrd.luks.devices or { });
  nmblLuksNames = map (l: l.name) act.luks;
  nmblFsDevices = map (fs: fs.device)
    (lib.filter (fs: fs.device != null) fileSystems);
  uncoveredLuksFs = lib.filter
    (n: (lib.elem "/dev/mapper/${n}" nmblFsDevices)
        && !(lib.elem n nmblLuksNames))
    nixosLuksNames;

  # Stacked case: LVM-on-LUKS. The root fs device is then an LVM LV
  # (/dev/mapper/<vg>-<lv>) whose name differs from the LUKS name, so the
  # direct check above never fires — yet NMBL still can't assemble the VG
  # because the LUKS PV is never unlocked. Conservatively flag when there
  # is BOTH an uncovered LUKS device AND a device-mapper fs that isn't
  # itself a declared LUKS mapping (i.e. an LVM LV / other dm target that
  # could sit on the unopened PV).
  uncoveredLuks = lib.filter (n: !(lib.elem n nmblLuksNames)) nixosLuksNames;
  mapperFsNames = map (d: lib.removePrefix "/dev/mapper/" d)
    (lib.filter (d: lib.hasPrefix "/dev/mapper/" d) nmblFsDevices);
  lvmLikeMapperFs = lib.filter (n: !(lib.elem n nixosLuksNames)) mapperFsNames;

  # --- pkgsStatic.* with graceful fallback ----------------------------------

  tryStatic = attr:
    let r = builtins.tryEval (pkgs.pkgsStatic.${attr} or null);
        s = if r.success then r.value else null;
        d = pkgs.${attr} or null;
    in if s != null then { pkg = s; isStatic = true; }
       else { pkg = d; isStatic = false; };

  lvm2 = tryStatic "lvm2";
  mdadm = tryStatic "mdadm";
  cryptsetup = tryStatic "cryptsetup";
  zfs = tryStatic "zfs";

  # --- requested-activation booleans ----------------------------------------

  luksAny = act.luks != [ ];
  luksTpm = lib.any (l: l.unlock == "tpm") act.luks;
  lvmOn = act.lvm.enable;
  mdOn = act.mdraid.enable;
  zfsOn = act.zfs.pools != [ ];

  # --- extraKernelModules ---------------------------------------------------

  # encrypted_keys.ko's init calls alloc_cipher("ecb(aes)") which needs
  # both an AES provider (aesni_intel) AND the ecb cipher mode module
  # registered with the kernel crypto API. Load them here so they're
  # live by the time the base list pulls in dm-crypt (whose dep walk
  # pulls in encrypted_keys). xts is the LUKS2 data-encryption mode.
  extraKernelModules = lib.unique (
    lib.optionals luksAny [ "dm_mod" "aesni_intel" "ecb" "xts" "sha256_generic" "dm-crypt" ]
    ++ lib.optionals luksTpm [ "tpm_crb" "tpm_tis" ]
    ++ lib.optional lvmOn "dm_mod"
    ++ lib.optionals mdOn [ "md_mod" "raid0" "raid1" "raid10" "raid456" ]
    ++ lib.optional zfsOn "zfs"
  );

  # --- extraContents (makeInitrd entries) -----------------------------------

  link = object: symlink: { inherit object symlink; };
  bin = pkg: name: link "${pkg}/bin/${name}" "/bin/${name}";
  sbin = pkg: name: link "${pkg}/sbin/${name}" "/bin/${name}";

  extraContents =
    lib.optionals (lvmOn && lvm2.pkg != null) (map (bin lvm2.pkg) [ "vgchange" "vgs" "lvchange" ])
    ++ lib.optionals (mdOn && mdadm.pkg != null) [ (sbin mdadm.pkg "mdadm") ]
    ++ lib.optionals (luksAny && cryptsetup.pkg != null) [ (bin cryptsetup.pkg "cryptsetup") ]
    ++ lib.optionals (zfsOn && zfs.pkg != null) (map (sbin zfs.pkg) [ "zpool" "zfs" ]);

  # --- activationBlocks ([[activation]] TOML rows) --------------------------

  luksBaseMods = [ "dm_mod" "dm-crypt" "aesni_intel" "xts" "sha256_generic" ];
  luksTpmMods = luksBaseMods ++ [ "tpm_crb" "tpm_tis" ];

  mkLuksBlock = l:
    let mapper = "/dev/mapper/${l.name}"; in
    if l.unlock == "tpm" then {
      kind = "luks-tpm"; required_modules = luksTpmMods;
      binary = "/bin/cryptsetup"; argv = [ "open" "--token-only" l.device l.name ];
      produces_devices = [ mapper ]; description = "Unlock ${l.name} via TPM-sealed token";
    } else if l.unlock == "keyfile" then {
      kind = "luks-keyfile"; required_modules = luksBaseMods;
      binary = "/bin/cryptsetup";
      argv = [ "open" l.device l.name "--key-file=${toString l.keyfile}" ];
      produces_devices = [ mapper ]; description = "Unlock ${l.name} via keyfile";
    } else {
      kind = "luks-password"; required_modules = luksBaseMods;
      binary = "/bin/cryptsetup"; argv = [ "open" l.device l.name "--key-file=-" ];
      produces_devices = [ mapper ]; description = "Unlock ${l.name} via passphrase";
      prompt_label = l.promptLabel;
      pass_to_stage1 = l.passToStage1;
    };

  # Order: mdraid (lowest level) -> LVM -> LUKS -> ZFS.
  activationBlocks =
    lib.optional mdOn {
      kind = "mdraid"; required_modules = [ "md_mod" ];
      binary = "/bin/mdadm"; argv = [ "--assemble" "--scan" ];
      produces_devices = [ ]; description = "Assemble mdraid arrays";
    }
    ++ lib.optional lvmOn {
      kind = "lvm"; required_modules = [ "dm_mod" ];
      binary = "/bin/vgchange"; argv = [ "-ay" ];
      produces_devices = [ ]; description = "Activate LVM volume groups";
    }
    ++ map mkLuksBlock act.luks
    ++ map (pool: {
      kind = "zfs"; required_modules = [ "zfs" ];
      binary = "/bin/zpool"; argv = [ "import" "-N" pool ];
      produces_devices = [ ]; description = "Import zpool '${pool}'";
    }) act.zfs.pools;

  # --- assertions -----------------------------------------------------------

  warnFallback = need: ts: tool: lib.optional (need && ts.pkg != null && !ts.isStatic) {
    assertion = true;
    message = "boot.nmbl.activation: pkgsStatic.${tool} unavailable; falling back to dynamic pkgs.${tool} (initramfs closure will be larger)";
  };
  failMissing = need: ts: tool: lib.optional (need && ts.pkg == null) {
    assertion = false;
    message = "boot.nmbl.activation requires '${tool}' but neither pkgs.pkgsStatic.${tool} nor pkgs.${tool} is available";
  };

  computedAssertions =
    # keyfile path existence (best effort at eval time)
    map (l: {
      assertion = builtins.pathExists l.keyfile;
      message = "boot.nmbl.activation.luks entry '${l.name}' uses keyfile unlock but keyfile path ${toString l.keyfile} does not exist at build time";
    }) (lib.filter (l: l.unlock == "keyfile" && l.keyfile != null) act.luks)
    # missing-package failures
    ++ failMissing lvmOn lvm2 "lvm2"
    ++ failMissing mdOn mdadm "mdadm"
    ++ failMissing luksAny cryptsetup "cryptsetup"
    ++ failMissing zfsOn zfs "zfs"
    # non-static fallback warnings (assertion = true so they only show as hints)
    ++ warnFallback lvmOn lvm2 "lvm2"
    ++ warnFallback mdOn mdadm "mdadm"
    ++ warnFallback luksAny cryptsetup "cryptsetup"
    ++ warnFallback zfsOn zfs "zfs"
    # TPM unlock requires TPM modules
    ++ lib.optional luksTpm {
      assertion = lib.any (m: lib.elem m extraKernelModules) [ "tpm_crb" "tpm_tis" ];
      message = "boot.nmbl.activation.luks entry requests TPM unlock but no TPM kernel modules were added to extraKernelModules";
    }
    # LUKS filesystems NMBL mounts but isn't told to unlock
    ++ map (n: {
      assertion = false;
      message = ''
        NMBL: filesystem on /dev/mapper/${n} is a LUKS device declared in
        boot.initrd.luks.devices, but ${n} is not covered by
        boot.nmbl.activation.luks. NMBL replaces NixOS stage-1 and will not
        unlock it, so the filesystem cannot be mounted at boot.

        Declare it for NMBL, e.g.:

          boot.nmbl.activation.luks = [{
            name = "${n}";
            device = "<backing block device, e.g. /dev/nvme0n1p2>";
            unlock = "password";  # or "tpm" / "keyfile"
          }];
      '';
    }) uncoveredLuksFs
    # Stacked LVM-on-LUKS: a dm filesystem may sit on an unopened LUKS PV.
    ++ lib.optional (uncoveredLuks != [ ] && lvmLikeMapperFs != [ ]) {
      assertion = false;
      message = ''
        NMBL: filesystem(s) on device-mapper volume(s) [${lib.concatStringsSep ", " lvmLikeMapperFs}] may be layered on LUKS device(s) [${lib.concatStringsSep ", " uncoveredLuks}] that are declared in boot.initrd.luks.devices but not covered by boot.nmbl.activation.luks. NMBL replaces NixOS stage-1; if these volumes sit on those LUKS devices, NMBL cannot unlock the PV, the volume group never assembles, and boot hangs. Declare each LUKS device in boot.nmbl.activation.luks (or, if a listed mapper volume is genuinely not on an encrypted PV, this is a false positive — report it).
      '';
    };

  # --- option types ---------------------------------------------------------

  luksSubmodule = lib.types.submodule ({ config, ... }: {
    options = {
      name = lib.mkOption {
        type = lib.types.str;
        description = lib.mdDoc "Name of the resulting mapping under /dev/mapper.";
      };
      device = lib.mkOption {
        type = lib.types.str;
        description = lib.mdDoc "Backing block device path (e.g. /dev/nvme0n1p3).";
      };
      unlock = lib.mkOption {
        type = lib.types.enum [ "tpm" "keyfile" "password" ];
        description = lib.mdDoc ''
          Unlock strategy:
          - tpm: cryptsetup --token-only (TPM2 sealed token in header)
          - keyfile: cryptsetup --key-file <path> (keyfile bundled in initramfs)
          - password: passphrase entered interactively in the TUI
        '';
      };
      keyfile = lib.mkOption {
        type = lib.types.nullOr lib.types.path;
        default = null;
        description = lib.mdDoc "Keyfile path to bundle in the initramfs. Required when unlock = \"keyfile\".";
      };
      tpmPcrs = lib.mkOption {
        type = lib.types.listOf lib.types.int;
        default = [ ];
        description = lib.mdDoc "PCR registers expected to be sealed against (informational). Only meaningful when unlock = \"tpm\".";
      };
      promptLabel = lib.mkOption {
        type = lib.types.str;
        default = "Enter passphrase";
        description = lib.mdDoc "Label shown in the TUI password modal. Only meaningful when unlock = \"password\".";
      };
      passToStage1 = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        # Per-instance default is computed in the submodule's `config`
        # block below via `lib.mkOptionDefault`, so it can depend on
        # this entry's own `unlock` and `name`. We deliberately do NOT
        # set `default = null` here: a top-level `default` and a
        # `config.passToStage1 = mkOptionDefault ...` collide in the
        # module merger when both are present.
        defaultText = lib.literalMD ''
          `"/etc/nmbl-luks/''${name}"` when `unlock = "password"` (and
          any future passphrase-style unlock such as `"tpm+pin"`),
          `null` otherwise. Operators opt out by explicitly setting
          `passToStage1 = null`, which also suppresses the
          auto-wired `boot.initrd.luks.devices.<name>.keyFile`.
        '';
        example = "/etc/nmbl-luks/cryptroot";
        description = lib.mdDoc ''
          When non-null, NMBL captures the typed passphrase after a
          successful unlock and injects it into the kexec'd initrd at
          this path as a cryptsetup-compatible keyfile (memory only,
          never written to disk). NMBL also auto-sets
          `boot.initrd.luks.devices.<name>.keyFile` and
          `fallbackToPassword = true` (both via `lib.mkDefault`) so
          the post-kexec NixOS stage-1 picks up the injected secret
          and the operator types the passphrase exactly once. Only
          meaningful for passphrase-style unlocks (currently
          `unlock = "password"`; future `"tpm+pin"` will behave the
          same).
        '';
      };
    };
    # Per-entry default. `mkOptionDefault` sits below the priority of
    # any explicit operator value, so writing `passToStage1 = null`
    # opts out cleanly. For non-password unlocks we default to `null`:
    # there's no operator-typed secret to hand through.
    # NOTE: when a future `unlock = "tpm+pin"` variant lands, add it
    # to the passphrase-style branch so it gets the same auto-wiring.
    config.passToStage1 = lib.mkOptionDefault (
      if config.unlock == "password" then "/etc/nmbl-luks/${config.name}" else null
    );
  });

  mkComputed = type: desc: lib.mkOption {
    inherit type;
    internal = true;
    readOnly = true;
    description = lib.mdDoc desc;
  };

  # ---- post-stage-0 keyfile wipe -----------------------------------------
  # For every passToStage1 entry we emit a stage-1 cleanup that overwrites
  # the injected keyfile bytes in tmpfs before unlinking, then removes the
  # parent directory. Overwriting before free is what actually scrubs the
  # bytes: tmpfs pages live in RAM, and the kernel does not zero freed
  # pages — only what we write in place is guaranteed-gone post-unlink.
  #
  # `injectedLuks` is the set of entries that opted in to the stage-0 ->
  # stage-1 passphrase hand-off. Default-on for `unlock = "password"`; an
  # explicit `passToStage1 = null` opts out (and suppresses the auto-wired
  # `boot.initrd.luks.devices.<name>` below). Future `unlock = "tpm+pin"`
  # should land here too.
  injectedLuks =
    lib.filter (l: l.unlock == "password" && l.passToStage1 != null) act.luks;
  injectedPaths = map (l: l.passToStage1) injectedLuks;

  wipeSnippet = path: ''
    if [ -e ${path} ]; then
      size=$(stat -c%s ${path} 2>/dev/null || echo 0)
      if [ "$size" -gt 0 ]; then
        dd if=/dev/zero of=${path} bs=1 count="$size" conv=notrunc 2>/dev/null || true
      fi
      rm -f ${path}
      rmdir "$(dirname ${path})" 2>/dev/null || true
    fi
  '';

  wipeShellScript = lib.concatMapStrings wipeSnippet injectedPaths;

in
{
  options.boot.nmbl.activation = {

    luks = lib.mkOption {
      type = lib.types.listOf luksSubmodule;
      default = [ ];
      description = lib.mdDoc ''
        Explicit LUKS volumes to unlock during NMBL boot. Each entry produces
        one [[activation]] block of kind luks-tpm / luks-keyfile / luks-password.
      '';
    };

    lvm.enable = lib.mkOption {
      type = lib.types.bool;
      default = lvmAutoDetected;
      defaultText = lib.literalMD ''
        true if any `config.fileSystems.*.device` is under `/dev/mapper/` and
        no explicit `boot.nmbl.activation.luks` entry covers it; false otherwise.
      '';
      description = lib.mdDoc "Run `vgchange -ay` in the initramfs to activate LVM volume groups before mounting.";
    };

    mdraid.enable = lib.mkOption {
      type = lib.types.bool;
      default = hasMd;
      defaultText = lib.literalMD "true if any `config.fileSystems.*.device` is under `/dev/md*`; false otherwise.";
      description = lib.mdDoc "Run `mdadm --assemble --scan` in the initramfs to assemble software RAID arrays before mounting.";
    };

    zfs.pools = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = lib.optionals hasZfs [ "rpool" ];
      defaultText = lib.literalMD ''`[ "rpool" ]` if any filesystem has `fsType = "zfs"`, else `[ ]`.'';
      example = [ "rpool" "tank" ];
      description = lib.mdDoc "Names of ZFS pools to import (`zpool import -N <name>`) before mounting the root filesystem.";
    };

    # Computed outputs consumed by lib/config-toml.nix and lib/config.nix.
    activationBlocks = mkComputed (lib.types.listOf lib.types.attrs)
      "Computed list of `[[activation]]` blocks for the runtime TOML config.";
    extraKernelModules = mkComputed (lib.types.listOf lib.types.str)
      "Computed kernel module names that the activations require; merged into the bootloader initramfs module set.";
    extraContents = mkComputed (lib.types.listOf lib.types.attrs)
      "Computed `pkgs.makeInitrd` content entries for the activation binaries.";
    assertions = mkComputed (lib.types.listOf lib.types.attrs)
      "Computed NixOS assertions about the activation configuration.";
  };

  config = {
    boot.nmbl.activation = {
      inherit activationBlocks extraKernelModules extraContents;
      assertions = computedAssertions;
    };
    # Surface activation assertions through the standard NixOS mechanism so
    # they fail nixos-rebuild and our nmblAssertionCheck derivation alike.
    assertions = computedAssertions;

    # Auto-wire the post-kexec NixOS stage-1 to consume NMBL's injected
    # passphrase. `mkDefault` lets operators override either value
    # explicitly without conflict. Setting `passToStage1 = null` on the
    # activation entry skips it from `injectedLuks`, so no auto-wiring
    # is emitted for that device.
    #
    # `fallbackToPassword` is only meaningful under scripted stage 1
    # (systemd stage 1 implies it and rejects an explicit setting via
    # an assertion in nixpkgs' luksroot.nix), so we only set it when
    # systemd stage 1 is disabled.
    boot.initrd.luks.devices = lib.mkMerge (map (l: {
      ${l.name} = {
        keyFile = lib.mkDefault l.passToStage1;
      } // lib.optionalAttrs (!config.boot.initrd.systemd.enable) {
        fallbackToPassword = lib.mkDefault true;
      };
    }) injectedLuks);

    # Scripted stage-1 cleanup: runs after LUKS unlock, before pivot.
    # systemd stage 1 rejects any non-empty `postDeviceCommands` via a
    # fatal assertion in nixpkgs' stage-1 module, so gate this on the
    # scripted-stage-1 path only.
    boot.initrd.postDeviceCommands = lib.mkIf
      (injectedPaths != [ ] && !config.boot.initrd.systemd.enable)
      wipeShellScript;

    # systemd-stage-1 cleanup: a oneshot that runs after cryptsetup
    # targets have completed but before initrd-switch-root. The unit
    # also Requires=cryptsetup.target so it definitely fires after the
    # injected keyfile has done its job.
    boot.initrd.systemd.services = lib.mkIf (injectedPaths != [ ]) {
      nmbl-wipe-injected-keys = {
        description = "Wipe NMBL-injected LUKS keyfiles from initrd tmpfs";
        wantedBy = [ "initrd.target" ];
        after = [ "cryptsetup.target" ];
        before = [ "initrd-switch-root.target" "sysroot.mount" ];
        unitConfig.DefaultDependencies = false;
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
        };
        script = wipeShellScript;
      };
    };
  };
}
