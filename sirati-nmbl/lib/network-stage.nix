{
  pkgs,
  lib,
  cfg,
  moduleClosure,
}:

let
  stage = cfg.rescue.fullSystem.networkStage;
  profile = import ./network-profile.nix { inherit lib stage; };
in
if !stage.enable then
  null
else
  pkgs.runCommand "nmbl-network.erofs" { nativeBuildInputs = [ pkgs.erofs-utils ]; } ''
    set -eu
    mkdir -p root/etc/nmbl-network root/lib/firmware
    cat > root/etc/nmbl-network/network.conf <<'EOF'
    ${profile}
    EOF

    closure=${lib.escapeShellArg (toString moduleClosure)}
    if [ -d "$closure/lib/modules" ]; then
      cp -aL "$closure/lib/modules" root/lib/modules
    fi
    if [ -d "$closure/lib/firmware" ]; then
      cp -aL "$closure/lib/firmware" root/lib/firmware
    fi

    mkfs.erofs -zlz4hc "$out" root
  ''
