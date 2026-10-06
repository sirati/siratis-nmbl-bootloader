{
  source,
  bakedStatic ? false,
  bakedSlaac ? false,
  nativeIdentity ? false,
  publicKeyPath,
  publicKeyHash,
  sshPublicKeyPath,
  sshPublicKeyHash,
}:

let
  flake = builtins.getFlake "path:${source}";
  publicKey = builtins.path {
    path = publicKeyPath;
    name = "nmbl-network-stage-vm-public.key";
    sha256 = publicKeyHash;
  };
  sshPublicKey = builtins.readFile (builtins.path {
    path = sshPublicKeyPath;
    name = "nmbl-network-stage-vm-ssh-public.key";
    sha256 = sshPublicKeyHash;
  });
  # The console-inheritance fixture replaces the rescue console launcher
  # through the guarded test hook, so the rescue image, its signature and
  # the digest NMBL's config pins are all built by the production path.
  consoleFixture = import ./console-fixture/default.nix { inherit pkgs; };
  baseline = (flake.lib.mkNetworkStageVmConfig { inherit publicKey sshPublicKey; }).extendModules {
    modules = [ { boot.nmbl.rescue.fullSystem.console = consoleFixture; } ];
  };
  config = if !(bakedStatic || bakedSlaac || nativeIdentity) then baseline else baseline.extendModules {
    modules = [ {
      boot.nmbl.signing.enable = lib.mkForce false;
      boot.nmbl.signing.enforce = lib.mkForce false;
      boot.nmbl.rescue.fullSystem.networkStage = {
        enable = lib.mkForce false;
        addressFamily = lib.mkForce (if bakedSlaac then "ipv6-only" else "dual-stack");
        dnsServers = lib.mkForce [ "1.1.1.1" ];
        staticProfiles = lib.mkForce (if bakedSlaac then [ ] else [ {
          interfaceName = "eth0";
          ipv4 = { addresses = [ "88.99.80.66/32" ]; gateway = "172.31.1.1"; gatewayOnLink = true; };
          ipv6 = { addresses = [ "2a01:4f8:1c17:5100::1/64" ]; gateway = "fe80::1"; gatewayOnLink = true; };
        } ]);
      };
    } ] ++ lib.optional nativeIdentity {
      boot.nmbl.bootstrap.kernelModules.explicit = lib.mkAfter [ "btrfs" ];
      boot.nmbl.rescue.fullSystem.identityVolume = { device = "/dev/vdb"; fsType = "btrfs"; options = [ "subvol=@persistent" ]; };
      boot.nmbl.rescue.fullSystem.hostKeyPath = lib.mkForce "/nmbl-identity/etc/ssh/ssh_host_ed25519_key";
      fileSystems."/".device = lib.mkForce "/dev/intentional-missing-generation-root";
    };
  };
  build = config.config.system.build;
  pkgs = flake.inputs.nixpkgs.legacyPackages.x86_64-linux;
  lib = flake.inputs.nixpkgs.lib;
  invalid = profile: config.extendModules {
    modules = [ {
      boot.nmbl.rescue.fullSystem.networkStage.staticProfiles = lib.mkForce [ profile ];
    } ];
  };
  evaluates = systemConfig:
    (builtins.tryEval systemConfig.config.system.build.toplevel.drvPath).success;
  bothSelectors = invalid {
    interfaceName = "eth0";
    macAddress = "52:54:00:12:34:56";
    ipv4.addresses = [ "10.0.2.15/24" ];
  };
  noAddress = invalid { interfaceName = "eth0"; };
  wrongFamily = (invalid {
    interfaceName = "eth0";
    ipv6.addresses = [ "fec0::15/64" ];
  }).extendModules {
    modules = [ {
      boot.nmbl.rescue.fullSystem.networkStage.addressFamily = lib.mkForce "ipv4-only";
    } ];
  };
in
assert !evaluates bothSelectors;
assert !evaluates noAddress;
assert !evaluates wrongFamily;
pkgs.linkFarm "nmbl-network-stage-vm-artifacts" ([
  { name = "kernel"; path = "${build.nmblKernel}/bzImage"; }
  { name = "initrd"; path = "${build.nmblInitramfs}/initrd"; }
  { name = "config.toml"; path = build.nmblConfigToml; }
  { name = "rescue.sfs"; path = build.nmblRescueSquashfs; }
  ] ++ lib.optional (!(bakedStatic || bakedSlaac || nativeIdentity)) { name = "network.erofs"; path = build.nmblNetworkStage; } ++ lib.optional (!(bakedStatic || bakedSlaac || nativeIdentity))
  { name = "rescue-installer"; path = build.nmblRescueStageInstaller; }
)
