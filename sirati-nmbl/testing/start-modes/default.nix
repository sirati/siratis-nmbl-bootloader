# Start-mode registry. Adding a new start mode = one new file in
# this directory + one line below.
#
# Each start-mode module returns an attrset with at least `mkArtefact`;
# some modes (nixos-anywhere-install) also expose helpers.
{
  self,
  nixpkgs,
  disko,
  nixos-anywhere ? null,
  system ? "x86_64-linux",
}:
let
  kvm-kexec = import ./kvm-kexec.nix { inherit self nixpkgs disko system; };
  nix-build-vm = import ./nix-build-vm.nix { inherit self nixpkgs disko system; };
  kvm-kexec-installed = import ./kvm-kexec-installed.nix { inherit self nixpkgs disko system; };
  nixos-anywhere-install =
    if nixos-anywhere != null then
      import ./nixos-anywhere-install.nix {
        inherit self nixpkgs disko nixos-anywhere system;
      }
    else
      null;
in
{
  inherit
    kvm-kexec
    nix-build-vm
    kvm-kexec-installed
    nixos-anywhere-install
    ;
}
