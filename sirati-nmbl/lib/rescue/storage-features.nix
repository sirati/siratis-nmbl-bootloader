# Which storage stacks this host actually uses, so the full-system rescue
# only carries the tools (and kernel modules) it can need. Shared by the
# rescue package defaults (lib/options.nix) and its module list
# (lib/config.nix). Mirrors the signals NMBL's own activation uses
# (lib/modules/activation.nix) plus the NixOS-side declarations.
{ lib, config }:

let
  fileSystems = lib.attrValues config.fileSystems;
  activation = config.boot.nmbl.activation;
  supported = config.boot.supportedFilesystems or { };
  supports = fs:
    if builtins.isList supported then lib.elem fs supported else supported.${fs} or false;
  identity = config.boot.nmbl.rescue.fullSystem.identityVolume;
in
{
  # LUKS: NMBL unlocks a mapping itself, or NixOS declares one.
  luks = activation.luks != [ ] || (config.boot.initrd.luks.devices or { }) != { };
  # NixOS enables its initrd LVM service by default, so only NMBL's own
  # activation (auto-detected from device-mapper filesystems) counts.
  lvm = activation.lvm.enable;
  mdraid = activation.mdraid.enable || (config.boot.swraid.enable or false);
  btrfs = lib.any (fs: (fs.fsType or "") == "btrfs") fileSystems
    || supports "btrfs"
    || (identity != null && identity.fsType == "btrfs");
  nvme = lib.elem "nvme" (
    config.boot.initrd.availableKernelModules
    ++ config.boot.initrd.kernelModules
    ++ config.boot.nmbl.bootstrap.kernelModules.explicit
  );
}
