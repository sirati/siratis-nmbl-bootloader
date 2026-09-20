{
  pkgs,
  lib,
  cfg,
  moduleClosure,
}:

let
  stage = cfg.rescue.fullSystem.networkStage;
  interfaceLines = lib.concatMapStringsSep "\n" (name: "interface ${name}") stage.interfaces;
  mode = enabled: if enabled then "onlink" else "normal";
  familyLines =
    family: settings:
    lib.concatMapStringsSep "\n" (address: "address ${family} ${address}") settings.addresses
    + lib.optionalString (
      settings.gateway != null
    ) "\ngateway ${family} ${settings.gateway} ${mode settings.gatewayOnLink}"
    + lib.concatMapStringsSep "" (
      route:
      "\nroute ${family} ${route.destination} ${
        if route.via == null then "-" else route.via
      } ${mode route.onLink}"
    ) settings.routes;
  profileLines = profile: ''
    profile ${
      if profile.interfaceName != null then
        "interface ${profile.interfaceName}"
      else
        "mac ${lib.toLower profile.macAddress}"
    }
    ${familyLines "4" profile.ipv4}
    ${familyLines "6" profile.ipv6}
    end
  '';
  staticConfig = ''
    version 2
    address-family ${stage.addressFamily}
    ${lib.concatMapStringsSep "\n" (server: "dns ${server}") stage.dnsServers}
    ${lib.concatMapStringsSep "" profileLines stage.staticProfiles}
  '';
  dhcpConfig = ''
    version 1
    address-family ${stage.addressFamily}
    ${interfaceLines}
  '';
in
if !stage.enable then
  null
else
  pkgs.runCommand "nmbl-network.erofs" { nativeBuildInputs = [ pkgs.erofs-utils ]; } ''
    set -eu
    mkdir -p root/etc/nmbl-network root/lib/firmware
    cat > root/etc/nmbl-network/network.conf <<'EOF'
    ${if stage.staticProfiles == [ ] then dhcpConfig else staticConfig}
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
