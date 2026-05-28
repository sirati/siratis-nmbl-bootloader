# Backward-compat alias for the renamed artefact.nix.
#
# External flakes (nixos-anywhere-test, rescue-vm-test) and the
# legacy import sites in this flake used to do
#   import ./test-artefact.nix
# This file just re-exports the same value type from the new
# canonical location at ./artefact.nix so existing import lines keep
# working.
args: import ./artefact.nix args
