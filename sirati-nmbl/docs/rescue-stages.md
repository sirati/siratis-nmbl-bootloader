# Staged full-system rescue

`boot.nmbl.rescue.fullSystem.enable = true` (with `rescue.mode = "external"`)
starts the recovery system in two stages. NMBL stays PID 1 throughout.

## Stage 1: NMBL itself

The kernel has already unpacked NMBL's initramfs, and the rescue runs on
NMBL's kernel, so stage 1 is not a separate archive. For a full-system rescue
the initramfs carries `erofs.ko` (plus `loop` and `overlay`) in place of
`squashfs.ko`. NMBL's runtime config (`config.toml`, embedded or signed under
the boot-config domain) says what to mount:

```toml
[rescue]
mode = "external"
entrypoint = "/init"
sfs_path = "nmbl-rescue.sfs"        # boot-partition-relative; the name is historic

[rescue.image]
format = "erofs"
sha512 = "<128 hex digits>"         # the exact stage-2 image this config was built with

[rescue.network_stage]              # only with fullSystem.networkStage.enable
path = "nmbl/network.erofs"
sha512 = "<128 hex digits>"

[rescue.system]                     # this host's rescue settings
sshd_port = 22222
authorized_keys = ["ssh-ed25519 AAAA... operator"]
host_key_path = "/nmbl-identity/etc/ssh/ssh_host_ed25519_key"
modules = ["overlay", "ext4", "af_packet", "e1000e"]
network_profile = "version 2\naddress-family dual-stack\n..."   # without a networking stage
```

The build computes both digests from the images it ships and appends these
tables to `config.toml`. It then validates the result with the same
`nmbl-init` binary. The config lives with its images: on `/boot`, or in the
active generation directory for EROFS generations. A new rescue image
therefore comes with a new config, and never needs a new NMBL initramfs.

On rescue entry NMBL:

1. opens the stage-2 image once;
2. verifies its signature over that descriptor (domain `nmbl:rescue-sfs:v1`,
   when signing is enabled; enforce refuses as before);
3. compares the image's SHA-512 with `[rescue.image].sha512`, reusing the
   digest the signature check already streamed. A mismatch refuses the image
   before it is mounted, even if it is validly signed (for example an older
   rescue build), and NMBL halts with the reason on the console;
4. loads `loop`, `erofs` and `overlay` from its initramfs, binds that same
   descriptor to a loop device, and mounts it read-only under a tmpfs overlay
   at `/rescue`;
5. with a networking stage, verifies, pins and mounts it at
   `/rescue/nmbl-network` the same way;
6. validates `[rescue.system]` and writes it as plain data files into the
   rescue overlay at `/etc/nmbl-rescue/`. The baked network profile goes
   through the same strict parser as a networking stage's profile; a value
   that fails validation is not written;
7. starts `/init` from the image as a chrooted child, with NMBL's root
   bind-mounted at `/nmbl-root`.

A pin that cannot be checked (malformed, or a binary built without the
`rescue-stages` feature) fails closed. The Nix build enables that feature
whenever it renders a pin.

## Stage 2: the rescue image

An EROFS image holding the whole recovery system: its own `/nix/store` (and,
for the full profile, a Nix database), the kernel modules and firmware for its
NICs (built for NMBL's kernel), sshd, and the operator tools.

The image is host-independent, so it does not rebuild when a host's
configuration changes (EROFS compression is the slow part of a rescue build).
Its only inputs are the rescue package set, the module closure for NMBL's
kernel, and fixed scripts. It contains no addresses or network profile, no
authorized keys, no sshd port, no host key path, no module choice and no
hostname. Two hosts with the same NMBL kernel and rescue packages share one
image store path. Its fixed `/init` reads the host data NMBL handed over in
`/etc/nmbl-rescue/` and fails closed: a missing network profile or port keeps
the rescue local-console only, and missing keys disable remote login. With a
networking stage configured (`/etc/nmbl-rescue/network-stage`) it loads
modules and the profile from `/nmbl-network` only. The SSH host identity
comes from the configured key in NMBL's namespace (`/nmbl-root/...`) or is
generated per boot.

`nmblctl` is built with the signing public keys it must trust, so it is not
in this image. It ships in a small EROFS of its own
(`fullSystem.toolsImagePath`, default `nmbl/rescue-tools.erofs`), pinned under
`[rescue.tools]` and checked like the networking stage. NMBL mounts it at
`/nmbl-tools` and the rescue puts it on PATH. If NMBL refuses the tools
image, the rescue runs without `nmblctl`.

The image is compressed with LZ4HC in 64 KiB clusters, with tail packing,
fragments and deduplication. Decompression is much cheaper than squashfs with
zstd-19 in 128 KiB blocks, which the rescue used before.
`fullSystem.compression = "zstd"` builds a smaller image for a tight boot
partition (kernel 6.10 or later); `"none"` stores it uncompressed. Measured on
the minimal DNS-VPS profile (a 188 MB tree):

| Image | Size | Build |
| --- | --- | --- |
| squashfs, zstd-19 (before) | 57 MB | 4 s |
| EROFS, LZ4HC (default) | 76 MB | 29 s |
| EROFS, zstd-19 | 53 MB | 424 s |

The build times are forced rebuilds, including copying the closure.
`mkfs.erofs` compresses single-threaded because its multi-threaded output is
not reproducible, which is slower than `mksquashfs`. Because the image is
host-independent (below), it is rebuilt only when NMBL's kernel or the rescue
package set changes, not on every host configuration change.

## Only the storage tools the host uses

The default `fullSystem.packages` include `cryptsetup` only with LUKS (NMBL's
`activation.luks` or `boot.initrd.luks.devices`), `btrfs-progs` only with a
Btrfs filesystem (or a Btrfs identity volume), `lvm2` only with NMBL's LVM
activation, and `mdadm` only with mdraid. The minimal profile likewise loads
the `btrfs`, `raid1` and `nvme` modules only when the host uses them. The
`rescue-storage-tools-eval` flake check pins this.

## The flat rescue

`fullSystem.enable = false` keeps the busybox squashfs, the `squashfs` module
and an unpinned `[rescue]` section, exactly as before.

## Tests

* `nix build .#checks.x86_64-linux.rescue-image-host-independent`, which
  asserts that the image derivation is identical across hosts differing in
  network configuration, keys, port, host key and unrelated options, and
  differs for another kernel; plus `rescue-ssh-welcome` and
  `rescue-storage-tools-eval` (pure);
* `cargo test --features rescue-stages` covers the config tables, the pin
  and the hand-over of host data;
* `nix run .#test-network-stage-vm` boots the signed, baked-static, baked-SLAAC
  and identity variants from the stage-2 EROFS, re-pins a malformed network
  stage so the strict profile parser still rejects it, and boots a validly
  signed but substituted rescue image that the pin must refuse. Each VM
  prints `NMBL_TIMING` lines (`rescue_phase`: NMBL starts mounting the rescue
  until the rescue is ready; `rescue_phase_ssh`: until SSH answers).
