# Boot stages

A host with NMBL boots in this order:

1. The firmware starts the loader (GRUB, systemd-boot, or the NMBL UKI as an
   EFI stub), which starts NMBL's kernel and initramfs. The
   `qemu_kernel_invoke` test mode starts NMBL with QEMU's `-kernel`.
2. NMBL runs as PID 1 in its own initramfs. It unlocks storage, mounts the
   system, picks a generation and verifies it when signing is on.
3. NMBL kexecs the generation's kernel with the generation's initrd. NMBL
   appends a cpio fragment with its boot log (`/nmbl-log/nmbl.log`) and any
   `passToStage1` LUKS keyfiles (`/etc/nmbl-luks/<name>`).
4. The generation's NixOS stage 1 (the scripted initrd or the systemd initrd)
   mounts the root filesystem and starts stage 2.
5. NixOS stage 2 activates the system.

`lib/modules/log-import.nix` writes NMBL's boot log to the journal under the
`nmbl-init` tag and then deletes it. The compiled `nmbl-log-import`
(nmbl-host-tools) reads it, escapes control bytes and invalid UTF-8, bounds
the size, and sends each line over journald's native socket. With the systemd
initrd this happens in stage 1. With the scripted initrd, stage 1 copies the
file onto the booted root (`/var/lib/nmbl`), and a stage-2 service imports it.

The full-system rescue uses its own two stages (NMBL's initramfs, then a
pinned EROFS image). See [docs/rescue-stages.md](docs/rescue-stages.md).

## NixOS references

Filesystem declarations:

- https://github.com/NixOS/nixpkgs/raw/refs/heads/master/nixos/modules/tasks/filesystems.nix
- https://github.com/NixOS/nixpkgs/raw/refs/heads/master/nixos/modules/tasks/filesystems/exfat.nix
- https://github.com/NixOS/nixpkgs/raw/refs/heads/master/nixos/modules/tasks/filesystems/vfat.nix

Stage 1:

- https://github.com/NixOS/nixpkgs/raw/refs/heads/master/nixos/modules/system/boot/stage-1.nix
- https://github.com/NixOS/nixpkgs/raw/refs/heads/master/nixos/modules/system/boot/stage-1-init.sh
- https://github.com/NixOS/nixpkgs/raw/refs/heads/master/nixos/modules/system/boot/systemd/initrd.nix

Stage 2:

- https://github.com/NixOS/nixpkgs/raw/refs/heads/master/nixos/modules/system/boot/stage-2.nix
