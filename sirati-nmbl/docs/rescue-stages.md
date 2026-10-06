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
6. starts `/init` from the image as a chrooted child, with NMBL's root
   bind-mounted at `/nmbl-root`.

A pin that cannot be checked (malformed, or a binary built without the
`rescue-stages` feature) fails closed. The Nix build enables that feature
whenever it renders a pin.

## Stage 2: the rescue image

An EROFS image holding the whole recovery system: its own `/nix/store` (and,
for the full profile, a Nix database), the kernel modules and firmware for its
NICs (built for NMBL's kernel), the baked static/SLAAC/DHCP network profile,
sshd with its own authorized keys, and the operator tools. Without a
networking stage it brings up its baked network itself. With one, it loads
modules and the profile from `/nmbl-network`. The SSH host identity still
comes from `fullSystem.hostKeyPath` in NMBL's namespace.

The image is compressed with LZ4HC in 64 KiB clusters, with tail packing,
fragments and deduplication. Decompression is much cheaper than squashfs with
zstd-19 in 128 KiB blocks, which the rescue used before.
`fullSystem.compression = "zstd"` builds a smaller image for a tight boot
partition (kernel 6.10 or later); `"none"` stores it uncompressed. Measured on
the minimal DNS-VPS profile (a 188 MB tree):

| Image | Size |
| --- | --- |
| squashfs, zstd-19 (before) | 57 MB |
| EROFS, LZ4HC (default) | 75 MB |
| EROFS, zstd-19 | 52 MB |

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

* `nix build .#checks.x86_64-linux.rescue-ssh-welcome` and
  `rescue-storage-tools-eval` (pure);
* `cargo test --features rescue-stages` covers the config tables and the pin;
* `nix run .#test-network-stage-vm` boots the signed, baked-static, baked-SLAAC
  and identity variants from the stage-2 EROFS, re-pins a malformed network
  stage so the strict profile parser still rejects it, and boots a validly
  signed but substituted rescue image that the pin must refuse. Each VM
  prints `NMBL_TIMING` lines (`rescue_phase`: NMBL starts mounting the rescue
  until the rescue is ready; `rescue_phase_ssh`: until SSH answers).
