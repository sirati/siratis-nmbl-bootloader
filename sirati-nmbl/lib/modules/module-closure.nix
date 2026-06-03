# Shared `makeModulesClosure` factor (FIX-36).
#
# A module closure built against NMBL's exact kernel: `/lib/modules/<kver>`
# holds the requested `.ko` (+ their dependency closure) with a depmod'd
# `modules.dep`, and `/lib/firmware` holds ONLY the blobs those modules
# reference (makeModulesClosure extracts per-module firmware requests, so even
# passing a big package like `linux-firmware` stays scoped). The running
# kernel after a switch_root / kexec is still NMBL's, so `uname -r` matches
# `<kver>` and a plain `modprobe` / `finit_module` resolves them.
#
# This is the ADDITIVE factor introduced for the NEW driver-image build (#25a):
# the rescue squashfs keeps its own inline `makeModulesClosure` call in
# lib/config.nix so its store path stays byte-identical (FIX-37). The OPTIONAL
# de-dup of the rescue closure onto this factor is #25b, gated on a store-path
# diff.
#
# `firmwareName` is EXPLICIT (FIX-36): the firmware buildEnv's `name` feeds the
# closure's input-derivation name, so two callers that share this factor but
# want distinct, non-colliding store paths (rescue vs driver-image) MUST pass
# different names. There is no implicit default that could silently make the
# driver-image firmware env collide with the rescue one.
#
# Used as a pure function:
#   import ./modules/module-closure.nix { inherit pkgs lib; }
#     { rootModules = [...]; kernel = <modulesTree>; firmware = [...];
#       firmwareName = "nmbl-driver-<name>-firmware"; }
#   -> a makeModulesClosure derivation (or null when rootModules == []).

{ pkgs, lib }:

{
  # Out-of-tree / extra module names to resolve, in no particular order
  # (makeModulesClosure pulls in each one's dependency closure). When empty
  # the factor returns `null` so callers can skip staging entirely.
  rootModules,
  # The kernel's aggregated module tree to resolve against (NMBL's exact
  # kernel — typically `pkgs.aggregateModules [ (lib.getOutput "modules"
  # kernelPackage) ]`, the same derivation the rescue closure uses).
  kernel,
  # Firmware packages whose `/lib/firmware` blobs the modules may request.
  # A plain `listOf package`; joined here into the SINGLE store path
  # makeModulesClosure expects (it `cd`s into `$firmware/lib/firmware/<blob>`).
  firmware ? [ ],
  # EXPLICIT firmware-env name (FIX-36). REQUIRED — no default — so a caller
  # can never accidentally collide its firmware env (and thus its closure)
  # with another caller's.
  firmwareName,
  # Keep the build green when a named module turns out to be built into the
  # kernel (so it has no `.ko`). Matches the rescue closure's posture.
  allowMissing ? true,
}:

let
  # makeModulesClosure wants `firmware` as a SINGLE store path it can `cd`
  # into, not a list. Join the configured packages into one buildEnv exposing
  # `/lib/firmware` — exactly as NixOS's `hardware.firmware` does internally.
  # An empty list still yields a valid (empty) `/lib/firmware`, so the closure
  # builds cleanly when no firmware is configured.
  firmwareEnv = pkgs.buildEnv {
    name = firmwareName;
    paths = firmware;
    pathsToLink = [ "/lib/firmware" ];
    ignoreCollisions = true;
  };
in
if rootModules == [ ] then
  null
else
  pkgs.makeModulesClosure {
    inherit kernel allowMissing;
    rootModules = lib.unique rootModules;
    firmware = firmwareEnv;
  }
