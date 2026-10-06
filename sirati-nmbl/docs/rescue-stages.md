# Staged full-system rescue

`boot.nmbl.rescue.fullSystem.enable = true` (with `rescue.mode = "external"`)
starts the recovery system in two stages. NMBL stays PID 1 the whole time.

## Stage 1: NMBL itself

The kernel has already unpacked NMBL's initramfs, and the rescue runs on
NMBL's kernel, so stage 1 has no archive of its own. For a full-system rescue
the initramfs carries `erofs.ko`, `loop` and `overlay`. The flat rescue
carries `squashfs.ko` in place of `erofs.ko`. NMBL's runtime config
(`config.toml`, embedded or signed under the boot-config domain) says what to
mount:

```toml
[rescue]
mode = "external"
entrypoint = "/init"
sfs_path = "nmbl-rescue.sfs"        # relative to the boot partition; the name is historic

[rescue.image]
format = "erofs"
sha512 = "<128 hex digits>"         # the exact stage-2 image this config was built with

[rescue.network_stage]              # only with fullSystem.networkStage.enable
path = "nmbl/network.erofs"
sha512 = "<128 hex digits>"

[rescue.tools]                      # the nmblctl image
path = "nmbl/rescue-tools.erofs"
sha512 = "<128 hex digits>"

[rescue.system]                     # this host's rescue settings
sshd_port = 22222
authorized_keys = ["ssh-ed25519 AAAA... operator"]
host_key_path = "/nmbl-identity/etc/ssh/ssh_host_ed25519_key"
modules = ["overlay", "ext4", "af_packet", "e1000e"]
network_profile = "version 2\naddress-family dual-stack\n..."   # without a networking stage
```

The build computes the digests from the images it ships and appends these
tables to `config.toml`. It then validates the result with the same
`nmbl-init` binary. The config lives next to its images. That is `/boot`, or
the active generation directory on hosts with EROFS generations. A new
rescue image therefore comes with a new config and needs no new NMBL
initramfs.

On rescue entry NMBL:

1. opens the stage-2 image once;
2. verifies its signature over that descriptor (domain `nmbl:rescue-sfs:v1`)
   when signing is enabled. Under enforcement a bad signature refuses the
   boot;
3. compares the image's SHA-512 with `[rescue.image].sha512`, and reuses the
   digest from the signature check. A mismatch rejects the image before NMBL
   mounts it, even if the signature is valid (for example an older rescue
   build). NMBL then tries the network fallback when `rescue.network` is
   set, or halts and shows the reason;
4. loads `loop`, `erofs` and `overlay` from its initramfs, binds the same
   descriptor to a loop device, and mounts it read-only under a tmpfs overlay
   at `/rescue`;
5. with a networking stage, verifies, pins and mounts that image at
   `/rescue/nmbl-network` in the same way;
6. verifies, pins and mounts the tools image at `/rescue/nmbl-tools` in the
   same way. Audit mode does not relax this signature check. If the tools
   image fails a check, NMBL leaves it unmounted, writes the reason to
   `/etc/nmbl-tools-disabled`, and the rescue runs without `nmblctl`;
7. validates `[rescue.system]` and writes it as plain data files to
   `/etc/nmbl-rescue/` in the rescue overlay. The network profile goes
   through the same strict parser as a networking stage's profile. NMBL
   skips a value that fails validation;
8. starts `/init` from the image as a chrooted child and bind-mounts NMBL's
   root at `/nmbl-root` inside it.

When the operator commits an action (boot a generation, retry or reboot) in
an `nmbl` session inside the rescue, NMBL stops the rescue. It sends
`SIGTERM` and then `SIGKILL` to every rescue process, including SSH
sessions, syncs the filesystems and unmounts the rescue tree. It then
performs the action. A retried boot uses the installed mounts that the
rescue's `/mnt` covered. When the rescue exits without an action, NMBL
reboots.

A pin that NMBL cannot check fails closed. This covers a malformed digest
and a binary built without the `rescue-stages` Cargo feature. The Nix build
enables that feature whenever it writes a pin.

## Stage 2: the rescue image

Stage 2 is an EROFS image with the whole recovery system. It contains its
own `/nix/store` (and a Nix database for the full profile), the kernel
modules and firmware for its NICs (built for NMBL's kernel), sshd and the
operator tools.

The image does not depend on the host. Its only inputs are the rescue
package set, the module closure for NMBL's kernel and fixed scripts. It
contains no addresses, network profile, authorized keys, sshd port, host key
path, module list or hostname. Two hosts with the same NMBL kernel and rescue
packages share one image store path. A change to the host configuration does
not rebuild the image, which matters because EROFS compression is the slow
part of a rescue build.

The fixed `/init` in the image reads the host data from `/etc/nmbl-rescue/`
and fails closed. Without a network profile or port, the rescue stays on the
local console. Without keys, remote login is off. With a networking stage
(`/etc/nmbl-rescue/network-stage`), `/init` loads modules and the profile
from `/nmbl-network` only. The SSH host key comes from the configured path
in NMBL's namespace (under `/nmbl-root`). Without a configured key, `/init`
generates one per boot.

## The tools image

`nmblctl` is built with this host's signing public keys, so it lives in a
small EROFS of its own and stays out of the stage-2 image. The image holds
`nmblctl` and its closure. Its path is `fullSystem.toolsImagePath` (default
`nmbl/rescue-tools.erofs`), and `[rescue.tools]` pins it. The rescue `/init`
links its store paths into the rescue store and its `bin/` into `/bin`. The
tools image uses the same compressor as stage 2. A change to it does not
rebuild stage 2.

## Compression

The default image uses LZ4HC in 64 KiB clusters, with tail packing,
fragments and deduplication. It decompresses faster than the squashfs with
zstd level 19 in 128 KiB blocks that the rescue used before.
`fullSystem.compression = "zstd"` builds a smaller image for a small boot
partition. EROFS supports zstd from kernel 6.10, and an assertion rejects
`zstd` when `boot.nmbl.kernelPackage` is older. `"none"` stores the image
uncompressed.

Measured on the minimal DNS-VPS profile (a 188 MB tree):

| Image | Size | Build |
| --- | --- | --- |
| squashfs, zstd-19 (before) | 57 MB | 4 s |
| EROFS, LZ4HC (default) | 76 MB | 29 s |
| EROFS, zstd-19 | 53 MB | 424 s |

The build times are forced rebuilds and include copying the closure.
`mkfs.erofs` compresses on one thread because its multi-threaded output is
not reproducible. This makes it slower than `mksquashfs`. The image only
rebuilds when NMBL's kernel or the rescue package set changes.

## Storage tools

The default `fullSystem.packages` include each storage tool only when the
host uses that stack:

| Tool | Included when |
| --- | --- |
| `cryptsetup` | NMBL's `activation.luks` or `boot.initrd.luks.devices` is set (full profile). |
| `btrfs-progs` | A filesystem or the identity volume uses Btrfs, or `boot.supportedFilesystems` lists it. The minimal profile uses a build without the ext4 converter. |
| `lvm2` | NMBL's LVM activation is on (full profile). |
| `mdadm` | NMBL's mdraid activation or `boot.swraid.enable` is on. |

`lib/rescue/storage-features.nix` computes these conditions. The minimal
profile loads the `btrfs`, `raid1` and `nvme` modules only when the host
uses them. The `rescue-storage-tools-eval` flake check tests these
rules.

## Login and the remote TUI

sshd prints `/etc/motd` after an interactive login:

```text
NMBL rescue - run `nmbl` to enter the bootloader.
```

`/bin/nmbl` is NMBL's own binary, which the rescue bind-mounts into the
image. `nmbl-tui` links to it. Run outside PID 1, it connects to
`NMBL_TUI_SOCK` (`/nmbl-root/nmbl-run/tui.sock`) and shows NMBL's menu. The
operator can boot a generation, retry or reboot from there. Ctrl+E ends the
session. NMBL keeps a session open when the client reads slowly, and ends it
when the client or its SSH connection goes away.

## The flat rescue

With `fullSystem.enable = false`, the rescue is the busybox squashfs from
`rescue.squashfsContents`. The initramfs carries the `squashfs` module, and
the `[rescue]` section has no pin.

## Tests

* `nix build .#checks.x86_64-linux.rescue-image-host-independent` checks that
  the image derivation stays the same across hosts that differ in network
  configuration, keys, port, host key, signing keys and unrelated options, and that it
  changes for another kernel.
* `rescue-ssh-welcome` checks the motd and `PrintMotd yes`.
  `rescue-storage-tools-eval` checks the storage tool rules.
  `rescue-compression-kernel-eval` checks the zstd kernel assertion. All
  three are pure checks.
* `cargo test --features rescue-stages` covers the config tables, the pin
  and the handover of host data.
* `nix run .#test-network-stage-vm` boots the signed, baked-static,
  baked-SLAAC and identity variants from the stage-2 EROFS. It re-pins a
  malformed network stage to show that the strict profile parser still
  rejects it. It also boots a validly signed but different rescue image,
  which the pin must refuse. It runs `nmblctl` in the rescue and refuses a
  tampered tools image before mounting it. Each VM prints `NMBL_TIMING` lines.
  `rescue_phase` covers the time from the start of the rescue mount until
  the rescue is ready. `rescue_phase_ssh` covers the time until SSH answers.
