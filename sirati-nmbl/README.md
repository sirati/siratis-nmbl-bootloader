# NMBL — NixOS Minimal BootLoader

Linux as a bootloader. NMBL boots a tiny pinned Linux kernel plus a
minimal initramfs, lets the operator pick a NixOS generation, and
`kexec`s straight into it. The bootloader never copies kernels or
initrds onto a boot partition — it reads them in place from the
target system's `/nix/var/nix/profiles/system-N-link/`, which is the
whole point of NMBL.

The userspace in that initramfs is a single static Rust binary,
`nmbl-init`, that runs as PID 1.

## Why?

Conventional bootloaders (GRUB, systemd-boot) duplicate kernel
images onto an ESP and reimplement filesystem drivers in their own
codebase. NMBL skips both: a real Linux kernel mounts the real
root filesystem with the real kernel driver, walks the real Nix
profile symlinks, and kexecs the generation the operator chose.

That buys you:

- Any storage stack Linux can mount is bootable (LVM, LUKS, mdraid,
  ZFS, btrfs subvolumes, network block devices once you load the
  module, …).
- The boot partition only needs the NMBL kernel and initramfs; it
  does not need to be kept in sync with every NixOS generation.
- Boot-time interactivity (cmdline editing, passphrase entry,
  emergency shell) lives in a TUI instead of a half-broken
  bootloader scripting language.

## What's in the initramfs

| Path | Purpose |
|------|---------|
| `/init` | `nmbl-init` — static musl Rust binary, runs as PID 1. |
| `/etc/nmbl/config.toml` | Runtime config rendered by Nix at build time. |
| `/lib/modules/<ver>/…` | Kernel modules closure. |
| `/etc/modprobe.d/nixos.conf` | Module blacklists. |
| `/bin/sh` | busybox — **only** used as the emergency shell. |

`nmbl-init` performs every boot-time operation itself via direct
syscalls (`mount(2)`, `finit_module(2)`, `kexec_file_load(2)`,
`reboot(2)`). It does not shell out to `mount`, `modprobe`, or
`kexec`. The only binary the Rust init may `execve` is the
emergency shell, and that only when a fatal error occurs.

busybox is in the initramfs to satisfy the emergency-shell contract
and nothing else.

Storage-activation helpers (`cryptsetup`, `vgchange`/`lvchange`,
`mdadm`, `zpool`/`zfs`) are added to the initramfs **only** when
the corresponding `boot.nmbl.activation.*` options request them,
and `pkgsStatic` variants are preferred when available.

## What changed from the bash bootloader

The pre-Rust NMBL was a busybox shell script driven by Nix string
interpolation. That is gone. Concretely:

- `init` is the `nmbl-init` static musl Rust binary (roughly 700 KiB
  stripped), not a shell script.
- Runtime state lives in `/etc/nmbl/config.toml`, rendered by
  `lib/config-toml.nix` from your NixOS options. Changing
  `boot.nmbl.*` regenerates the TOML; the binary itself does not
  need rebuilding for config changes.
- The TUI is a real `ratatui` interface — generation list, countdown
  with first-key cancel, cmdline editor with caret, kernel-param
  passthrough toggle, LUKS passphrase modal, emergency-shell key.
  It works over `/dev/console`, including serial consoles.
- Storage activation (LVM, LUKS via TPM / keyfile / passphrase,
  mdraid, ZFS) is performed before mounting the target root.
  Required tools are pulled into the initramfs conditionally based
  on `boot.nmbl.activation.*`, so you do not pay for storage stacks
  you don't use.
- `nmbl-init` is forbidden from panicking: `panic`, `unwrap`,
  `expect`, raw indexing, `todo!`, `unimplemented!`, and
  `unreachable!` are clippy-denied. A panic hook installed at
  startup re-`execve`s `/proc/self/exe` with `--errored=<path>` to
  enter a recovery mode that drops to the emergency shell with the
  panic report attached.
- The `kexec-tools` and `kmod` userspace packages are no longer in
  the initramfs; their work is done by the Rust binary directly.

## Quick start

Run a VM that exercises the full GPT+UEFI+GRUB bootstrap path into
NMBL:

```bash
nix run .#test-gpt-uefi-grub
```

Other prebuilt demo configurations:

```bash
nix run .#test-gpt-bios                  # legacy BIOS via GRUB
nix run .#test-gpt-uefi-systemd          # systemd-boot bootstrap
nix run .#test-gpt-qemu-kernel-invoke    # QEMU -kernel direct boot
nix run .#test-gpt-qemu-kernel-invoke -- --debug-shell
```

All VMs are wired through `vm-serial-man`, which exposes a serial
console you can drive from another shell:

```bash
vm-serial-man status
vm-serial-man send 'ls /mnt/system/nix/var/nix/profiles'
vm-serial-man send $'\x1b[B'   # arrow keys etc. work too
vm-serial-man stop
```

To inspect the TOML config that would be embedded in the initramfs
for a given configuration, evaluate the corresponding builder
output:

```bash
nix build .#nixosConfigurations.test-gpt-uefi-grub.config.system.build.nmblInitramfs
```

The rendered config TOML is produced by `lib/config-toml.nix`.

## NixOS module options

Users normally only set a handful of options. The full surface is
in `lib/options.nix` and `lib/modules/activation.nix`.

```nix
{
  boot.nmbl = {
    enable = true;

    bootstrapper = {
      partition_table = "gpt";
      bootMode = "uefi";          # "bios" | "uefi" | "qemu_kernel_invoke"
      loader = "grub";            # "grub" | "systemd" | null
    };

    timeoutSeconds = 3;           # countdown before auto-boot
    serialConsole = "ttyS0,115200";  # null = video console

    kernelModules = [ "nvme" "ahci" ];   # explicitly load at boot
    blacklistedKernelModules = [ ];

    # Storage activations — only what you actually use:
    activation.lvm.enable = true;
    activation.mdraid.enable = true;
    activation.zfs.pools = [ "rpool" ];
    activation.luks = [
      { name = "cryptroot"; device = "/dev/nvme0n1p3"; unlock = "password"; }
    ];
  };
}
```

Notable points:

- `boot.nmbl.activation.{lvm,mdraid,zfs}` auto-detect from
  `config.fileSystems`: if any filesystem sits on `/dev/mapper/*`,
  `/dev/md*`, or has `fsType = "zfs"`, the corresponding activation
  defaults to on.
- `activation.luks[*].unlock` is one of `tpm` (TPM-sealed token),
  `keyfile` (bundled into the initramfs), or `password` (entered in
  the TUI passphrase modal).
- `verbose` defaults to inheriting `boot.initrd.verbose`.

## Where to find things

| Path | What it is |
|------|------------|
| `nmbl-init-rs/` | Rust crate — the `/init` binary. |
| `nmbl-init-rs/PLAN.md` | Source-of-truth design contract. |
| `nmbl-init-rs/src/config.rs` | Runtime TOML schema (`serde` types). |
| `nmbl-init-rs/src/main.rs` | Phase orchestration, panic recovery. |
| `nmbl-init-rs/src/ui/` | `ratatui` TUI (list, editor, passphrase modal). |
| `lib/options.nix` | `boot.nmbl.*` NixOS option definitions. |
| `lib/config.nix` | Module implementation (assembles the initramfs). |
| `lib/config-toml.nix` | Renders `/etc/nmbl/config.toml` from `cfg`. |
| `lib/modules/activation.nix` | Activation options + computed outputs. |
| `lib/modules/kernel-modules.nix` | Module closure and `modprobe.conf`. |
| `lib/modules/assertions.nix` | Build-time validation. |
| `lib/install-bootloader.nix` | Hook NixOS calls during `nixos-rebuild`. |
| `testing/` | VM harnesses and `nix run .#test-*` apps. |
| `ARCHITECTURE.md` | Longer-form architecture notes. |

## Status

Working:

- Pseudo-fs mount, explicit kernel module load (with `MODULE_INIT_COMPRESSED_FILE` for `.ko.xz`/`.ko.zst`).
- Device-wait poll and configured filesystem mount.
- NixOS generation discovery from `/nix/var/nix/profiles/`.
- Ratatui TUI: generation list, countdown, cmdline editor, passthrough toggle, emergency shell, serial-console fallback.
- Storage activation: LVM (`vgchange -ay`), mdraid (`mdadm --assemble --scan`), LUKS via TPM / keyfile / passphrase, ZFS (`zpool import -N`).
- `kexec_file_load(2)` handover into the selected generation.
- Panic hook with `--errored` recovery re-exec.
- Bootstrapper installation via GRUB or systemd-boot on GPT for BIOS or UEFI, plus QEMU `-kernel` direct invocation.

Not supported in v1:

- `LABEL=`, `UUID=`, `PARTUUID=` filesystem specifiers — `nmbl-init` has no udev, so only raw `/dev/*` paths are resolved. The config loader rejects the others up front.
- LUKS unlock via FIDO2 / YubiKey / smartcard.
- MBR partition tables (only GPT is supported by the bootstrapper).

Deferred:

- Reading `/etc/nmbl/config.toml` from the boot partition instead of the initramfs, so config edits don't require rebuilding the initramfs.
- A squashfs rescue blob mounted on demand from the emergency shell.

## License

MIT License. The scripts and Rust source in this tree are MIT; the
content rendered into the initramfs (kernel, busybox, storage
tools, etc.) carries its own licenses.
