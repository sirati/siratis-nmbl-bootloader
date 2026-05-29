# Compose layer for the three-axis test matrix.
#
# Generates the cross product
#     start-mode × target × interaction
# subject to a small set of constraints (filtered out — see
# `invalidCombo` below). For each surviving triple we instantiate
# the artefact (start-mode side) and the runner (interaction side)
# and expose them as flake apps keyed by `<start>-<target>-<inter>`.
#
# The compose layer also returns a list of `aliases` so the caller's
# flake can keep legacy app names like `tmux-serial-test-...` and
# `install-test-gpt-...` alive as re-exports of the new names.
{
  self,
  nixpkgs,
  disko,
  nixos-anywhere ? null,
  vmSerialMan,
  system ? "x86_64-linux",
}:
let
  pkgs = nixpkgs.legacyPackages.${system};
  lib = nixpkgs.lib;

  targets = import ./targets/default.nix { inherit pkgs lib; };
  bootstrappers = import ./bootstrappers.nix { };
  startModes = import ./start-modes/default.nix {
    inherit
      self
      nixpkgs
      disko
      nixos-anywhere
      system
      ;
  };
  interactions = import ./interactions/default.nix { inherit nixpkgs system; };

  # Canonical bootstrapper per (start-mode, target) pair. Each target
  # has one "default" bootstrapper used in the short app name; the
  # full matrix can be exposed via aliases below for legacy callers.
  defaultBootstrapperFor = startMode: target:
    if startMode == "kvm-kexec" && target == "plain-ext4" then "qemu-kernel-invoke"
    else "gpt-uefi-grub";

  # Filter: which (start-mode, target, interaction) triples are invalid?
  #
  # `nixos-anywhere-install` is excluded from the generic cross-product
  # because its renderer is the install ORCHESTRATOR (set up rescue VM
  # + run nixos-anywhere + boot stage 3), not a plain QEMU launch. The
  # existing orchestrators in nixos-anywhere-test/flake.nix already
  # cover plain-ext4 + mdraid + btrfs-raid1 with bios/uefi-grub/uefi-systemd
  # bootstrappers; the caller's flake aliases those into the
  # `nixos-anywhere-install-<target>-screen` namespace below.
  # For `nixos-anywhere-install`, the renderer is the install
  # orchestrator (kept in nixos-anywhere-test/flake.nix); the caller's
  # flake aliases each existing orchestrator app into the matrix
  # namespace as `nixos-anywhere-install-<target>-{screen,qemu-serial-rs}`.
  # Long-term we could refactor that flake to emit artefacts here, but
  # for now the alias path keeps the install behaviour unchanged.
  # The `luks-password-splash` target renders the GRAPHICAL splash to a
  # DRM framebuffer, so it only makes sense paired with the `vnc`
  # interaction (a serial/tmux UART can't show what NMBL draws on
  # /dev/dri/card0). Constrain it to vnc; leave every other target's
  # interaction set untouched.
  invalidCombo = startMode: target: interaction:
    startMode == "nixos-anywhere-install"
    || (target == "luks-password-splash" && interaction != "vnc");

  # Build artefact for a (start-mode, target, bootstrapper) triple.
  mkArtefactFor =
    startModeName: target: bootstrapperName:
    let
      sm = startModes.${startModeName};
      bs = bootstrappers.${bootstrapperName};
      configName = "${target.id}-${bootstrapperName}";
    in
    if sm == null then
      null
    else
      sm.mkArtefact {
        inherit target;
        bootstrapper = bs;
        configName = target.id; # short — bootstrapper is implied default
      };

  # Build a runner from an artefact + interaction name.
  mkRunner = interactionName: artefact:
    let
      ix = interactions.${interactionName};
    in
    if interactionName == "qemu-serial-rs" then
      ix.mkRunner { inherit artefact vmSerialMan; }
    else
      ix.mkRunner { inherit artefact; };

  # The 3-axis cross product. Each entry has:
  #   { app-name → { type = "app"; program = "..."; } }
  crossProduct =
    let
      startModeNames = builtins.filter (n: startModes.${n} != null) (
        builtins.attrNames startModes
      );
      targetNames = builtins.attrNames targets;
      interactionNames = builtins.attrNames interactions;
      buildEntry =
        startModeName: targetName: interactionName:
        let
          target = targets.${targetName};
          bsName = defaultBootstrapperFor startModeName targetName;
          artefact = mkArtefactFor startModeName target bsName;
          runner =
            if artefact != null then mkRunner interactionName artefact else null;
          appName = "${startModeName}-${targetName}-${interactionName}";
          # Runner derivation's main binary is named after the
          # interaction file (tmux/{vnc,qemu-serial-rs}-${artefact.name}).
          binName =
            if interactionName == "tmux" then "tmux-${artefact.name}"
            else if interactionName == "vnc" then "vnc-${artefact.name}"
            else "qemu-serial-rs-${artefact.name}";
        in
        if runner == null || (invalidCombo startModeName targetName interactionName) then
          null
        else
          {
            name = appName;
            value = {
              type = "app";
              program = "${runner}/bin/${binName}";
            };
          };
    in
    lib.filter (e: e != null) (
      lib.concatLists (
        map (
          smn:
          lib.concatLists (
            map (
              tn: map (i: buildEntry smn tn i) interactionNames
            ) targetNames
          )
        ) startModeNames
      )
    );

  apps = builtins.listToAttrs crossProduct;

  # Public surface
in
{
  inherit
    targets
    bootstrappers
    startModes
    interactions
    apps
    ;
  inherit mkArtefactFor mkRunner;

  # Convenience: full target list and start-mode list as strings (for
  # debug/introspection).
  axisNames = {
    startModes = builtins.attrNames startModes;
    targets = builtins.attrNames targets;
    interactions = builtins.attrNames interactions;
  };
}
