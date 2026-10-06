# sirati's NMBL, no more boot loader

NMBL uses Linux as the bootloader for NixOS. A conventional bootloader such
as GRUB or systemd-boot carries its own filesystem drivers, its own disk
stack and its own scripting language. Linux already has drivers for every
filesystem and storage stack it can run on, so NMBL boots Linux with Linux.

NMBL boots a small pinned kernel and a minimal initramfs. Its PID 1 mounts
the target system, lets the operator pick a NixOS generation and then
`kexec`s into it. Because a full kernel mounts the real root with the real
driver, two bootloader limits go away:

- Any storage stack Linux can mount is bootable. This includes LVM, LUKS,
  mdraid, ZFS and Btrfs subvolumes.
- The boot partition does not need a copy of every generation's kernel and
  initrd. NMBL reads them in place from
  `/nix/var/nix/profiles/system-N-link/` on the target system.

The boot partition holds the NMBL kernel and initramfs (or a UKI). With
external configuration, external rescue or signed EROFS generations it also
holds NMBL's runtime config and images.

PID 1 in the initramfs is `nmbl-init`, a static musl Rust binary.

## What's in the initramfs

| Path | Purpose |
|------|---------|
| `/init` | `nmbl-init`, the static Rust PID 1. |
| `/etc/nmbl/config.toml` | Runtime config rendered by `lib/config-toml.nix` (with `configLocation = "embedded"`). |
| `/etc/nmbl/bootstrap.toml` | Small bootstrap config that locates the full config on the boot partition (with `configLocation = "external"`). |
| `/bin/blkid` | util-linux `blkid`. `nmbl-init` runs it to create `/dev/disk/by-*` links, because the initramfs has no udev. |
| `/bin/sh` | busybox, for the emergency menu's Raw Shell. Present for `rescue.mode = "embedded"` and `"external"`, absent for `"none"`. |
| `/lib/modules/<ver>/...` | The kernel module closure. |
| `/etc/modprobe.d/nixos.conf` | Module blacklist. |
| `/etc/splash/font.ttf`, `/etc/splash/image.png` | Splash font and background, only with `boot.nmbl.splash.enable`. The PNG moves to the boot partition with `splash.backgroundLocation = "boot-partition"`. |

Storage tools (`cryptsetup`, `lvm2`, `mdadm`, `zfs`) are added only when
`boot.nmbl.activation.*` needs them. The build prefers the `pkgsStatic`
variant of each tool and falls back to the dynamic one with a warning. A
LUKS volume with `unlock = "tpm"` needs the dynamic `cryptsetup`, because
the static build cannot load the `systemd-tpm2` token plugin.

## How nmbl-init works

`nmbl-init` mounts filesystems, loads modules and starts the next kernel with
direct system calls: `mount(2)`, `init_module(2)` and `kexec_file_load(2)`.
It decompresses `.ko.xz`, `.ko.zst` and `.ko.gz` modules itself with pure
Rust decoders. The initramfs contains no `kmod` and no `kexec-tools`. It runs external programs for three jobs: `blkid` for device
links, the storage tools for activation, and a shell or rescue system when
the operator asks for one.

The crate denies `unwrap`, `expect`, `panic!`, raw indexing, `todo!`,
`unimplemented!`, `unreachable!` and `dbg!` through clippy lints in
`nmbl-init-rs/Cargo.toml`. A panic hook still exists. It writes a report and
re-executes `/proc/self/exe` with `--errored=<path>`, and the new process
shows the emergency menu with the report attached.

Optional code sits behind Cargo features. The Nix build turns each one on
from your options (`lib/signing-build.nix`):

| Feature | Enabled by |
|---------|------------|
| `pretty-shell` | Always (default feature). An in-TUI terminal for the emergency shell. |
| `image-splash` | `splash.enable` |
| `network-rescue` | `rescue.network` |
| `remote-tui` | `rescue.fullSystem.enable` |
| `rescue-stages` | `rescue.mode = "external"` with `rescue.fullSystem.enable` |
| `stateful` | `stateful.enable` |
| `secure-boot` | Any signing, TPM measurement, secure-boot, driver-image or generation-image setting |
| `staged-boot` | `staged.enable` |

`nmbl-init-rs/PLAN.md` describes the boot phases and the crate layout in
detail, and `ARCHITECTURE.md` gives the longer overview.

## Quick start

This command starts a VM that boots GPT, UEFI and GRUB into NMBL:

```bash
nix run .#test-gpt-uefi-grub
```

Other prebuilt test configurations:

```bash
nix run .#test-gpt-bios                    # legacy BIOS through GRUB
nix run .#test-gpt-uefi-systemd            # systemd-boot
nix run .#test-gpt-qemu-kernel-invoke      # QEMU -kernel direct boot
nix run .#test-gpt-qemu-kernel-invoke -- --debug-shell
nix run .#test-external-config             # config.toml on /boot
nix run .#test-external-rescue             # rescue image on /boot
nix run .#test-external-rescue-network     # rescue with the HTTP fallback
nix run .#test-stateful                    # stateful boot tracking
nix run .#test-external-splash-bg          # splash PNG on the boot partition
nix run .#test-gpt-uefi-grub-luks-password # LUKS passphrase unlock
nix run .#test-secure-boot                 # signed generations and measured boot
nix run .#test-secure-boot-tpm-roundtrip   # TPM seal and unseal round trip
nix run .#test-secure-boot-driver-image    # signed driver image
nix run .#test-secure-boot-staged          # staged config fragment behind LUKS
nix run .#test-rescue-ssh                  # full-system rescue over SSH
nix run .#test-network-stage-vm            # staged rescue and network stage
```

Each test configuration also has a `tmux-serial-<name>` app that runs the VM
in a tmux session. `flake.nix` lists every app under `apps` and the pure
build checks under `checks`.

The VM runners use `vm-serial-man`, which exposes the serial console to other
shells:

```bash
vm-serial-man status
vm-serial-man send 'ls /mnt/system/nix/var/nix/profiles'
vm-serial-man send $'\x1b[B'   # arrow keys work too
vm-serial-man stop
```

With more than one VM running, pass `--socket /tmp/vm-serial-man-<pid>.sock`
to `status` and `stop` so they act on one VM.

To build the initramfs of a test configuration:

```bash
nix build .#nixosConfigurations.test-gpt-uefi-grub.config.system.build.nmblInitramfs
```

`nix build .#debugInfo.<name>` writes a text file that lists the
configuration's filesystems, modules and other NMBL settings.

## Recommended setups

External configuration and external rescue are independent options. Pick
the combination that fits how you recover the machine:

| Profile | `configLocation` | `rescue.mode` | `rescue.automatic` | `rescue.network` | Use |
|---------|------------------|---------------|--------------------|------------------|-----|
| Default install | `embedded` | `embedded` | `false` | `false` | Desktop or laptop. Everything ships in the initramfs. |
| Workstation | `external` | `external` | `false` | `false` | Edit the config on `/boot` and reboot. The rescue tools live on the boot partition. |
| Server | `external` | `external` | `true` | optional | Headless machines. Add `rescue.fullSystem` for SSH into the rescue. A failed boot enters the rescue without input. |
| Appliance | `embedded` | `none` | `false` | `false` | Recovery happens out of band. NMBL prints a banner and halts. |

All profiles boot the same `nmbl-init` source. Only the enabled features, the
initramfs contents and the files on the boot partition differ.

For verified boot, add `boot.nmbl.signing`, `boot.nmbl.tpm` and
`boot.nmbl.secureBoot` to any profile. See
[Verified loading and measured boot](#verified-loading-and-measured-boot).

## NixOS module options

Most installs set a few options. `lib/options.nix` defines the core options.
`lib/modules/` and `lib/modules/security/` define the rest.

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
    instantBoot.enable = false;   # skip the countdown after a healthy boot
    serialConsole = "ttyS0,115200";  # null uses the video console

    kernelModules = [ "nvme" "ahci" ];   # loaded explicitly at boot
    blacklistedKernelModules = [ ];

    # Storage activation, only for what you use:
    activation.lvm.enable = true;
    activation.mdraid.enable = true;
    activation.zfs.pools = [ "rpool" ];
    activation.luks = [
      { name = "cryptroot"; device = "/dev/nvme0n1p3"; unlock = "password"; }
    ];
  };
}
```

Notes on these options:

- `activation.lvm.enable` defaults to on when a filesystem device is under
  `/dev/mapper/` and `activation.luks` is empty. `activation.mdraid.enable`
  defaults to on when a filesystem device is under `/dev/md`.
  `activation.zfs.pools` defaults to `[ "rpool" ]` when a filesystem has
  `fsType = "zfs"`.
- `activation.luks` is a list. Each entry's `unlock` is `tpm` (a TPM-sealed
  token in the LUKS header), `keyfile` (a key file bundled into the
  initramfs) or `password` (typed into the TUI).
- For `password` and `tpm` entries, `passToStage1` defaults to
  `/etc/nmbl-luks/<name>`. NMBL passes the secret into the kexec'd initrd at
  that path, and the NixOS stage 1 unlocks the volume with it. The operator
  types the passphrase once. Set `passToStage1 = null` to turn this off.
- The default NMBL kernel is `pkgs.linux_6_6`. An assertion rejects it
  together with LUKS, because dm-crypt cannot create the mapping on that
  series. Set `boot.nmbl.kernelPackage` to a newer kernel for LUKS hosts.
  This option sets only NMBL's kernel. The target system's kernel is
  independent.
- `verbose` defaults to `boot.initrd.verbose`. `verbosity` (`quiet`, `info`
  or `verbose`) is the runtime setting it maps to.
- `earlyKernelModules` load before the boot console opens. Use them for DRM
  drivers the splash needs. `kernelModules` load after the console is up.
  NMBL also loads the filesystem drivers it derives from
  `fileSystems.*.fsType`.

Other options:

| Option | Default | Effect |
|--------|---------|--------|
| `timeoutMillis` | `timeoutSeconds * 1000` | Selector countdown in milliseconds, for sub-second delays. |
| `deviceTimeoutSeconds` | `30` | How long NMBL waits for each device. See [Device timeout](#device-timeout). |
| `emergencyTimeoutSecs` | `null` (30 s built in) | Auto-reboot countdown on the emergency screen. |
| `ignoreMissingDiskModules` | `false` | Skip the build check that storage drivers for your disks are in the initrd module lists. |
| `refuseInvalidHardwareOnInstall` | `true` | Abort the install when `nmbl-init --validate-hardware` finds a missing device or LUKS header. With `false` the installer prints a warning and continues. |
| `kernelParams` | `[ ]` | Command line of the NMBL kernel. The target generation keeps its own. |
| `tui.enableEditor`, `tui.showKernelParams` | `true` | Command-line editor and display in the selector. |
| `emergencyShell.extraConsoles` | `[ ]` | Extra `/dev/<tty>` devices the emergency shell may run on. |
| `bootstrapper.bootDisks` | `[ ]` | Install targets. When empty, BIOS installs GRUB on every disk with an EF02 partition, and UEFI uses the mounted `/boot` ESP. |

### Device timeout

`boot.nmbl.deviceTimeoutSeconds` (default `30`) is the wait for each device.
NMBL waits this long for each `fileSystems.<name>.device` to appear during the
mount of the target system. It also waits this long for the devices that a
cryptsetup, LVM or mdraid activation creates. Raise it for slow USB
enclosures or controllers.

### Sealing a LUKS volume to the TPM

For `unlock = "tpm"` volumes, the host tool `nmbl-tpm-enroll` seals the
volume key to the TPM. It is installed on the booted system and is not part
of the initramfs. It wraps `systemd-cryptenroll`. Run it once after the
first boot of the installed system:

```sh
# Seal to PCRs 11 and 7 (the default). PCR 11 is predicted for the NMBL UKI
# that will unlock the volume.
sudo nmbl-tpm-enroll --device /dev/disk/by-partlabel/disk-main-luks \
  --uki /boot/EFI/BOOT/BOOTX64.EFI

# Or predict on the build host (no TPM or device needed), then seal to the
# printed digest on the target:
nmbl-tpm-enroll --uki result/BOOTX64.EFI --print-pcrs   # prints 11:sha256=<hex>+7
sudo nmbl-tpm-enroll --device /dev/disk/by-partlabel/disk-main-luks \
  --pcrs "11:sha256=<hex>+7"
```

Pass `--uki` or a literal PCR 11 digest. NMBL unseals during storage
activation, before it extends PCR 11 with its own handoff. At that moment
PCR 11 holds only systemd-stub's measurement of the NMBL UKI. The running
system sees a later PCR 11 value that includes NMBL's handoff. A token
sealed to that later value (plain `--pcrs 11+7` without `--uki`) does not
unseal at boot. `--uki` predicts the unlock-time value with
`systemd-measure calculate`.

The round trip has four steps:

1. Enroll on the host, once. `nmbl-tpm-enroll` runs
   `systemd-cryptenroll --tpm2-device=auto --tpm2-pcrs=11:sha256=<predicted>+7 <device>`.
   This adds a LUKS2 keyslot whose key is sealed to PCRs 11 and 7, and
   writes a `systemd-tpm2` token into the LUKS2 header. PCR 11 is NMBL's
   measure PCR (`boot.nmbl.tpm.pcrIndex`). PCR 7 holds the firmware's
   Secure Boot state.
2. Unlock at boot. NMBL runs `cryptsetup open --token-only <device> <name>`.
   libcryptsetup reads the token and unseals the key without a prompt, if
   PCRs 11 and 7 still have their enrolled values.
3. Hand off to stage 1. The kexec drops NMBL's dm-crypt mapping, and stage 1
   cannot unseal the token because PCR 11 has moved. Right after the unseal,
   NMBL reads the token passphrase with `nmbl-tpm-passphrase` and passes it
   to stage 1 as the `passToStage1` key file.
4. Rescue or tampering. NMBL extends PCR 11 with a poison value before it
   enters any rescue. A changed kernel or initrd, or firmware that stopped
   enforcing Secure Boot, changes PCR 11 or PCR 7. In each case the unseal
   fails and NMBL shows the passphrase prompt.

Keep a passphrase keyslot for recovery. After a change to the measured
inputs (a new NMBL UKI, or a firmware update that changes PCR 7), run
`nmbl-tpm-enroll --wipe-existing --uki <new UKI> ...` again. Use `--pcrs` if
your policy differs from `11+7`. NMBL warns at build time when an
`activation.luks` entry lists `tpmPcrs` without both PCR 11 and PCR 7.

### efi-stub direct boot

With `loader = "efi-stub"` (UEFI only), the build combines NMBL's kernel and
initrd into one UKI and writes it to the ESP. No GRUB or systemd-boot binary
is installed. The default path is `EFI/BOOT/BOOTX64.EFI`, which firmware
boots without an NVRAM entry. This suits a dedicated NMBL disk or an
uploaded image.

To install next to an existing bootloader without overwriting its fallback
binary, give the UKI its own path:

```nix
{
  boot.nmbl.bootstrapper = {
    bootMode = "uefi";
    loader   = "efi-stub";
    loader_extra_args = {
      efiStubInstallPath   = "EFI/nmbl/nmbl.efi";
      canTouchEfiVariables = true;   # register the NVRAM entry
    };
  };
}
```

Firmware does not boot an own path by itself, so the installer registers a
UEFI boot entry named `NMBL` and puts it first in BootOrder. The existing
bootloader's entry stays as a fallback. This requires
`canTouchEfiVariables = true`. With `false`, the installer writes the file
and prints a warning that tells you to add the entry by hand.

## Logging

`nmbl-init` keeps its log lines in a 1 MiB ring buffer. Before kexec, reboot,
`execve` or halt, it writes the ring to `/nmbl-log/nmbl.log` and calls
`fsync`. NMBL adds this file to the kexec'd initrd as an extra cpio archive.

The booted system imports it with the `nmbl-log-import` unit, tagged
`nmbl-init`. With the systemd initrd, the unit runs in stage 1 before
`initrd-switch-root.target`. With the scripted initrd, stage 1 copies the file
to the root filesystem and a stage-2 unit imports it after journald starts.

```
journalctl -b -t nmbl-init
```

If the ring overflowed, the imported log starts with
`=== nmbl-init: log truncated, earlier <N> bytes dropped ===`.

The unit runs `nmbl-log-import` from nmbl-host-tools. It reads at most 2 MiB
(keeping the newest lines, with the same header if it cut any), escapes
control characters, invalid UTF-8 and backslashes as `\xNN`, `\u{N}` and
`\\`, caps each line at 4 KiB, and sends it over journald's native socket.
If journald refuses a line, the line goes to `/dev/kmsg` instead. The file is
kept if a line could not be logged.

`nix run .#check-log-import` (scripted initrd) and
`nix run .#check-log-import-systemd` check this path in a VM.

## Stateful boot tracking

`boot.nmbl.stateful.enable = true` makes NMBL record which generations
booted successfully, and roll back after failed boots. NMBL keeps a 16 KiB
CBOR file, `state.bin`, in `stateful.stateDir` on the boot partition. It
writes the file through a read-write mount of the boot partition at
`stateful.rwMountpoint`.

| Option | Default | Effect |
|--------|---------|--------|
| `stateful.maxRecoveryAttempts` | `5` | Rollbacks NMBL tries before the failure is final. |
| `stateful.successTarget` | `multi-user.target` | The systemd target that marks a boot as successful. |
| `stateful.stateDir` | `/boot/nmbl` | Directory of `state.bin`, as seen from the booted system. |
| `stateful.rwMountpoint` | `/mnt/boot-state` | Where NMBL mounts the boot partition read-write. |

The install hook runs `nmbl-init --init-state <stateDir>`. It creates
`state.bin` when it is missing and validates an existing file without
rewriting it. A unit named `nmbl-boot-succeeded` runs
`nmbl-init --boot-succeeded <stateDir>` once the system reaches
`successTarget`.

At boot, NMBL checks whether the previous boot reached the success target.
If it did not, NMBL boots a known-good generation from a list of the last 20
good generations. After `maxRecoveryAttempts` failed rollbacks the failure
is final, and `rescue.automatic` decides what happens next. See
[Automatic rescue after a failed boot](#automatic-rescue-after-a-failed-boot).

Fields added after the first version of `state.bin` have serde defaults, so
an older `nmbl-init` reads a newer file. If the file has a
`state_format_version` newer than the binary knows, NMBL logs a warning and
boots without state tracking.

## Instant boot and nmblctl

`boot.nmbl.instantBoot.enable` skips the selector countdown when the last
boot succeeded, no rollback or rescue is pending, and no key was pressed
during early boot. It needs `stateful.enable` or a signed `generationImage`
as a source of boot health.

`nmblctl` is a root-only tool on the booted system. It shows the chain of
boot stages and NMBL's handover, and it sets the default generation or a
one-shot choice for the next boot. See [docs/nmblctl.md](docs/nmblctl.md).

## Signed EROFS generations

`boot.nmbl.generationImage` boots the Nix store of a generation from a
signed EROFS image under `/boot/nmbl-generations`. NMBL verifies the image
before kexec, and the target initrd verifies it again. With
`generationImage.automaticRollback`, an untested generation rolls back to
its tested predecessor when its boot does not complete. The tools `nmbl-erofsctl`, `nmbl-erofs-deploy` and
`nmbl-erofs-receive` build, deploy and switch these images. See
[docs/erofs-generations.md](docs/erofs-generations.md).

`boot.nmbl.bootUpdate` adds an authenticated A/B updater for the boot set,
with a separate unprivileged service user. See
[docs/boot-set-updates.md](docs/boot-set-updates.md).

## External configuration

By default the build embeds the runtime TOML into the initramfs, so a change
to any NMBL setting needs a rebuild. With
`boot.nmbl.configLocation = "external"` the config has two parts:

- `/etc/nmbl/bootstrap.toml` is in the initramfs. It holds what `nmbl-init`
  needs to reach the boot partition: the device, filesystem type, mount
  options, kernel modules and the path of the full config.
- `config.toml` on the boot partition (default path `/nmbl/config.toml`,
  relative to the boot partition) holds the full runtime config.

`nmbl-init` reads `bootstrap.toml` early, loads its modules, runs `blkid` to
create `/dev/disk/by-*` links, mounts the boot partition and loads
`config.toml` from it. The rescue code uses the same mount to find the
rescue image.

Without signing, you can edit the file and reboot:

```bash
sudo vi /boot/nmbl/config.toml
sudo reboot
```

With signing enabled, NMBL verifies the external config against a
signature, so an edited file needs a new signature. See
[docs/external-config-signing.md](docs/external-config-signing.md).

When a step fails, NMBL keeps whatever it already mounted, so the emergency
shell can fix the file on disk:

| Failure | Emergency shell sees |
|---------|----------------------|
| `bootstrap.toml` does not parse | Nothing mounted. This is a build bug and needs a rebuild. |
| A bootstrap module fails to load | Pseudo-filesystems only. |
| The boot device does not appear | Pseudo-filesystems and a diagnostic. |
| The boot partition does not mount | Pseudo-filesystems and a diagnostic. |
| `config.toml` is missing | `/mnt/boot` is mounted. |
| `config.toml` does not parse | `/mnt/boot` is mounted, and the error names the line. |

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

These values are the defaults. With `configLocation = "embedded"`, the
initramfs has no `bootstrap.toml` and `nmbl-init` reads
`/etc/nmbl/config.toml`.

## External rescue

`boot.nmbl.rescue.mode` has three values:

- `embedded` (default). busybox and the storage tools are in the
  initramfs. The rescue `execve`s `/bin/sh`, which replaces NMBL.
- `external`. The rescue image lives on the boot partition at
  `rescue.sfsPath` (default `nmbl-rescue.sfs`). NMBL opens it, verifies its
  signature when signing is enabled, loop-mounts it read-only under a tmpfs
  overlay at `/rescue`, and runs its entrypoint as a chrooted child. NMBL
  stays PID 1. It reboots when the rescue exits.
- `none`. No rescue tools ship. NMBL prints a banner and halts.

Before NMBL enters any rescue, it extends the TPM lock PCR with a poison
value and closes every TPM-unsealed LUKS mapping. If that fails, NMBL takes
the refuse path described in
[The priority-file gate](#the-priority-file-gate).

### The flat rescue

With `rescue.fullSystem.enable = false`, the external rescue is a squashfs
built from `rescue.squashfsContents` (default `busybox-sandbox-shell`,
`cryptsetup`, `lvm2` and `mdadm`) with zstd level 19. NMBL runs its
`/bin/sh`.

```nix
{
  boot.nmbl.rescue = {
    mode = "external";
    squashfsContents = with pkgs; [
      busybox-sandbox-shell
      cryptsetup lvm2 mdadm
      pkgsStatic.strace
    ];
  };
}
```

### Full-system rescue over SSH

`boot.nmbl.rescue.fullSystem.enable = true` replaces the busybox tree with a
recovery system that starts its own network and `sshd`. NMBL stays PID 1
outside it, and its own root is visible at `/nmbl-root`.

The rescue starts in two stages. [docs/rescue-stages.md](docs/rescue-stages.md)
describes them in detail.

- Stage 1 is NMBL's own initramfs. It carries `erofs.ko`, `loop` and
  `overlay`. Its `config.toml` names the stage-2 image and pins its SHA-512
  in `[rescue.image]`.
- Stage 2 is an EROFS image with the recovery system and the NIC drivers
  and firmware for NMBL's kernel. NMBL verifies the signature and the
  SHA-512 pin over one file descriptor, then loop-mounts that descriptor.
  A validly signed image with a different digest is refused.

The stage-2 image does not depend on the host. The host's settings live in
the `[rescue.system]` table of NMBL's config: the sshd port, the authorized
keys, the host key path, the modules to load and the network profile. NMBL
validates them at rescue entry and writes them to `/etc/nmbl-rescue/` in the
rescue overlay. A host configuration change therefore does not rebuild the
image. Two hosts with the same NMBL kernel and rescue packages share one
image.

`nmblctl` is built with the host's signing public keys, so it ships in a
separate small EROFS at `fullSystem.toolsImagePath` (default
`nmbl/rescue-tools.erofs`). `[rescue.tools]` pins it, and NMBL checks it
like the stage-2 image before it mounts it at `/nmbl-tools`. The rescue puts
`nmblctl` on `PATH`. If NMBL refuses the tools image, the rescue runs without
`nmblctl`.

| Option | Default | Effect |
|--------|---------|--------|
| `fullSystem.minimal` | `false` | Small profile without Nix, CA certificates, btop, cryptsetup, LVM or e2fsprogs. |
| `fullSystem.compression` | `"lz4hc"` | EROFS compressor: `lz4hc`, `zstd` or `none`. `zstd` needs an NMBL kernel of 6.10 or later, and an assertion enforces this. |
| `fullSystem.packages` | Profile packages and storage tools | Packages in the image. |
| `fullSystem.firmware` | `[ ]` | Firmware packages. The image gets only the blobs its modules request. |
| `fullSystem.sshdPort` | `22222` | sshd port, passed in `[rescue.system]`. |
| `fullSystem.rootAuthorizedKeys` | `[ ]` | Root SSH keys, passed in `[rescue.system]`. Without keys, remote login is off. |
| `fullSystem.hostKeyPath` | `null` | Persistent Ed25519 host key in NMBL's namespace. Without it, the rescue generates a host key per boot. |
| `fullSystem.identityVolume` | `null` | A plaintext filesystem with the SSH identity, mounted read-only before rescue. |
| `fullSystem.toolsImagePath` | `"nmbl/rescue-tools.erofs"` | Path of the `nmblctl` tools image on the boot partition. |
| `fullSystem.networkStage.*` | off | A separate signed networking EROFS stage. See [docs/network-stage.md](docs/network-stage.md). |

The default package list includes storage tools only for the stacks the
host uses. `cryptsetup` comes only with LUKS, `btrfs-progs` only with Btrfs,
`lvm2` only with LVM activation, and `mdadm` only with mdraid.

An interactive SSH login shows this message after authentication:

```text
NMBL rescue - run `nmbl` to enter the bootloader.
```

`nmbl` is NMBL's own binary, which the rescue bind-mounts at `/bin/nmbl`.
Run as a normal process, it connects to NMBL's root-only socket at
`/nmbl-root/nmbl-run/tui.sock` and shows NMBL's menu remotely. From there
the operator can boot a generation, retry or reboot. The session survives a
slow link. It ends on Ctrl+E, or when the client or its SSH connection goes
away. Pasted text reaches the menu whole, on the console and in remote
sessions.

When the operator commits an action in `nmbl`, NMBL stops every rescue
process, including SSH sessions, syncs and unmounts the rescue, and then
boots the chosen generation, retries or reboots.

### Automatic rescue after a failed boot

`boot.nmbl.rescue.automatic` decides what happens when a boot fails and NMBL
has nothing left to fall back to:

| `rescue.automatic` | Outcome |
|---|---|
| `true` | NMBL enters the configured rescue without input. |
| `false` (default) | NMBL opens the interactive emergency menu. |

The setting applies to every such failure: a failed tested generation
(`generationImage`), exhausted stateful rollbacks (`stateful`), and any
failure during a boot phase (mount, storage activation, generation scan,
kexec, config load or panic). Some rules around it:

- `rescue.mode` chooses which rescue NMBL enters. An assertion rejects
  `rescue.automatic = true` with `rescue.mode = "none"`.
- Rollback comes first. A failed untested generation rolls back to its
  tested predecessor, and stateful tracking tries its known-good
  generations, before NMBL treats the boot as failed.
- Operator choices (reboot, aborting a device wait, leaving a wrong-password
  shell) return to the menu. A rescue that fails falls back to the menu.
- After stateful recovery is exhausted, root on the booted or rescue system
  can allow one more attempt of a generation with
  `nmblctl retry-generation --generation N`. The failure counters stay as
  they are.

Unattended servers should set `rescue.automatic = true`. This option
replaces the former `generationImage.automaticRescue`.

Security refusals follow their own path. A bad or missing signature, the
priority-file gate or a failed TPM seal takes the refuse path described in
[The priority-file gate](#the-priority-file-gate), whatever
`rescue.automatic` says.

### Network fallback

`boot.nmbl.rescue.network = true` adds an HTTP/1.0 download for the rescue
image when the copy on disk cannot be used. The build adds the NIC drivers
from `rescue.nicDrivers` (default `virtio_net`, `e1000e`, `igb` and `r8169`),
the NIC modules from `hardware-configuration.nix` and `af_packet`. It also
enables the `network-rescue` Cargo feature. NMBL then:

1. Brings up the first interface with a link and runs a DHCPv4 exchange.
2. Applies the lease with `SIOCSIFADDR`, `SIOCSIFNETMASK` and `SIOCADDRT`.
3. Asks for a URL (pre-filled from `rescue.defaultUrl`) and streams the
   body through SHA-256 into a sealed `memfd`.
4. With signing enabled, downloads `<url>.sig` and verifies the image under
   the `rescue-sfs` domain. Under enforcement a missing or bad signature
   rejects the image. In audit mode NMBL warns and continues.
5. If the config pins the stage-2 SHA-512, requires that exact image. A
   different image boots only when the operator presses an upper-case `U` on
   the warning screen.
6. Shows the computed SHA-256 next to `rescue.defaultSha256` for the
   operator to confirm.
7. Loads `loop`, the image filesystem and `overlay`, loop-mounts the
   `memfd` and runs it like the disk rescue.

NMBL mounts no image that fails these checks. The download uses plain HTTP,
and the checks above provide the integrity. NMBL has no TLS, IPv6, Wi-Fi or
PXE support in this path.

```nix
{
  boot.nmbl.rescue = {
    mode    = "external";
    network = true;
    defaultUrl    = "http://rescue.lan/nmbl-rescue.sfs";
    defaultSha256 = "<64 hex digits>";
  };
}
```

With `rescue.network = false` (the default), the binary contains none of
the network code.

## Graphical splash

By default the TUI runs on a text console (`/dev/console`, a VT or a serial
port). With `boot.nmbl.splash.enable = true`, NMBL draws the same `ratatui`
menu on a DRM framebuffer over a PNG background. Both outputs use the same
state machine, so every menu works the same way.

When there is no `/dev/dri/card*`, or a DRM, font or framebuffer step fails,
NMBL falls back to the text TUI. The splash code is behind the
`image-splash` Cargo feature.

| Option | Type | Default | Effect |
|--------|------|---------|--------|
| `splash.enable` | bool | `false` | Draw the menu on a DRM framebuffer. |
| `splash.backgroundImage` | path | The cosmic-greeter background, converted to PNG | Background image (RGBA8 PNG). |
| `splash.backgroundLocation` | `"initrd"` or `"boot-partition"` | `"initrd"` | Embed the PNG in the initramfs, or store it as `nmblsplash.png` on the boot partition. The second needs `configLocation = "external"`. If the file is missing, NMBL draws a solid background. |
| `splash.font.{package,dir,variant}` | | Adobe Source Code Pro Regular (OTF) | The monospace face. The build fails and lists the available faces if the file does not exist. |

```nix
{
  boot.nmbl.splash = {
    enable = true;
    backgroundImage = ./wallpaper.png;
  };
}
```

The splash uses `simpledrm` or the EFI framebuffer by default. A board whose
GPU driver replaces `simpledrm` (virtio-gpu, amdgpu and others) must load
that driver through `boot.nmbl.earlyKernelModules`. The splash has no GPU
acceleration and no animation.

## Driver images

`boot.nmbl.driverImages` ships out-of-tree kernel modules and their firmware
in signed squashfs images on the boot partition. Before the handoff, NMBL
finds each image, verifies its detached signature against the baked keys,
loop-mounts it read-only, registers its `lib/firmware`, and loads the listed
modules in order.

Driver images require an active signing configuration. A bad or missing
signature refuses the boot.

| Option | Type | Default | Effect |
|--------|------|---------|--------|
| `driverImages.enable` | bool | `false` | Turns driver images on. Requires signing. |
| `driverImages.images.<name>.modules` | list of str | `[ ]` | Module names to load, in dependency order. |
| `driverImages.images.<name>.firmware` | list of package | `[ ]` | Firmware in the image's `/lib/firmware`. |
| `driverImages.images.<name>.blacklist` | list of str | `[ ]` | In-tree modules to blacklist first, for example `nouveau`. |
| `driverImages.images.<name>.path` | str | `nmbl/driver-<name>.sfs` | Image path relative to the boot partition. |
| `driverImages.images.<name>.sigPath` | str | `<path>.sig` | Signature path. |

The build creates each image without a key. The installer signs it with
`nmbl-sign` and `signing.imageKeyFile` or `signing.imageKeyCommand`.

## Staged boot

Staged boot puts a signed config fragment and a driver image on the
encrypted priority volume. NMBL reads them only after it unlocks storage.
The priority gate mounts the volume and verifies it. NMBL then verifies the
fragment and the image, merges the fragment onto the base config as one
transaction, and runs the merged config before kexec. The merged config can
load more modules, driver images and storage activations. The early config
stays small, and the fragment is not readable without the LUKS key.

| Option | Type | Default | Effect |
|--------|------|---------|--------|
| `staged.enable` | bool | `false` | Apply the fragment after unlock. Requires `secureBoot.enable`. |
| `staged.image` | str | `nmbl-staged.img` | Driver squashfs, relative to the priority volume. |
| `staged.fragment` | str | `nmbl/fragment.toml` | Signed config fragment, relative to the priority volume. |
| `staged.sig` | str | `nmbl/fragment.toml.sig` | Detached signature of the fragment. |
| `bootstrap.staged.{mountpoint,fragment,sig}` | str | `/mnt/staged`, `nmbl/fragment.toml`, `nmbl/fragment.toml.sig` | The same paths for the bootstrap stage. |

A fragment may change modules, activations, filesystems, TPM, rescue and
driver-image tables. The parser rejects a fragment that touches the
signing, secure-boot or staged tables. On any failure (bad signature,
unparseable fragment, failed run), NMBL restores the base config and refuses
into rescue with the TPM locked.

## Verified loading and measured boot

On top of UEFI Secure Boot, NMBL can verify everything it loads with
post-quantum signatures, measure the handoff into the TPM, and lock the TPM
before any shell or rescue.

### Generation signing

With `boot.nmbl.signing.enable` and `signing.enforce`, NMBL refuses to
`kexec` a generation unless its kernel and initrd carry a valid signature
from a public key compiled into `nmbl-init`. Signatures use FIPS 204 ML-DSA
(ML-DSA-65 by default, ML-DSA-87 optional) over each file's SHA-512 digest.
They are stored in a detached `.sig` file and bound to a domain tag per
role. NMBL tries every baked key of the matching algorithm. No option allows
unsigned generations. The keys are compiled into the binary, so editing the
config or the boot partition cannot replace them.

`signing.enable` without `enforce` is audit mode: NMBL logs bad signatures
and boots anyway. An assertion requires `secureBoot.allowAuditModeInsecure`
for audit mode.

The installer signs generations with `nmbl-sign` and
`signing.generationKeyFile`. That option holds a path string. An assertion
fails the build if a key path resolves into the Nix store. This keeps the
private key out of every derivation. `signing.deferInstallSigning` skips
the signing step in the installer for sealed disk-image builds. Runtime
enforcement stays on.

The key does not need to exist as a file. Set
`signing.generationKeyCommand` (and `imageKeyCommand` for driver, rescue, tools and
network-stage images) to a command whose standard output is the private
key. The installer runs it once per signature and pipes the key into
`nmbl-sign sign --key-stdin`. Nothing is written to disk. A secrets store
works directly:

```nix
boot.nmbl.signing.generationKeyCommand = [ "nix-secrets" "pipe-secret" "nmbl-generation-key" ];
```

`nmbl-sign keygen --alg ml-dsa-65 --stdio` creates a key pair the same way.
It writes the private key to stdout and the raw public key to fd 3. See
[Pipe-only signing keys](#pipe-only-signing-keys).

| Option | Type | Default | Effect |
|--------|------|---------|--------|
| `signing.enable` | bool | `false` | Compile the verifier into `/init`. |
| `signing.enforce` | bool | `false` | Refuse into rescue on a bad or missing signature. |
| `signing.publicKeys` | list of path | `[ ]` | ML-DSA public keys compiled into the binary. |
| `signing.algorithm` | `"ml-dsa-65"` or `"ml-dsa-87"` | `"ml-dsa-65"` | Signature algorithm. |
| `signing.sigPathSuffix` | str | `".sig"` | Suffix of signature files. |
| `signing.generationKeyFile` | null or path | `null` | Install-time private key path. |
| `signing.generationKeyCommand` | null or list of str | `null` | Command that prints the private key. |
| `signing.imageKeyFile`, `signing.imageKeyCommand` | | `null` | The same pair for driver, rescue, tools and network-stage images. |
| `signing.uki.enable` | bool | `false` | Sign NMBL's UKI at install time with a key enrolled in the firmware `db`. |
| `signing.uki.keyFile`, `signing.uki.certFile` | null or path | `null` | The `db` key and certificate, read at install time. |
| `signing.uki.refuseInstallIfNotEnforcing` | bool | `false` | Abort the install when the firmware would boot an unsigned UKI. |

### Pipe-only signing keys

`nmbl-sign` can create and use ML-DSA keys without a private-key file:

```console
# private key to stdout, raw public key to fd 3; fails if fd 3 is closed
nmbl-sign keygen --alg ml-dsa-65 --stdio > >(store-secret nmbl-key) 3> nmbl.pub

# private key from stdin (bounded, zeroized); the input must be a file path
print-secret nmbl-key | nmbl-sign sign --key-stdin --domain gen-kernel kernel --out kernel.sig
```

`--key-stdin` refuses a terminal on stdin. It also refuses `-`,
`/dev/stdin` and any input path that is the same file as stdin, because the
key and the payload cannot share one stream. The key-file forms
(`--out-priv`, `--out-pub` and `--key <FILE>`) still work.

The operator tools also accept `-` as the private key. `nmbl-erofs-deploy`
and `nmbl-erofsctl prepare` then run `NMBL_SIGN_KEY_COMMAND` (a shell
command line) once per signature. `nmbl-boot-update prepare A|B SRC OUT - PUB`
reads the key from stdin once for the whole slot.

`nmbl-sign sign-digests` signs a deployment from its digests. It reads one
JSON request of at most 64 KiB from stdin. The request carries the private
key, the SHA-256 of the matching public key, and the SHA-512 and size of each
artifact. The artifact roles are `generation-image`, `boot-config`,
`gen-kernel`, `gen-initrd` and `rescue-sfs`, plus an optional
`network-stage`. It prints the signatures as JSON and prints nothing if any
check fails.

The UEFI Secure Boot `db` key (`signing.uki.keyFile`) is an RSA PEM key for
`sbsign` and stays file-based.

### Measured boot and the TPM lock

With `boot.nmbl.tpm.measure`, NMBL extends PCR 11 with the handoff after the
signature check and before `kexec`. The measurement covers an NMBL marker,
the kernel digest, the initrd digest, the kexec command line and each
verified driver image. A LUKS key sealed to PCR 11 and PCR 7 then unseals
only when that exact image boots. NMBL has no TPM seal or unseal code of its
own. `systemd-cryptenroll` seals the key, and NMBL's
`cryptsetup --token-only` call unseals it.
See [Sealing a LUKS volume to the TPM](#sealing-a-luks-volume-to-the-tpm).

Before NMBL gives the operator any interactive context, it extends PCR 11
with a poison value and closes every TPM-unsealed LUKS mapping. This covers
the emergency shell, rescue, remote attach, a wrong-password shell and a
policy refusal. A secret sealed to the earlier PCR state then stays
unreachable until the next power cycle. The type system enforces this,
because a shell cannot start without a sealed token, and a build check
verifies it too.

| Option | Type | Default | Effect |
|--------|------|---------|--------|
| `tpm.measure` | bool | `false` | Extend the lock PCR with NMBL's boot events. Loads `tpm_crb` and `tpm_tis` early. |
| `tpm.pcrIndex` | int | `11` | PCR that NMBL measures into and caps. |
| `tpm.requireTpm` | bool | `true` when `tpm.measure` or `secureBoot.enable` is set | Abort the boot when no TPM works. |
| `tpm.device` | path | `/dev/tpmrm0` | TPM device. |

### The priority-file gate

`boot.nmbl.secureBoot` adds a gate before a measured boot or a staged
fragment. NMBL mounts a priority volume read-only and verifies a signed
file on it. If the file is valid, the boot continues. If the file is missing
or has a bad signature and `enforce` is on, NMBL refuses. It caps the TPM,
closes every TPM-unsealed mapping, writes the rescue sentinel, and locks
LUKS, LVM and mdraid again. It then shows a countdown whose only action is
reboot. Because the sentinel is set, the next boot goes to rescue.

| Option | Type | Default | Effect |
|--------|------|---------|--------|
| `secureBoot.enable` | bool | `false` | Verify the priority file before a measured or staged boot. |
| `secureBoot.priorityVolume.device` | str | `null` | The volume to mount and verify. |
| `secureBoot.priorityVolume.{mountpoint,fstype,options}` | str | `/mnt/nmbl-priority`, `ext4`, `ro,nosuid,nodev,noexec` | Mount settings. |
| `secureBoot.priorityVolume.insideLuks` | bool | `false` | Run the gate after LUKS unlock. |
| `secureBoot.signedFilePath` | str | `nmbl/priority.signed` | The signed file, with a `.sig` next to it. |
| `secureBoot.allowedKeyIds` | list of str | `[ ]` | Limit trust to these key fingerprints. Needed when more than one key is baked. |
| `secureBoot.enforce` | bool | `false` | Refuse on a bad or missing file. |
| `secureBoot.allowAuditModeInsecure` | bool | `false` | Allow `enable` without `enforce`. |
| `secureBoot.requireTpm` | bool | `false` | Abort when no TPM works. |
| `secureBoot.refuseCountdownSeconds` | int | `30` | Countdown on the refuse screen. |
| `secureBoot.sentinelPath` | str | `/boot/nmbl/rescue` | The rescue sentinel. |

### The rescue sentinel

NMBL checks for an empty file at `secureBoot.sentinelPath` (default
`/boot/nmbl/rescue`) at the start of the boot. If it exists, NMBL skips the
measured boot and goes to rescue with the TPM locked. The refuse path writes
this file, so a refused boot comes up in rescue on the next start. Delete
the file to return to normal boots. With embedded config, NMBL checks the
sentinel once `/boot` is mounted, before it measures or starts a
generation. The sentinel forces rescue whatever `rescue.automatic` says,
because it is an explicit request.

## Development tools

`nix run .#nmbl-simbox` runs the real `nmbl-init` as PID 1 in a rootless
container with simulated system calls. See
[docs/nmbl-simbox.md](docs/nmbl-simbox.md). `nix run .#nmbl-ui-preview`
shows NMBL's boot UI in an X11 window with mock state.

## Where to find things

| Path | Contents |
|------|----------|
| `nmbl-init-rs/` | Rust crate for the `/init` binary. |
| `nmbl-init-rs/PLAN.md` | Design of `nmbl-init`: phases, config schema, crate layout. |
| `nmbl-init-rs/src/config/` | Runtime TOML schema (`serde` types). |
| `nmbl-init-rs/src/main.rs`, `src/main_parts/` | Phase order, boot driver, panic recovery. |
| `nmbl-init-rs/src/ui/` | `ratatui` TUI, console backends, remote TUI. |
| `nmbl-init-rs/src/ipc/` | The TUI socket for remote sessions. |
| `nmbl-init-rs/src/splash/` | DRM splash backend (`image-splash`). |
| `nmbl-init-rs/src/rescue/` | Rescue dispatch, image verification and pinning, chrooted rescue child, network fallback. |
| `nmbl-init-rs/src/state/` | `state.bin` for stateful boot tracking. |
| `nmbl-init-rs/src/generations/`, `src/generation_*.rs` | Generation discovery and EROFS generation state. |
| `nmbl-init-rs/src/sig/` | ML-DSA verification and the baked keys. |
| `nmbl-init-rs/src/boot/` | Verify, measure and `kexec_file_load` handoff. |
| `nmbl-init-rs/src/tpm/` | PCR measurement and the lock cap. |
| `nmbl-init-rs/src/policy/` | Priority gate, refuse screen, sentinel, seal before shell, relock. |
| `nmbl-init-rs/src/imageload/` | Driver-image loop mount and module load. |
| `nmbl-init-rs/src/staged/` | Staged fragment verification, merge and re-run. |
| `nmbl-init-rs/nmbl-host-tools/` | `nmbl-sign`, the install-time signer. |
| `nmbl-init-rs/nmblctl/` | `nmblctl` (see `docs/nmblctl.md`). |
| `nmbl-init-rs/nmbl-boot-update/` | `nmbl-boot-update` (see `docs/boot-set-updates.md`). |
| `nmbl-init-rs/nmbl-simbox/`, `nmbl-init-rs/nmbl-ui-preview/` | Development tools. |
| `lib/options.nix` | Core `boot.nmbl.*` options. |
| `lib/modules/` | Activation, stateful, log import, rescue network stage and assertions. |
| `lib/modules/security/` | Signing, TPM, secure boot, staged boot, driver images, generation images, boot updates. |
| `lib/config.nix` | Module implementation, initramfs assembly. |
| `lib/config-toml.nix`, `lib/bootstrap-toml.nix` | Render `config.toml` and `bootstrap.toml`. |
| `lib/install-bootloader.nix`, `lib/install-signing.nix`, `lib/install-gen-signing.nix` | Install hook and install-time signing. |
| `lib/rescue-sfs.nix`, `lib/rescue/` | Flat rescue squashfs and stage-2 EROFS rescue image. |
| `lib/tpm-enroll.nix`, `lib/security-consts.nix` | `nmbl-tpm-enroll` and shared security constants. |
| `tools/` | `nmbl-erofsctl`, `nmbl-erofs-deploy`, `nmbl-erofs-receive` and VM test scripts. |
| `testing/` | VM tests and the `nix run .#test-*` apps. |
| `docs/` | Feature documentation. |
| `ARCHITECTURE.md` | Architecture overview. |

## Status

Working:

- Pseudo-filesystem mount, explicit module load, device wait and mount of the
  target filesystems.
- NixOS generation discovery from `/nix/var/nix/profiles/`.
- `ratatui` TUI: generation list, countdown, command-line editor, passthrough
  toggle, LUKS passphrase prompt, emergency menu with Pretty Shell and Raw
  Shell, serial console.
- Storage activation: LVM (`vgchange -ay`), mdraid (`mdadm --assemble --scan`),
  LUKS with TPM, key file or passphrase, and ZFS (`zpool import -N`).
- `kexec_file_load(2)` handoff, panic recovery with `--errored`.
- Install through GRUB or systemd-boot on GPT for BIOS or UEFI, the
  `efi-stub` UKI, and QEMU `-kernel`.
- External configuration, optionally signed.
- External rescue: the flat squashfs, and the staged full-system rescue with
  SSH and the remote TUI.
- Network rescue over HTTP/1.0 with signature, pin and SHA-256 checks.
- Graphical splash with fallback to the text TUI.
- ML-DSA generation signing, measured boot, the TPM lock before rescue, the
  priority-file gate and the rescue sentinel.
- Stateful boot tracking with rollback, instant boot and `nmblctl`.
- Signed EROFS generations and authenticated boot-set updates.
- Driver images and staged boot.

Not supported:

- `LABEL=`, `UUID=` and `PARTUUID=` device names in the runtime config. The
  config loader rejects them. Use the `/dev/disk/by-label/...`,
  `by-uuid`, `by-partlabel` or `by-partuuid` form, which NMBL creates with
  `blkid` at boot.
- LUKS unlock with FIDO2, YubiKey or smartcards.
- MBR partition tables. The bootstrapper supports GPT only.

Roadmap:

- Generations, the stage-2 rescue and the network stage use EROFS. Driver
  images, the staged image and the flat rescue still use squashfs. The goal
  is EROFS for these images too.

## License

MIT License. The scripts and Rust source in this tree are MIT. The content
in the initramfs (kernel, busybox, storage tools and others) carries its own
licenses.
