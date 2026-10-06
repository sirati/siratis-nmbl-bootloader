# NMBL, no more boot loader

NMBL uses Linux as the bootloader for NixOS. A small pinned kernel boots
first. Its PID 1 is `nmbl-init`, a static Rust program. `nmbl-init` mounts
the boot disk and the target system with the kernel's own drivers, picks a
NixOS generation and `kexec`s into it. Any disk layout that Linux can mount
is therefore bootable, including LUKS, LVM, mdraid, ZFS and Btrfs.

Other features:

- Signed images. NMBL can verify generations, its config and its rescue
  images with ML-DSA signatures against keys compiled into the binary. It
  can also measure the handoff into the TPM.
- Stateful rollback. NMBL records which generations reached a successful
  boot and falls back to a known-good one after a failed boot.
- Rescue. When a boot fails, NMBL can start a recovery system with SSH
  access and a remote copy of its boot menu.

NMBL supports NixOS only. It works on all 10 of the author's setups: 3 VPS,
3 dedicated servers, 1 laptop, 2 desktops and 1 live USB. Most
configuration options may be under-tested despite these efforts.

## Documentation

- [sirati-nmbl/README.md](sirati-nmbl/README.md) explains the options,
  setups and features in detail.
- [sirati-nmbl/docs/](sirati-nmbl/docs/) has one document per feature,
  such as the staged rescue, EROFS generations and `nmblctl`.
- [sirati-nmbl/ARCHITECTURE.md](sirati-nmbl/ARCHITECTURE.md) describes the
  boot phases and the code layout.
