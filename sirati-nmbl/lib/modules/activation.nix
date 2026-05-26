# NMBL activation hook module
#
# Inspects config.fileSystems to work out which userspace activation steps
# (LVM, mdraid, ZFS) the initramfs needs to perform before mounting the root
# filesystem, and exports the result as four attrs consumed by lib/config.nix
# / lib/config-toml.nix:
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

  # --- pkgsStatic.* with graceful fallback ----------------------------------

  tryStatic = attr:
    let r = builtins.tryEval (pkgs.pkgsStatic.${attr} or null);
        s = if r.success then r.value else null;
        d = pkgs.${attr} or null;
    in if s != null then { pkg = s; isStatic = true; }
       else { pkg = d; isStatic = false; };

  lvm2 = tryStatic "lvm2";
  mdadm = tryStatic "mdadm";
  zfs = tryStatic "zfs";

  # --- requested-activation booleans ----------------------------------------

  lvmOn = act.lvm.enable;
  mdOn = act.mdraid.enable;
  zfsOn = act.zfs.pools != [ ];

  # --- extraKernelModules ---------------------------------------------------

  extraKernelModules = lib.unique (
    lib.optional lvmOn "dm_mod"
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
    ++ lib.optionals (zfsOn && zfs.pkg != null) (map (sbin zfs.pkg) [ "zpool" "zfs" ]);

  # --- activationBlocks ([[activation]] TOML rows) --------------------------
  # Order: mdraid (lowest level) -> LVM -> ZFS.

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
    failMissing lvmOn lvm2 "lvm2"
    ++ failMissing mdOn mdadm "mdadm"
    ++ failMissing zfsOn zfs "zfs"
    ++ warnFallback lvmOn lvm2 "lvm2"
    ++ warnFallback mdOn mdadm "mdadm"
    ++ warnFallback zfsOn zfs "zfs";

  mkComputed = type: desc: lib.mkOption {
    inherit type;
    internal = true;
    readOnly = true;
    description = lib.mdDoc desc;
  };

in
{
  options.boot.nmbl.activation = {

    lvm.enable = lib.mkOption {
      type = lib.types.bool;
      default = hasMapper;
      defaultText = lib.literalMD "true if any `config.fileSystems.*.device` is under `/dev/mapper/`; false otherwise.";
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
