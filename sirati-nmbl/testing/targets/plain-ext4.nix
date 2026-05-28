# Target: plain single-disk GPT + ext4 root, no encryption, no raid.
#
# Returned shape (consumed by start-mode files):
#   {
#     id                    short-key used in app names ("plain-ext4")
#     description           one-line human description
#     diskoModule           disko config import, or null to let
#                           make-disk-image.nix synthesise the layout
#     extraInitrdKernelModules
#     extraModules          NixOS modules layered on top of the base
#     nmblKernelPackage     pkgs.X.kernel override; null = default
#     diskCount             how many disks the start mode must provide
#   }
#
# Adding a new target = one new file in this directory + one line in
# targets/default.nix.
{ pkgs, lib, ... }:
{
  id = "plain-ext4";
  description = "single GPT disk, ext4 root, no encryption";
  diskoModule = null;
  extraInitrdKernelModules = [ ];
  extraModules = [ ];
  nmblKernelPackage = null;
  diskCount = 1;
}
