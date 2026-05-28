# Target registry. Adding a new target = one new file in this
# directory + one line below.
#
# Each target is a function `{ pkgs, lib, ... }: { id; description;
# diskoModule; extraInitrdKernelModules; extraModules;
# nmblKernelPackage; diskCount; }`. The registry instantiates them
# lazily so they can refer to pkgs without forcing nixpkgs evaluation
# for unused targets.
{ pkgs, lib }:
let
  callTarget = path: import path { inherit pkgs lib; };
in
{
  plain-ext4 = callTarget ./plain-ext4.nix;
  luks-password = callTarget ./luks-password.nix;
  luks-tpm = callTarget ./luks-tpm.nix;
  mdraid = callTarget ./mdraid.nix;
  btrfs = callTarget ./btrfs.nix;
  btrfs-raid1 = callTarget ./btrfs-raid1.nix;
}
