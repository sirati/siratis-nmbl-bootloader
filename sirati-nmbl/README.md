# sirati's NMBL — no more boot loader

**Using Linux as a bootloader.** A conventional bootloader (GRUB,
systemd-boot) is a second, lesser operating system: it reimplements
filesystem drivers, a disk/partition stack, and a scripting language,
all just to locate a kernel and hand off to it. But Linux already
ships every driver for the filesystem it runs on — so the most capable
thing to boot Linux with is Linux itself.

NMBL boots a tiny pinned Linux kernel plus a minimal initramfs, lets
the operator pick a NixOS generation, and `kexec`s straight into it.
Because a *real* kernel mounts the *real* root with the *real* driver,
two long-standing bootloader restrictions simply disappear:

- **No restriction on where the system lives.** Any storage stack
  Linux can mount is bootable — LVM, LUKS, mdraid, ZFS, btrfs
  subvolumes, network block devices — with no bootloader-side driver
  to reimplement or keep up to date.
- **No copying the kernel and initrd onto a boot partition.** NMBL
  reads them in place from the target system's
  `/nix/var/nix/profiles/system-N-link/`, so the boot partition never
  has to be kept in sync with every NixOS generation. That is the
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
nix run .#test-external-config           # config.toml on /boot
nix run .#test-external-rescue           # rescue squashfs on /boot
nix run .#test-external-rescue-network   # rescue + HTTP fallback
```

## Recommended setups

The two post-v1 features (external config, external rescue) are
independently togglable. Pick the combination that matches the
operator's recovery story:

| Profile         | `configLocation` | `rescue.mode` | `rescue.network` | When to pick |
|-----------------|------------------|---------------|------------------|--------------|
| **Default install** | `embedded`   | `embedded`    | `false`          | Single-user desktop or laptop. Smallest moving-parts surface; everything ships in the initramfs. |
| **Power user**  | `external`       | `external`    | `false`          | Workstation where the operator wants edit-and-reboot config changes and a richer rescue toolbox without bloating the initramfs. |
| **Servers**     | `external`       | `external`    | `true`           | Headless / remote machines. The HTTP fallback recovers an unbootable system over the network when the boot partition's rescue blob is missing or stale. |
| **Air-gapped / tiny** | `embedded` | `none`        | `false`          | Appliances and air-gapped systems where rescue is handled out-of-band (e.g. yank the disk into another machine). NMBL halts cleanly with a banner instead of dropping to a shell. |

All four profiles boot the same `nmbl-init` binary; only the
initramfs contents and the on-boot-partition staging differ.

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
      loader = "grub";            # "grub" | "systemd" | "efi-stub" | null
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

#### Sealing a LUKS volume to the TPM (`nmbl-tpm-enroll`)

For `unlock = "tpm"` devices, the volume key is sealed to the TPM with
the host helper **`nmbl-tpm-enroll`** (shipped on the installed system,
never inside the initramfs). It is a thin wrapper over
`systemd-cryptenroll` — there is no bespoke TPM sealing code. Run it
once, after the box has first-booted the installed system, against the
LUKS header:

```sh
# Seal the volume key to the TPM, bound to PCRs 11+7 (the default).
sudo nmbl-tpm-enroll --device /dev/disk/by-partlabel/disk-main-luks
```

The **enroll → boot-unlock round trip**:

1. **Enroll (host, once).** `nmbl-tpm-enroll` runs
   `systemd-cryptenroll --tpm2-device=auto --tpm2-pcrs=11+7 <device>`,
   which generates a random volume key, seals it to the TPM under the
   `{11, 7}` PCR policy, adds a LUKS2 keyslot for it, and writes a
   `systemd-tpm2` **token** into the LUKS2 header. PCR 11 is NMBL's
   measure PCR (`boot.nmbl.tpm.pcrIndex`); PCR 7 is the firmware /
   Secure-Boot-state PCR.
2. **Boot-unlock (NMBL).** At boot NMBL runs its existing
   `cryptsetup open --token-only <device> <name>`. `--token-only` makes
   libcryptsetup consume that `systemd-tpm2` token and unseal the
   volume key from the TPM **without any passphrase prompt** — but only
   if PCR 11 (NMBL's measured handoff) and PCR 7 (Secure-Boot state)
   still match the values they had at enrol time.
3. **Tamper / rescue ⇒ secrets safe.** PCR 11 is *capped* (extended with
   a poison value) the moment NMBL diverts to rescue, and a tampered
   kernel/initrd or a firmware that stopped enforcing Secure Boot moves
   PCR 7. Either way the sealed PCR policy no longer matches, the
   `--token-only` unseal **fails**, and the box falls back to the
   passphrase modal instead of auto-unlocking — so the disk stays sealed
   on an untampered-only basis.

Keep a passphrase keyslot as a recovery path, and re-run
`nmbl-tpm-enroll --wipe-existing …` after any change to the measured
inputs (a new NMBL kernel/initrd, or a firmware update that moves PCR 7).
The default PCR set is `11+7`; override with `--pcrs` if your
`boot.nmbl.activation.luks.<name>.tpmPcrs` policy differs.

### efi-stub direct boot

With `loader = "efi-stub"` (UEFI only) NMBL's kernel + initrd are
combined into a single UKI (Unified Kernel Image) PE and written to
the ESP — no GRUB or systemd-boot binary at all. By default it lands
at the firmware removable/fallback path `EFI/BOOT/BOOTX64.EFI`, which
firmware auto-boots with no NVRAM entry: ideal for a dedicated NMBL
disk or a manually-uploaded image.

To install **alongside an existing bootloader** (e.g. GRUB) without
overwriting its fallback binary, point the UKI at its own path:

```nix
{
  boot.nmbl.bootstrapper = {
    bootMode = "uefi";
    loader   = "efi-stub";
    loader_extra_args = {
      efiStubInstallPath  = "EFI/nmbl/nmbl.efi";  # own path, not BOOTX64
      canTouchEfiVariables = true;                # register the NVRAM entry
    };
  };
}
```

An own path is not auto-booted by firmware, so NMBL registers a UEFI
NVRAM boot entry (`NMBL`, placed first in BootOrder) pointing at it,
leaving the existing bootloader's entry intact as a fallback. The
NVRAM write requires `canTouchEfiVariables = true`; with it `false`
the file is written and a warning tells you to add the boot entry by
hand.

## External configuration

By default the runtime TOML is baked into the initramfs at build
time, so any change to a NMBL knob — even a timeout tweak — requires
`nixos-rebuild`. Setting `boot.nmbl.configLocation = "external"`
splits the config into two tiers:

- **`/etc/nmbl/bootstrap.toml`** — embedded in the initramfs. Tiny.
  Carries only what `nmbl-init` needs to reach the boot partition:
  the boot device path, filesystem type, mount options, the kernel
  modules required to expose that device, and the relative path to
  the full config inside the boot partition.
- **`/boot/nmbl/config.toml`** — staged onto the boot partition by
  the install hook. Hand-editable. The full runtime schema
  (filesystems, activations, modules, TUI, paths, ...).

The Rust /init runs a new **Phase 0.5** between pseudo-fs mount and
the explicit module load: it loads the bootstrap config, brings up
its module list, populates `/dev/disk/by-*` via a `blkid` sweep,
mounts the boot partition, and loads the full `Config` from there.
The boot mountpoint is then visible to the rescue dispatcher so the
disk-rescue path can find `nmbl-rescue.sfs` against the same mount.

Operator workflow:

```bash
# edit anything in the runtime config
sudo vi /boot/nmbl/config.toml

# reboot, changes apply, no rebuild needed
sudo reboot
```

Failure handling: each Phase 0.5 failure leaves the boot mount in
place (when it got that far) so the emergency shell can fix the
on-disk config without re-flashing:

| Failure                            | Emergency shell sees |
|------------------------------------|----------------------|
| `bootstrap.toml` parse fails       | nothing mounted (build bug — needs rebuild) |
| bootstrap module load fails        | pseudo-fs only |
| boot device never appears          | pseudo-fs + diagnostic |
| boot partition mount fails         | pseudo-fs + diagnostic |
| `config.toml` missing on partition | `/mnt/boot` mounted, can `cat` directory |
| `config.toml` parse fails          | `/mnt/boot` mounted, error names the line |

Minimal example:

```nix
{
  boot.nmbl = {
    configLocation = "external";
    bootstrap = {
      configPath = "/nmbl/config.toml";
      bootFs = {
        device     = "/dev/disk/by-partlabel/disk-main-ESP";
        fstype     = "vfat";
        options    = "ro";
        mountpoint = "/mnt/boot";
      };
      kernelModules.explicit = [
        "vfat" "nls_cp437" "nls_iso8859_1" "ahci" "nvme"
      ];
    };
  };
}
```

The default `configLocation = "embedded"` keeps v1 behaviour
(full config inside the initramfs); `nmbl-init` probes for
`/etc/nmbl/bootstrap.toml` at startup and falls through to the
single-tier path when it is absent.

## External rescue

`boot.nmbl.rescue.mode` is a three-way enum:

- **`embedded`** (default) — busybox + storage activation tools live
  in the initramfs at `/bin/sh`. Legacy v1 behaviour. The emergency
  path is a bare `execve(/bin/sh, …)`.
- **`external`** — `nmbl-rescue.sfs` is built at install time from
  `boot.nmbl.rescue.squashfsContents` (default:
  `busybox-sandbox-shell`, `cryptsetup`, `lvm2`, `mdadm`) via
  `pkgs.squashfsTools` (`mksquashfs`) with zstd-19 compression. The blob is
  staged on the boot partition. The Rust /init loop-mounts it on
  demand via `LOOP_CTL_GET_FREE` + `LOOP_CONFIGURE`, then
  `switch_root`s into it (chdir → `mount --move . /` → chroot . →
  chdir /) and `execve`s `/bin/sh` from the squashfs. The initramfs
  ships no in-band shell in this mode, so the size win is real (see
  `nmbl-init-rs/PLAN.md` §13 for measured deltas).
- **`none`** — no rescue tools at all. The emergency-shell path
  prints a structured banner and halts via `reboot(RB_HALT_SYSTEM)`.

Example: external rescue with extra debug tooling.

```nix
{
  boot.nmbl.rescue = {
    mode = "external";
    squashfsContents = with pkgs; [
      busybox-sandbox-shell
      cryptsetup lvm2 mdadm
      pkgsStatic.strace
      pkgsStatic.tmux
    ];
  };
}
```

### Network fallback

`boot.nmbl.rescue.network = true` adds an HTTP/1.0 fallback for the
rescue squashfs. It bundles the configured NIC drivers
(`rescue.nicDrivers`, plus any NIC modules already required by
`hardware-configuration.nix`), enables the `network-rescue` Cargo
feature in `nmbl-init`, and turns on a ratatui flow that:

1. Brings up the first link-up interface and runs a one-shot DHCPv4
   exchange (DISCOVER → OFFER → REQUEST → ACK).
2. Applies the lease via `SIOCSIFADDR` / `SIOCSIFNETMASK` /
   `SIOCADDRT`.
3. Prompts the operator for a rescue URL (pre-filled from
   `rescue.defaultUrl`), streams the body through `Sha256` into a
   `memfd_create(2)` fd, then lets the operator confirm the
   computed hex digest against `rescue.defaultSha256`.
4. Loop-mounts the memfd and `switch_root`s into it just like the
   disk path.

Operator-confirmed SHA-256 substitutes for transport integrity, so
the implementation stays HTTP-only — no TLS / `rustls` / `openssl`.
HTTPS, IPv6, Wi-Fi, and PXE are intentionally out of scope.

```nix
{
  boot.nmbl.rescue = {
    mode    = "external";
    network = true;
    defaultUrl    = "http://rescue.lan/nmbl-rescue.sfs";
    defaultSha256 = "deadbeefcafe...";
    nicDrivers    = [ "virtio_net" "e1000e" "igb" "r8169" ];
  };
}
```

The whole network surface is conditionally compiled. With
`rescue.network = false` (the default) none of `sha2`, `dhcproto`,
or the network modules ship — the Nix store dedup keeps the
`nmbl-init` binary byte-identical between the embedded-rescue and
external-rescue-without-network configurations.

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
- Bootstrapper installation via GRUB or systemd-boot on GPT for BIOS or UEFI, the `efi-stub` direct-boot UKI (fallback path or an own path alongside another bootloader), plus QEMU `-kernel` direct invocation.
- **External configuration** on the boot partition
  (`boot.nmbl.configLocation = "external"`): tiny bootstrap.toml
  embedded in the initramfs, full config.toml staged on /boot and
  edit-and-reboot at runtime.
- **External rescue squashfs** (`boot.nmbl.rescue.mode = "external"`):
  loop-mount + switch_root into a zstd-compressed `nmbl-rescue.sfs`
  on the boot partition, with `none` as a halt-only alternative.
- **Network rescue fallback** (`boot.nmbl.rescue.network = true`):
  HTTP/1.0 download of the rescue squashfs into a `memfd`, with
  operator-confirmed SHA-256, behind the `network-rescue` Cargo
  feature.

Not supported in v1:

- `LABEL=`, `UUID=`, `PARTUUID=` filesystem specifiers in the
  operator's full config — `nmbl-init` has no udev for the runtime
  phase, so only raw `/dev/*` paths are resolved. The config loader
  rejects the others up front. (Phase 0.5's `blkid` sweep populates
  `/dev/disk/by-*` only for the bootstrap stage's own boot device.)
- LUKS unlock via FIDO2 / YubiKey / smartcard.
- MBR partition tables (only GPT is supported by the bootstrapper).

Deferred:

- A GUI menu for generation selection in graphical-console
  environments. The current TUI (`ratatui` over `/dev/console`)
  covers VT and serial, which spans every supported bootstrapper
  config.

## License

MIT License. The scripts and Rust source in this tree are MIT; the
content rendered into the initramfs (kernel, busybox, storage
tools, etc.) carries its own licenses.
