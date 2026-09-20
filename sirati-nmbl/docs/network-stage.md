# Rescue networking EROFS stage

`boot.nmbl.rescue.fullSystem.networkStage.enable = true` moves the recovery
kernel-module closure, firmware, and network policy out of
`nmbl-rescue.sfs` into `/boot/nmbl/network.erofs`.

The rescue squashfs contains a trusted marker naming the boot-relative EROFS
path. Before the rescue child starts, NMBL:

1. opens the EROFS once;
2. verifies that pinned file descriptor under the distinct
   `nmbl:network-stage:v1` ML-DSA domain;
3. loads the minimal `erofs` filesystem module from the initramfs;
4. binds the same descriptor read-only to a loop device; and
5. mounts it at `/rescue/nmbl-network` with `nodev,nosuid,noexec`.

The rescue then loads its filesystem, packet, and NIC modules with
`modprobe -d /nmbl-network`. Firmware requests use
`/nmbl-network/lib/firmware`. The data-only `network.conf` selects one of
`dual-stack`, `ipv4-only`, or `ipv6-only`. With no `staticProfiles`, rescue
keeps the DHCP default and either discovers NICs or uses the fixed `interfaces`
list. Static profiles select exactly one NIC by kernel name or canonical MAC
address, and carry addresses, an optional default gateway, additional direct
or gateway routes, and optional DNS servers. Gateways and routes can be marked
on-link. The strict Rust parser validates the bounded file before rescue
starts; the shell only applies accepted directives and never evaluates file
content.

```nix
boot.nmbl.rescue.fullSystem.networkStage = {
  enable = true;
  addressFamily = "dual-stack";
  dnsServers = [ "1.1.1.1" "2606:4700:4700::1111" ];
  staticProfiles = [ {
    macAddress = "52:54:00:12:34:56";
    ipv4 = {
      addresses = [ "192.0.2.10/32" ];
      gateway = "192.0.2.1";
      gatewayOnLink = true;
    };
    ipv6 = {
      addresses = [ "2001:db8::10/64" ];
      routes = [ {
        destination = "default";
        via = "fe80::1";
        onLink = true;
      } ];
    };
  } ];
};
```

The stage requires enforced NMBL signing. Build with only the operator public
key, then run the generated production installer on the target or from an
operator environment that can write its boot filesystem:

```console
NMBL_BOOT_ROOT=/boot \
NMBL_IMAGE_KEY_FILE=/secure/off-host/image.key \
  /nix/store/...-nmbl-install-rescue-stage/bin/nmbl-install-rescue-stage
```

`system.build.nmblRescueStageInstaller` provides that command. It signs and
atomically installs both `nmbl-rescue.sfs` and `network.erofs` under their
separate signature domains. The private key path must be outside `/nix/store`.
Setting `signing.imageKeyFile` supplies the default imperative path;
`NMBL_IMAGE_KEY_FILE` overrides it without making the key a Nix input.

Remote rescue also requires `rescue.fullSystem.hostKeyPath`. This names an
already-provisioned Ed25519 private key in NMBL's mount namespace, normally on
a bootstrap-mounted persistent state volume. Rescue rejects a symlink, a
missing key, non-root ownership, or any mode other than 0600; on rejection it
keeps local console recovery available and does not start sshd. The key path,
but never its contents, appears in the generated rescue init script.

Boot artifacts use an atomic install-if-changed helper. Identical kernel,
initramfs, config, rescue, splash, and networking blobs are left untouched,
which avoids rewriting a large boot blob on an ordinary configuration switch.

Run `nix run .#test-network-stage-vm` for the production-flow VM test. Its
outer harness creates a one-use ML-DSA key in private tmpfs, evaluates Nix with
only the derived public key, invokes the production installer, and deletes the
private key before booting. It scans evaluated sources, store closures, initrd,
rescue/network images, signatures, and boot disks for the exact key bytes and a
random secret marker. The suite boots valid, tampered, unsigned, and correctly
signed but malformed images. The valid VM verifies static IPv4 and IPv6
addresses, gateways, routes, and DNS without starting DHCP.
Rejected networking stages keep the signed local rescue console available but
start neither networking nor SSH.
