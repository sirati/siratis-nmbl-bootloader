# Rescue networking EROFS stage

`boot.nmbl.rescue.fullSystem.networkStage.enable = true` moves the rescue
network policy into a separately signed EROFS image, by default
`nmbl/network.erofs` on the boot filesystem. The stage also carries the
kernel module closure and firmware for NMBL's kernel. The stage-2 rescue
image keeps its own copy of that closure, because it is host-independent and
does not depend on this setting. See [rescue-stages.md](rescue-stages.md) for
the stage-1 and stage-2 rescue.

The stage contains:

```text
/etc/nmbl-network/network.conf
/lib/modules/<kernel version>/...
/lib/firmware/...
```

## Mounting

NMBL's runtime config names the boot-relative image path and pins its
SHA-512 in `[rescue.network_stage]`. After NMBL has mounted the stage-2 image
under its overlay at `/rescue`, it:

1. opens the networking image once;
2. verifies the sidecar (`<image><sigPathSuffix>`) over that descriptor under
   the `nmbl:network-stage:v1` ML-DSA domain, then compares the pinned
   SHA-512 with the digest that check computed;
3. loads `erofs` from its initramfs;
4. binds the same descriptor read-only to a loop device;
5. mounts it at `/rescue/nmbl-network` with `ro,nodev,nosuid,noexec`;
6. validates `etc/nmbl-network/network.conf` with its strict profile parser.

If any step fails, NMBL writes `/etc/nmbl-network-disabled` into the rescue
overlay. The rescue `/init` then starts neither networking nor sshd and opens
the local console. NMBL also writes an empty `/etc/nmbl-rescue/network-stage`
marker when a stage is configured. With that marker, `/init` loads the
modules named in `/etc/nmbl-rescue/modules` with `modprobe -d /nmbl-network`,
points the firmware loader at `/nmbl-network/lib/firmware`, and reads its
profile from `/nmbl-network/etc/nmbl-network/network.conf` only. NMBL rejects
a `network_profile` in `[rescue.system]` next to a configured stage.

## Network profile

The same options build the profile with or without a stage. Without a stage,
NMBL hands the profile to the rescue from `[rescue.system].network_profile`
as `/etc/nmbl-rescue/network.conf`, after the same strict parser accepts it,
and the NIC drivers come from the stage-2 image.

| Option | Default | Meaning |
| --- | --- | --- |
| `networkStage.enable` | `false` | Build and use the signed stage. |
| `networkStage.imagePath` | `"nmbl/network.erofs"` | Image path relative to the boot filesystem. |
| `networkStage.addressFamily` | `"dual-stack"` | `dual-stack`, `ipv4-only` or `ipv6-only`. |
| `networkStage.interfaces` | `[ ]` | DHCP interfaces. Empty means every NIC. |
| `networkStage.staticProfiles` | `[ ]` | Static profiles. Empty means DHCP. |
| `networkStage.dnsServers` | `[ ]` | DNS servers for static profiles. |

With no static profiles, the profile is `version 1`. The rescue brings up the
listed interfaces (or every NIC), waits up to 20 seconds for a carrier, and
runs `dhcpcd --nodev --slaac hwaddr` with the address-family flags and a
20-second timeout. `--nodev` makes dhcpcd use kernel interfaces, because the
rescue runs no udev.

With static profiles, the profile is `version 2` and the rescue starts no
DHCP client. Each profile selects one NIC by kernel name (`interfaceName`) or
canonical MAC address (`macAddress`), and needs at least one address. Each
address family carries `addresses`, an optional `gateway` with
`gatewayOnLink`, and `routes` with `destination` (a CIDR or `default`), an
optional `via` and `onLink`. `interfaces` cannot be combined with static
profiles, and the addresses must match `addressFamily`. The rescue applies
the directives with `ip` and fails to local-console mode on any unknown or
malformed line. The shell does not evaluate file content.

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

## Requirements and installation

The stage requires the external full-system rescue
(`rescue.mode = "external"` and `rescue.fullSystem.enable`), enforced NMBL
signing, and `rescue.fullSystem.hostKeyPath`. `imagePath` must be relative
and free of `..`.

Build with only the operator public key. The bootloader installer runs the
generated `nmbl-install-rescue-stage` unless `signing.deferInstallSigning` is
set. You can also run it on the target, or from an operator environment that
can write the boot filesystem:

```console
NMBL_BOOT_ROOT=/boot \
NMBL_IMAGE_KEY_FILE=/secure/off-host/image.key \
  /nix/store/...-nmbl-install-rescue-stage/bin/nmbl-install-rescue-stage
```

`system.build.nmblRescueStageInstaller` provides that command. It installs
and signs the stage-2 image at `rescue.sfsPath` (domain `rescue-sfs`) and the
stage at `imagePath` (domain `network-stage`). The `config.toml` built with
them pins both by SHA-512, so the config, the images and the signatures must
come from one build. The installer takes the key from `NMBL_IMAGE_KEY_FILE`
first, then from `signing.imageKeyCommand`, then from `signing.imageKeyFile`.
A key file must exist and lie outside `/nix/store`. `imageKeyCommand` is an
argv that prints the key; the installer runs it once per image and pipes the
output into `nmbl-sign sign --key-stdin`.

The installer writes each image through an install-if-changed helper. It
leaves an identical file untouched and replaces a changed one through a
temporary file and a rename, so an ordinary configuration switch does not
rewrite a large image.

On hosts with `generationImage.enable`, the installer stages neither image.
Both travel in the signed generation directory, as described in
[erofs-generations.md](erofs-generations.md).

## SSH host identity

`rescue.fullSystem.hostKeyPath` names an Ed25519 private key in NMBL's mount
namespace, for example on a bootstrap-mounted state volume. NMBL hands the
path to the rescue as `/etc/nmbl-rescue/host-key-path`, and the rescue opens
it below `/nmbl-root`. The rescue rejects a symlink, a missing file, an owner
other than `root:root` or a mode other than `0600`. On rejection it keeps the
local console and starts no sshd. The key contents do not enter the Nix
store or the rescue image.

`rescue.fullSystem.identityVolume` (`device`, `fsType` of `btrfs`, `ext4` or
`xfs`, and `options`) makes NMBL mount a plaintext identity filesystem at
`/nmbl-identity` before rescue. NMBL mounts it read-only with
`nodev,nosuid,noexec` and log replay off, and accepts only a Btrfs `subvol=`
or `subvolid=` option. With an identity volume, `hostKeyPath` must lie below
`/nmbl-identity/`. If the volume does not mount, SSH stays off.

## Tests

`nix run .#test-network-stage-vm` runs the production flow. Its outer harness
creates one-use ML-DSA and SSH keys in a private tmpfs, evaluates Nix with
only the public keys, runs the production `nmbl-install-rescue-stage` twice
(the second run must leave the stage's inode unchanged), and deletes the
private key before booting. It scans the sources, store closures, initrd,
rescue and networking images, signatures and boot disks for the key bytes and
a random marker.

| Scenario | Expected result |
| --- | --- |
| `good` | Signed stage mounted `ro,nodev,nosuid,noexec`, modules and firmware from the stage, static profile applied without dhcpcd, persistent host key used, remote TUI over SSH. |
| `tampered`, `unsigned` | Stage rejected, local console only. |
| `malformed` | Correctly signed and pinned stage with an invalid profile, rejected by the parser, local console only. |
| `substituted` | A different validly signed stage-2 image, refused by the pin before mounting. |
| `baked-static` | No stage, static profile from `[rescue.system]`, sshd listening. |
| `baked-slaac` | No stage, `ipv6-only` DHCP profile, SLAAC address, sshd listening. |
| `native-identity` | Host key from a read-only Btrfs identity volume. |
| `missing-identity` | Identity volume configured but absent, so no host key and no sshd. |

For each scenario that reaches the rescue, the harness prints one line:

```text
NMBL_TIMING mode=good rescue_ready=<t>s rescue_phase=<t>s ssh_ready=<t>s rescue_phase_ssh=<t>s
```

`rescue_ready` and `ssh_ready` count from QEMU start until the rescue reports
ready and until an authenticated SSH command first succeeds. `rescue_phase`
and `rescue_phase_ssh` count the same events from the moment NMBL starts to
mount the rescue. The harness also writes the figures to
`<transcript>.timing.json`.
