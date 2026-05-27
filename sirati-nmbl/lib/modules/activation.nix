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

  extraKernelModules = lib.unique (
    # encrypted_keys.ko's init calls alloc_cipher("ecb(aes)") which needs
    # both an AES provider (aesni_intel) AND the ecb cipher mode module
    # registered with the kernel crypto API. Load them here so they're
    # live by the time the base list pulls in dm-crypt (whose dep walk
    # pulls in encrypted_keys). xts is the LUKS2 data-encryption mode.
    lib.optionals luksAny [ "dm_mod" "aesni_intel" "ecb" "xts" "dm-crypt" ]
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

  luksBaseMods = [ "dm_mod" "dm-crypt" "aesni_intel" "xts" ];
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
    };

  # --- option types ---------------------------------------------------------

  luksSubmodule = lib.types.submodule {
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
    };
  };

  mkComputed = type: desc: lib.mkOption {
    inherit type;
    internal = true;
    readOnly = true;
    description = lib.mdDoc desc;
  };

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
  };
}
