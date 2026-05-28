# Interaction registry. Adding a new interaction = one new file in
# this directory + one line below.
#
# Each interaction module returns an attrset { mkRunner; ... }. The
# compose layer instantiates them per-artefact.
{
  nixpkgs,
  system ? "x86_64-linux",
}:
let
  tmux = import ./tmux.nix { inherit nixpkgs system; };
  qemu-serial-rs = import ./qemu-serial-rs.nix { inherit nixpkgs system; };
  vnc = import ./vnc.nix { inherit nixpkgs system; };
in
{
  inherit tmux qemu-serial-rs vnc;
}
