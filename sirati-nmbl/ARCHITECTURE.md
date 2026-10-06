# NMBL architecture

NMBL (NixOS Minimal BootLoader) uses Linux as the bootloader for NixOS. A
small kernel and one static-musl Rust binary boot first. That binary
(`nmbl-init`, PID 1) mounts the system storage, finds the NixOS generations,
lets the operator pick one, and `kexec`s into it.

This file describes how the pieces fit together. The `docs/` directory has
the detail for each subsystem:

| Document | Topic |
|---|---|
| [`docs/erofs-generations.md`](docs/erofs-generations.md) | signed EROFS generation images |
| [`docs/external-config-signing.md`](docs/external-config-signing.md) | signed `config.toml` on `/boot` |
| [`docs/boot-set-updates.md`](docs/boot-set-updates.md) | atomic two-slot boot-set updates |
| [`docs/rescue-stages.md`](docs/rescue-stages.md) | the staged full-system rescue |
| [`docs/network-stage.md`](docs/network-stage.md) | the signed rescue networking stage |
| [`docs/nmblctl.md`](docs/nmblctl.md) | `nmblctl`, remembered defaults, instant boot |
| [`docs/nmbl-simbox.md`](docs/nmbl-simbox.md) | the real PID 1 in a rootless container |

## System overview

### Why Linux as a bootloader

Each NixOS generation ships its own kernel and initrd in the Nix store,
linked from `/nix/var/nix/profiles/system-<N>-link/{kernel,initrd}`. A loader
that can mount the system filesystem can read those files in place. NMBL
therefore does not copy kernels to `/boot` on every `nixos-rebuild` and does
not maintain one loader entry per generation.

In exchange NMBL does the work of an early-userspace init. It mounts
pseudo-filesystems, loads storage drivers, activates LVM, LUKS, mdraid and
ZFS, mounts the system filesystem, enumerates generations and hands off with
`kexec_file_load(2)` and `reboot(LINUX_REBOOT_CMD_KEXEC)`. `nmbl-init` is
written in Rust so that this code stays small, starts fast, and on failure
reaches a diagnosed recovery menu where a kernel panic would otherwise occur.

### Boot chain

```
 firmware (BIOS or UEFI)
        |
        v
 first-stage loader (GRUB, systemd-boot, efi-stub UKI, or qemu -kernel)
        |   loads: NMBL kernel + NMBL initramfs
        v
 nmbl-init  (PID 1, static musl Rust binary)
        |
   +----+--------------------------------------------------------------+
   | 0.  early_init: mount /dev, /proc, /sys; wire /dev/console to     |
   |       fd 0-2; chmod / 0755; arm the early key tap; panic hook     |
   |     parse args; build-time and client modes exit here             |
   |     load /etc/nmbl/config.toml (embedded mode)                    |
   | 1.  mount pseudo-filesystems                                      |
   |     ---- everything below runs inside the async runtime ----      |
   | 0.5 bootstrap (external config only): bootstrap.toml, modules,    |
   |       blkid sweep, mount /boot, verify and load config.toml       |
   | G.  signed EROFS generation state: rollback or failure decision   |
   | S.  force_on_boot or the rescue sentinel: enter rescue            |
   | 2a. load early (graphics) kernel modules                          |
   | D.  driver images: verify, loop-mount, load modules               |
   |     open the console (splash or tty)                              |
   | P1. priority gate, PrePlainBoot phase              [secure boot]  |
   | 2b. load explicit kernel modules                                  |
   | 2c. blkid sweep for /dev/disk/by-*                                |
   | 3.  storage activations (mdraid, LVM, LUKS, ZFS)                  |
   | P2. priority gate, PostUnlock phase, then staged boot             |
   | 3b. mount system filesystems under /mnt/system                    |
   | 4.  rescue sentinel check, scan system-*-link                     |
   | 5.  instant boot or the selector countdown                        |
   | 6.  handoff: verify, measure PCR-11, kexec_file_load              |
   +-------------------------------------------------------------------+
        |
        v
 chosen generation's kernel + initrd (with NMBL's cpio fragment)
        |
        v
 normal NixOS stage 1 and systemd
```

The orchestration lives in `src/main.rs` and `src/main_parts/`.
`main_parts/boot_runtime.rs` runs the steps from Phase 0.5 to the console,
`main_parts/phases/post_console.rs` runs Phases 2b to 3b, and
`main_parts/dispatch.rs` runs Phases 4 to 6 and executes the final action.
The secure-boot steps (G, D, P1, P2, the verify and measure parts of
Phase 6) do nothing unless the matching `boot.nmbl.{signing, tpm,
secureBoot, staged, driverImages, generationImage}` options are set.

`early_init` mounts `/dev`, `/proc` and `/sys` before anything else. The
initramfs has an empty `/dev`, so PID 1 starts without a usable stdin,
stdout or stderr. Mounting devtmpfs and reopening `/dev/console` makes the
earliest failure visible. Phase 1 later mounts the pseudo-filesystems again
with full error reporting.

Phase 0.5 runs only when the initramfs contains `/etc/nmbl/bootstrap.toml`
(`boot.nmbl.configLocation = "external"`). With the default
`configLocation = "embedded"`, the full config is `/etc/nmbl/config.toml`
inside the initramfs. See [External configuration](#external-configuration).

### Terminal actions

No inner layer fires a no-return syscall. Every path returns a
`TerminalAction` (`src/terminal.rs`). Control unwinds to `main`, every
console and termios `Drop` runs, and `execute_terminal_action` then
performs the syscall:

| Variant | Effect |
|---|---|
| `Kexec` | `reboot(RB_KEXEC)` into the loaded image |
| `Reboot` | `reboot(RB_AUTOBOOT)` |
| `RebootIntoRescue` | prints the refuse banner, then `reboot(RB_AUTOBOOT)`; built only from a `Sealed` witness |
| `HaltWithBanner` | prints a structured banner, then `reboot(RB_HALT_SYSTEM)` |
| `Execve` | seals the TPM, then `execve`s a shell (the embedded rescue) |

Before any action, `execute_terminal_action` flushes the log ring to
`/nmbl-log/nmbl.log`. If a syscall returns, `halt_final` halts the machine
and falls back to `libc::_exit(1)`.

## External configuration

With `configLocation = "external"` the initramfs embeds only
`bootstrap.toml`. Phase 0.5 (`run_bootstrap_phase`) loads the bootstrap
modules, runs `blkid` to create the `/dev/disk/by-*` links (NMBL has no
udev), mounts the boot filesystem and reads the full `config.toml` from it.
With signing enabled, NMBL verifies the file under the `nmbl:boot-config:v1`
domain before it parses any byte. On GRUB boot-set hosts a cmdline token
selects which slot's config to read. Failures carry
`NmblError::Bootstrap { stage, .. }` with the stages `probe`,
`load-modules`, `blkid-sweep`, `mount-boot`, `verify-config`, `read-config`
and `mount-state`. The boot mount stays in place on error so the operator
can inspect and fix the config.

The config lives next to the images it names. On EROFS generation hosts it
is the generation directory's `config.toml`, so one `active` rename switches
config and image together. With `boot.nmbl.bootUpdate`, the config lives in
a signed boot-set slot. See
[`docs/external-config-signing.md`](docs/external-config-signing.md) and
[`docs/boot-set-updates.md`](docs/boot-set-updates.md).

## Generations and selection

### Profile generations

`generations::scan_generations` reads
`<system_root>/nix/var/nix/profiles/system-*-link` (default
`/mnt/system/nix/var/nix/profiles`). For each link it resolves the kernel,
initrd and `kernel-params`. When the scan finds nothing,
`generations/readiness.rs` reports whether nothing is mounted at the system
root, the wrong filesystem is mounted there, or the profiles directory is
empty.

### Signed EROFS generations

With `boot.nmbl.generationImage`, `/nix` is a read-only EROFS image chosen
by an `active` symlink in a generation directory. Phase 3b opens the image
once, verifies it under `nmbl:generation-image:v1`, and binds the same
descriptor to a read-only loop device (`devices::setup_verified_loop_device`).
Step G in the boot chain reads the `tested`, `pending` and `attempted`
selectors (`generation_state.rs`). It rolls a failed untested generation
back to its tested predecessor, or declares the boot failed when no
rollback target exists. When the image store is outside `/boot`,
`generation_store.rs` mounts it at a private path first. After `kexec`, the
target's systemd initrd runs `nmbl-generation-mount` (`src/bin/`) and
verifies the image a second time. See
[`docs/erofs-generations.md`](docs/erofs-generations.md).

### Default, one-shot, retry and instant boot

On stateful hosts NMBL first looks for a one-use operator retry request
(`nmblctl retry-generation`) and a rescue-exit retry. Either one boots its
generation without the selector. Otherwise the selector's default is the
one-shot `boot-once` file, then the persistent `boot-default` file, then
the active profile. `nmblctl` writes these files, and `boot_selection.rs`
defines their format for both sides. On EROFS generation hosts NMBL ignores
the files and boots `active`.

With `boot.nmbl.instantBoot.enable`, `ui::decide_instant_boot` skips the
countdown when the last boot succeeded, no rollback is active, there is no
rescue sentinel or pending generation, and no key was pressed. The early
key tap (`ui/early_key_tap.rs`) watches `/dev/console` from `early_init`
until the console opens. See [`docs/nmblctl.md`](docs/nmblctl.md).

### Persistent boot state

The `stateful` Cargo feature (`boot.nmbl.stateful.enable`) keeps a CBOR
`state.bin` in a fixed 16 KiB slot on the boot filesystem (`src/state/`).
Phase 0.5 mounts the boot filesystem read-write and bind-mounts it at the
state mountpoint. On FAT the mount forces `uid=0,gid=0,fmask=0177,dmask=0077`.
The state records the last attempted generation, so NMBL can fall back
after a failed boot. `nmbl-init --boot-succeeded` marks success from the
booted system.

## Failure handling

A phase error does not halt the machine. `rescue::automatic::on_boot_error`
decides the route with one setting, `[rescue].automatic`
(`boot.nmbl.rescue.automatic`):

- `true` enters the configured rescue without operator input;
- `false` opens the emergency menu.

Operator decisions (reboot, abort, leaving a wrong-password shell) always
return to the menu. A failure while entering rescue falls back to the menu,
so the decision cannot loop. Security refusals take the refuse terminus
described under [Lock-on-rescue](#the-seal-before-shell-invariant).

The rescue sentinel (default `/boot/nmbl/rescue`), and
`rescue.force_on_boot` with `mode = "external"`, send the boot to rescue
before Phase 2a (`policy::should_force_rescue`). Phase 4 checks the
sentinel again before it scans generations.

### Emergency menu

`shell::drop_to_emergency` runs the emergency screen (`ui/emergency/`) on
the live console. It offers these choices (`EmergencyChoice`):

| Choice | Behaviour |
|---|---|
| Pretty Shell | a shell on a pty, rendered inside the TUI by `alacritty_terminal` (`pretty-shell` feature, on by default) |
| Raw Shell | a console picker, then one busybox on a pty relayed to the chosen consoles (`ui/console_picker`, `ui/console_relay`) |
| Retry boot | runs Phases 3, 3b, 4 and 5 again |
| Verify kexec readiness | skips activations and mounts, scans generations, and asks for confirmation |
| Reboot | `TerminalAction::Reboot` |

Both shells run as children while NMBL stays PID 1. When a shell exits,
the menu returns. The extra consoles for Raw Shell come from
`[emergency_shell].extra_consoles`. On an unattended boot the menu reboots
after `general.emergency_timeout_secs` (30 seconds by default) without
input. Once any key was pressed in this session, the menu waits.

Call sites without a console (bootstrap failure, panic recovery) use
`open_console_and_drop_to_emergency`. It seals first and opens a tty
console without the splash.

### Rescue dispatch

```
 automatic rescue, sentinel, force_on_boot
        |
        v
 rescue::dispatch(config, console, cause)
   seal_secrets_blocking  (cap PCR-11, close TPM-unsealed mappers)
        |
        +-- mode = "embedded" --> TerminalAction::Execve(paths.shell)
        |
        +-- mode = "none" ------> TerminalAction::HaltWithBanner
        |
        +-- mode = "external"  (dispatch_external)
               image::open                 one descriptor for everything below
               verify::verify_rescue_image_gated   nmbl:rescue-sfs:v1
                  refuse under enforce --> relock, sentinel, RebootIntoRescue
               disk::prepare_disk_rescue
                  check [rescue.image].sha512 against the streamed digest
                  load loop, squashfs or erofs, overlay
                  LOOP_CONFIGURE (read-only) on the same descriptor
                  mount ro   -> /run/nmbl-rescue/lower
                  tmpfs      -> /run/nmbl-rescue/rw {upper,work}
                  overlay    -> /rescue  (writable)
                  network stage (if configured) -> /rescue/nmbl-network
                     |
                     +-- ok --> run_chrooted_external(/rescue)
                     |
                     +-- error, rescue.network = true (network-rescue feature)
                     |      rescue::net::try_network_rescue
                     |        pick source, DHCP, prompt URL, download to memfd,
                     |        confirm SHA-256, loop-mount, overlay
                     |        --> run_chrooted_external(/rescue)
                     |
                     +-- error otherwise --> HaltWithBanner
```

The network fallback (`src/net/`, `src/rescue/net/`) brings up the first
Ethernet interface with carrier, gets a DHCPv4 lease, and downloads the
rescue image over HTTP/1.0 into a `memfd`. The URL prompt starts with
`rescue.defaultUrl`. NMBL shows the computed SHA-256, and the operator
confirms it against `rescue.defaultSha256`.

### Chrooted rescue child

NMBL runs the external rescue as a chrooted child and stays PID 1 on the
initramfs root. `run_chrooted_external` drops the boot console first, so
its `Drop` restores `KD_TEXT` and termios. It then writes the host data from
`[rescue.system]` into the overlay (`rescue/host.rs`), mounts the optional
identity volume (`rescue/identity.rs`), and calls
`rescue::child::run_external_rescue_child`:

```text
// pre-fork, PID 1 (rescue/child_mounts.rs::apply_mount_plan):
rbind        <installed mounts> -> /rescue/nmbl-installed-<i>  // /mnt, system root, /boot, state
mkdir -p     /mnt /rescue/nmbl-root /rescue/mnt
bind         /rescue/mnt -> /rescue/mnt       // self-bind so it is a mount
make-shared  /rescue/mnt                      // MS_SHARED
rbind        /rescue/mnt -> /mnt              // PID 1 sees the child's mounts
rbind        /           -> /rescue/nmbl-root // NMBL root and TUI socket
make-private /rescue/nmbl-root
bind         /init       -> /rescue/bin/nmbl  // NMBL's binary as `nmbl`
rbind        /rescue/nmbl-installed-<i> -> /rescue/nmbl-root/<mount>

fork()
  // child, async-signal-safe calls only:
  chroot("/rescue"); chdir("/"); setsid()
  open("/dev/console"); dup2 onto 0, 1, 2
  execve(<rescue.entrypoint>, [basename],
         [TERM=linux, PATH=..., NMBL_TUI_SOCK=/nmbl-root/nmbl-run/tui.sock])

  // PID 1: reaps the child with the poller's waitpid(WNOHANG) op while
  // the remote TUI server runs; on exit it detaches the binds and
  // returns TerminalAction::Reboot.
```

The child reports through a pipe when its console is ready. NMBL then
records the rescue exit, so the next boot leaves the rescue request
(`rescue/child_ready_paths.rs`, `state/rescue_exit.rs`).

The flat rescue (`fullSystem.enable = false`) is a busybox squashfs with no
`/nix/store`. Its default contents use `busybox-sandbox-shell`, a static
busybox, because a dynamic binary's ELF interpreter path does not resolve
after the `chroot`.

### Staged full-system rescue

With `boot.nmbl.rescue.fullSystem.enable`, the rescue has two stages. Stage
1 is NMBL itself: its initramfs carries `erofs.ko`, `loop` and `overlay`.
Stage 2 is a host-independent EROFS image (LZ4HC by default;
`fullSystem.compression = "zstd"` needs NMBL kernel 6.10 or later, and an
assertion enforces it). `config.toml` pins the image by SHA-512 in
`[rescue.image]`. NMBL takes the host settings (SSH port, keys, host key
path, modules, network profile) from `[rescue.system]` at runtime and writes
them to `/etc/nmbl-rescue/` in the overlay. The image contains storage
tools only for the storage stacks the host uses
(`lib/rescue/storage-features.nix`). An interactive SSH login shows a motd
that tells the operator to run `nmbl`. See
[`docs/rescue-stages.md`](docs/rescue-stages.md) and
[`docs/network-stage.md`](docs/network-stage.md).

## Async runtime and remote TUI

### Single-threaded runtime

NMBL is one OS thread, so every `fork()` site stays safe. The TUI and the
later boot phases run on a tokio current-thread `LocalRuntime`
(`ui/runtime.rs`). Tasks use `spawn_local` and no worker threads exist. The
synchronous code in `main.rs`, the shell entry and the rescue dispatcher
enter the runtime through `block_on_tui` or `block_on_tui_with_poller`.
`main` builds the runtime right after Phase 1, and everything from Phase
0.5 onward runs inside it. `rescue::dispatch` builds its own runtime, so
`run_inner` calls it after the boot runtime has ended
(`BootOutcome::ForceRescue`).

Two mechanisms feed the runtime:

- `Console::poll_event` is `async`. The backends register their fd with
  `tokio::io::unix::AsyncFd` and await readiness.
- `sys/poller` is a single-threaded poller for syscalls without an async
  wrapper, paced by a 1 ms tokio timer. Its `waitpid(WNOHANG)` op
  (`sys/poller/waitpid.rs`) reaps the `blkid` sweep, the activation tools
  and the rescue child without blocking the thread.

### Root-only control socket

The `remote-tui` Cargo feature (enabled with `rescue.fullSystem.enable`)
adds a control socket in `src/ipc/tui_socket/`:

```
 PID 1 (server)                          non-PID-1 nmbl-init (client)
 bind_listener                           connect_and_serve
   mkdir  /nmbl-run          (0700)        open /dev/tty
   bind   /nmbl-run/tui.sock (0600)        connect to the socket
        |                                  sendmsg: Handshake (TERM, rows, cols)
        |                                    + SCM_RIGHTS tty fd
        v                                          |
 authenticate_and_receive                          |
   SO_PEERCRED uid 0? --no--> "N" + reason --------+--> client prints, exits 1
        | yes                                      |
   recvmsg fd + handshake                          |
   write "K" --------------------------------------+--> client waits for EOF
        |
        v
 serve a session on the received tty
```

`early_exit.rs` puts every invocation with `getpid() != 1` into client mode.
The client tries `$NMBL_TUI_SOCK`, then `/nmbl-run/tui.sock`, then the
chroot view `/nmbl-root/nmbl-run/tui.sock`.

### Recovery sessions

In the emergency menu (`shell/recovery.rs`), PID 1 runs the local menu and
the remote accept loop (`ui/remote/`) together under `tokio::select!`. The
first `TerminalAction` wins and the socket is unlinked. The recovery state
is borrowed, so session futures cannot be `spawn_local`ed. The accept
driver keeps them in a boxed `FuturesUnordered` and polls `accept`, the
session set and a `Shutdown` flag on each wake. A stuck session stays
`Pending` and does not block `accept`.

Each remote session has its own `SessionInteraction` and its own
`TtyConsole` on the received tty. `Ctrl+E` ends the session and `Ctrl+L`
opens a scrollable copy of the boot log. A remote session commits an action
into a shared first-committer `ActionSink`. A remote session counts as
attended, so it has no auto-reboot countdown. A render or poll error on the
remote tty ends that session and commits nothing.

The tty backend writes through a non-blocking queue (`ui/console/tty/queued.rs`).
A full terminal buffer (`EAGAIN`) keeps the rest queued until the fd is
writable, and frames coalesce. A remote tty that reads EOF or `EIO` ends its
session and releases its descriptors.

### Attaching from the rescue

NMBL stays PID 1 during the external rescue and binds its root at
`/rescue/nmbl-root`, so the socket is reachable inside the chroot. The
full-system image provides an empty `/bin/nmbl` file (and the alias
`nmbl-tui`), and the mount plan above binds NMBL's `/init` onto it. The
installed filesystems that were mounted before rescue stay visible under
`/nmbl-root`. `NMBL_TUI_SOCK` is
set in `/init`, `/etc/profile` and `sshd_config`. An operator who logs in
over SSH runs `nmbl` to attach to NMBL's menu. While the rescue child runs,
`reap_with_server` (`rescue/child.rs`) serves remote sessions until the
child exits or a remote session commits an action. For a committed action,
`rescue/child_stop.rs` signals every rescue process (`kill(-1)` with
`SIGTERM`, then `SIGKILL`), reaps them, syncs and detaches the rescue tree.
NMBL then performs the action. When the child exits on its own, NMBL
reboots.

## Console backends and the graphical splash

The TUI is written against the `Console` trait (`ui/console/`). Screen
content lives in `ui/app/` and `ui/view/`; the backends are render targets.
`open_console` picks the backend once, after Phase 2a and the driver
images, so DRM drivers are loaded:

- `TtyConsole` (`ui/console/tty/`) drives ratatui through a termwiz
  `Surface` and terminfo renderer on a non-blocking `/dev/console` fd or a
  remote tty. NMBL parses input bytes itself with termwiz's `InputParser`
  and translates them to crossterm key types. It keeps a VT in `KD_TEXT`
  and silences printk while the TUI is active.
- `SplashConsole` (`ui/console/splash/`, `image-splash` feature,
  `boot.nmbl.splash.enable`) draws the same screens on a KMS
  framebuffer over a PNG background.

Panic recovery always uses `TtyConsole`. Any splash bring-up failure (no
`/dev/dri/card*`, no connector, font error, buffer allocation) logs a
warning and falls back to the tty. A serial console always gets the text
TUI.

Each splash frame goes through these steps: ratatui renders vt100 bytes;
a headless `alacritty_terminal::Term` turns them into a cell grid; the
compositor (`src/splash/compositor/`) blits the background, draws a blurred
contrast halo, and blends `fontdue` glyphs into an XRGB8888 dumb buffer.
Keyboard input comes from `/dev/tty1` with the keyboard in `K_XLATE` mode.
NMBL reads the shift state through `TIOCLINUX`. The splash uses `simpledrm`
or the EFI framebuffer by default. A GPU driver such as virtio-gpu or amdgpu
replaces `simpledrm`, so it must be listed in `boot.nmbl.earlyKernelModules`.

## Verified loading, measurement, and lock-on-rescue

With `boot.nmbl.signing`, `tpm` or `secureBoot` set, the `secure-boot` Cargo
feature adds signature checks and measured boot on top of UEFI Secure Boot.
The code lives in `src/sig` (signatures), `src/tpm` (measurement and the
PCR cap), `src/policy` (the priority gate, the seal guard, the refuse
terminus) and `src/boot/handoff.rs` with `handoff_load.rs`.

### The handoff: verify, measure, load

`boot::handoff::verify_measure_then_load` runs three steps in fixed order
for every generation boot:

1. Verify. NMBL opens the kernel and initrd once each and checks their
   signatures over those descriptors before anything enters the kexec slot.
   An enforce-mode failure returns `NmblError::PolicyRefused`.
2. Measure. NMBL extends PCR-11 with an NMBL identity marker, the kernel and
   initrd digests from step 1, the kexec cmdline, and each driver image
   digest.
3. Load. `kexec_file_load(2)` receives the same verified descriptor and the
   cmdline that was measured.

The verified bytes, the measured bytes and the booted bytes are therefore
the same. The cpio fragment NMBL appends (LUKS keyfiles for
`pass_to_stage1`, the boot log) is outside the verified and measured set.
`tpm/sbstate.rs` reads the firmware Secure Boot state before measuring; it
warns in audit mode and refuses under `secure_boot.enforce`.

### Post-quantum signatures (`src/sig`)

Signatures are FIPS 204 ML-DSA (ML-DSA-65 by default, ML-DSA-87 optional)
from the pure-Rust `fips204` crate, over each blob's SHA-512. Each signed
file has a detached `NMBLSIG1` sidecar (`sig/wire.rs`, shared byte for byte
with the host signer) with the algorithm id, a 32-byte domain tag and the
signature. The verifier checks the domain tag before it tries any key, then
accepts the first baked key of that algorithm that verifies. There is no
path that accepts an unsigned file under enforce. Each role has its own
domain, so a signature for one role fails for every other:

| Domain | Signed object |
|---|---|
| `nmbl:gen-kernel:v1`, `nmbl:gen-initrd:v1` | profile generation kernel and initrd |
| `nmbl:generation-image:v1` | EROFS generation image |
| `nmbl:boot-config:v1` | external `config.toml` |
| `nmbl:rescue-sfs:v1` | rescue image |
| `nmbl:network-stage:v1` | rescue networking stage |
| `nmbl:rescue-tools:v1` | rescue tools image (`nmblctl`) |
| `nmbl:driver-image:v1` | driver images and the staged image |
| `nmbl:staged-fragment:v1` | staged config fragment |
| `nmbl:priority-file:v1` | priority-volume signed file |
| `nmbl:boot-set-manifest:v1`, `nmbl:boot-set-artifact:v1` | boot-set slots |

The trust anchor is a set of public keys compiled into the binary
(`sig/baked_keys.rs`). The Nix builder `mkNmblInit { publicKeys }`
regenerates that file. It is committed empty, and the
`zero-keys-rejected` check in `nmbl-init-rs/flake.nix` proves that an
enforcing build with no keys fails at evaluation. Nothing on the writable
boot partition can replace the anchor.

The `nmbl-sign` host tool (`nmbl-host-tools/`) signs at install time. It
reads the private key from a runtime path or from stdin
(`signing.imageKeyCommand`, `generationKeyCommand`). Evaluation fails if a
key path is under the Nix store (`lib/install-signing.nix`,
`lib/signing-key.nix`). `signing.deferInstallSigning` skips signing in the
installer for builds that cannot see the key, for example remote EROFS
deployment. Runtime enforcement stays on.

### Measured boot and the TPM cap (`src/tpm`)

`tpm/measure.rs` extends PCR-11 with the handoff events in fixed order.
Each event is `SHA-256(domain || 0x00 || body)`. A host-side predictor
computes the same value, so a secret can be sealed to the PCR-11 of the
next measured boot. NMBL sends no `TPM2_Unseal`. LUKS auto-unlock uses
`systemd-cryptenroll` and `cryptsetup open --token-only`, bound to PCRs 11
and 7 (README, "Sealing a LUKS volume to the TPM").

The TPM core (`tpm/mod.rs`, `transport.rs`, `commands.rs`, `presence.rs`)
is compiled in every build, because the lock-on-rescue cap needs it. It
talks to `/dev/tpmrm0` through the pure-Rust `tpm2-protocol` crate. The cap
extends PCR-11 with `SHA-256("nmbl:relock-poison:v1")`. After the cap, a
secret sealed to the earlier PCR-11 value cannot be unsealed until the next
reset.

### The seal-before-shell invariant

`policy::seal_secrets` (`policy/guard.rs`) runs in this order: cap PCR-11,
close every TPM-unsealed LUKS mapper recorded by `activation/seal.rs`, then
return a `Sealed` token. Shell, pty, remote-session and `execve` helpers
take `Sealed` by value, so code cannot spawn a shell without sealing first.
The `nmbl-init-must-seal` check fails the build when a spawn site lacks a
`Sealed` witness or a `// seal-exempt:` justification. The cap result maps
as follows: `Capped` proceeds, `NoTpm` proceeds only without `requireTpm`,
and `Failed` (TPM present, cap failed) always fails closed. The emergency
menu, the wrong-password shell, every rescue mode, each remote session and
the refuse screen all pass through this guard.

### The priority gate and the refuse terminus

`boot.nmbl.secureBoot` mounts a priority volume read-only and verifies a
signed file on it. The gate runs at `PrePlainBoot` (a volume outside LUKS)
or at `PostUnlock` (a volume that appears after activation). On success it
returns an `AttestedVolume`. That type has no public constructor, and only
staged boot consumes it.

A refusal under enforce (a bad priority file, a bad generation, rescue or
driver signature, a failed staged apply, a failed seal) goes to one
terminus: `policy::relock_and_refuse` and `run_refuse_screen`. It caps
PCR-11, closes TPM-unsealed mappers, writes the rescue sentinel while
`/boot` is still writable, and relocks storage (`cryptsetup close`,
`vgchange -an`, `mdadm --stop`). It then reduces the shown log to a fixed
banner and runs a non-interactive countdown with two actions, reboot now
and view the reduced log. The result is `RebootIntoRescue`. Because the
sentinel exists, the next boot enters rescue with the TPM capped. Removing
the sentinel restores the measured boot.

## Driver images and staged boot

### Driver images (`src/imageload`)

`boot.nmbl.driverImages` ships out-of-tree modules and firmware in signed
squashfs images on the boot partition. After Phase 2a, `load_driver_images`
handles each image over one descriptor: verify under
`nmbl:driver-image:v1`, loop-mount read-only, register the image's
`lib/firmware` with `firmware_class` (NMBL has no udev), and load the
listed modules with the per-image blacklist. Each image's digest goes into
the PCR-11 measurement. A failure takes the refuse terminus. NMBL detaches
the images before kexec, reboot, halt or rescue. For an `Execve` shell it
leaves them mounted, since they contain no secrets. A Nix assertion
requires an active secure-boot setup for driver images.

### Staged boot (`src/staged`)

`boot.nmbl.staged` (Cargo feature `staged-boot`, which implies
`secure-boot`) puts a signed config fragment and a driver image on the
priority volume inside LUKS. After the `PostUnlock` gate returns its
`AttestedVolume`, `apply_staged_boot` verifies the image
(`nmbl:driver-image:v1`) and the fragment (`nmbl:staged-fragment:v1`). It
parses the fragment with `deny_unknown_fields` and merges it into the
running `Config` table by table. It validates the result and restores
every table on failure. It then runs the merged modules, driver images and
activations again. The fragment type has no `signing`, `secure_boot` or
`staged` table, so a fragment that names one fails to parse. Any failure is
a `PolicyRefused`.

## Handover to the next kernel

`boot/handoff.rs` builds the kexec cmdline from the generation's
`kernel-params`, an `init=` argument and NMBL's markers (for example
`nmbl.rollback-after-untested-new-generation-failed`). It appends a newc
cpio fragment (`sys/cpio.rs`) to the initrd with any LUKS keyfiles marked
`pass_to_stage1` and the log transcript `/nmbl-log/nmbl.log`. NMBL loads the
image before it unmounts anything, because `kexec_file_load` reads from
`/mnt/system`.

`src/handover/` contains the decoders for each payload (cmdline, cpio, log
buffer, key handover). `nmblctl status` and `nmbl-simbox` use them, and
round-trip tests compare them against the encoders. Key material is masked
unless the caller passes `--reveal`.

## Source layout

The Rust workspace lives in `nmbl-init-rs/`. The binary entry is
`src/main.rs`; the logic is in library modules under `src/lib.rs` so it is
unit-testable.

```
nmbl-init-rs/
|-- Cargo.toml            # workspace root, -Oz, fat LTO, clippy denies, features
|-- flake.nix             # crane + fenix static-musl build, checks, key baking
|-- nmbl-host-tools/      # nmbl-sign, the install-time signer
|-- nmblctl/              # root-only system control tool
|-- nmbl-boot-update/     # signed two-slot boot-set updater
|-- nmbl-simbox/          # PID 1 in a rootless container (development)
|-- nmbl-ui-preview/      # boot UI in an X11 window (development)
`-- src/
    |-- main.rs, main_parts/   # early_init, args, phase driver, dispatch, client exit
    |-- bin/                   # nmbl-generation-mount, nmbl-generation-state
    |-- config/                # TOML schema per table, load, validate, fragment
    |-- error/                 # NmblError, format_chain
    |-- log/                   # verbosity, byte ring, /dev/kmsg tee, TUI gate
    |-- panic.rs               # panic hook and re-exec into recovery
    |-- terminal.rs            # TerminalAction
    |-- security_consts.rs     # lock PCR, poison, sentinel path, countdown
    |
    |-- mount.rs               # Phase 1 pseudo-filesystems
    |-- modules.rs             # early and explicit module loading
    |-- activation/            # Phase 3 orchestrator, LUKS, TPM-mapper registry
    |-- devices/               # Phase 3b mounts, loop devices, generation images
    |-- generations/           # Phase 4 scan, gen-id, readiness diagnosis
    |-- generation_mount.rs    # post-kexec EROFS mount (secure-boot)
    |-- generation_state.rs    # tested/pending/attempted selectors (secure-boot)
    |-- generation_store.rs    # early mount of an external image store
    |-- boot_selection.rs      # boot-default, boot-once, retry files
    |-- boot/                  # Phase 6 handoff: verify, measure, load
    |-- handover/              # decoders for the kexec payloads
    |
    |-- sig/                   # ML-DSA verify, NMBLSIG1, baked keys, config verify
    |-- tpm/                   # TPM core, PCR-11 measure, Secure Boot state
    |-- policy/                # gate, seal guard, sentinel, relock, refuse screen
    |-- imageload/             # driver images
    |-- staged/                # staged fragment verify, merge, rerun
    |-- state/                 # state.bin (stateful)
    |
    |-- shell/                 # emergency entry, recovery driver, banners
    |-- rescue/                # dispatch, disk, image pin, host data, child, net
    |-- net/                   # DHCP, HTTP/1.0, interfaces (network-rescue)
    |-- ipc/                   # root-only TUI socket (remote-tui)
    |-- validate/              # --validate-hardware, closure and tool checks
    |-- mocking/               # --debug-tui scenarios (mocking feature)
    |-- util/                  # hex, hash
    |
    |-- ui/
    |   |-- app/, view/        # state machine and renderers
    |   |-- runtime.rs         # LocalRuntime
    |   |-- console/           # Console trait, tty and splash backends, parser
    |   |-- emergency/         # emergency screen
    |   |-- pretty_shell/      # in-TUI terminal (pretty-shell)
    |   |-- console_picker/, console_relay/   # Raw Shell
    |   |-- remote/            # remote accept loop and sessions (remote-tui)
    |   |-- rescue/            # network-rescue screens
    |   `-- instant_boot.rs, early_key_tap.rs, selector.rs, ...
    |
    |-- splash/                # DRM, PNG, glyph cache, compositor, input
    |
    `-- sys/                   # syscall wrappers without policy
        |-- mount.rs, module/, kexec.rs, loopdev/, cpio.rs, uname.rs
        |-- activation/        # fork and execve runner for tools
        |-- blkid/             # /dev/disk/by-* links from blkid -o export
        |-- btrfs.rs           # btrfs device scan
        |-- poller/            # single-threaded poller, waitpid(WNOHANG)
        |-- pty/               # post-fork async-signal-safe pty spawn
        `-- tty.rs, vt.rs, printk.rs
```

Modules under `sys/` are thin wrappers. The phase modules run once per
boot from the driver. `tpm/` and `policy/` are always compiled; `sig/` is
always compiled for its `wire` format, and the rest of it is
`secure-boot`-gated.

## Runtime configuration

All runtime settings come from one TOML file: `/etc/nmbl/config.toml` in
the initramfs, or `config.toml` on `/boot` in external mode.
`lib/config-toml.nix` generates it from `config.boot.nmbl`, the resolved
`fileSystems` and the activation blocks, with `pkgs.formats.toml`.

### Initramfs contents

`lib/config.nix` builds `system.build.nmblInitramfs` with `pkgs.makeInitrd`
(gzip -9):

| Path | Content |
|---|---|
| `/init` | `nmbl-init`, built with the Cargo features the options need (`lib/signing-build.nix`) |
| `/etc/nmbl/config.toml` or `/etc/nmbl/bootstrap.toml` | the runtime or bootstrap config |
| `/bin/sh` | busybox, for `rescue.mode` `embedded` and `external` |
| `/bin/blkid` | util-linux `blkid` |
| `/lib/modules`, `/etc/modprobe.d/nixos.conf` | the module closure and blacklist |
| `/etc/splash/font.ttf`, `/etc/splash/image.png` | with the splash; the image only for `backgroundLocation = "initrd"` |
| activation tools | `cryptsetup`, `vgchange`, `mdadm`, `zpool`, from `cfg.activation.extraContents` |

The `luks-tpm` tools come as a prepended cpio built with `makeInitrdNG`. A
build check fails if `nmbl-tpm-enroll` is in the initramfs closure.

The Cargo features follow the options: `image-splash` (splash),
`network-rescue` (`rescue.network`), `remote-tui` (`fullSystem.enable`),
`rescue-stages` (external full-system rescue), `stateful`, `secure-boot`
(any security table) and `staged-boot`. A config-only change regenerates the
TOML and the initramfs and reuses the binary.

### Schema

`src/config/` defines the schema as `serde` structs with
`deny_unknown_fields`, so a typo fails the boot with an error. Top-level
tables:

| Table | Main fields | Purpose |
|---|---|---|
| `general` | `verbosity`, `timeout_ms` (5000), `emergency_timeout_secs`, `device_timeout_secs` (30), `panic_report_dir` (`/run`), `instant_boot` | logging and timers |
| `kernel_modules` | `early`, `explicit`, `blacklist`, `modules_dir` | Phase 2a and 2b modules |
| `filesystems[]` | `device`, `mountpoint`, `fstype`, `options`, `is_root` | Phase 3b mounts |
| `activations[]` | `kind`, `required_modules`, `binary`, `argv`, `produces_devices`, `source_devices`, `description`, `prompt_label` | Phase 3 steps |
| `tui` | `enable_editor`, `show_kernel_params` | selector features |
| `paths` | `nix_profiles_dir`, `system_root` (`/mnt/system`), `shell` (`/bin/sh`) | scan and mount locations |
| `rescue` | `mode`, `entrypoint`, `sfs_path`, `automatic`, `force_on_boot`, `network`, `default_url`, `identity_volume`, `image`, `network_stage`, `tools`, `system` | rescue |
| `emergency_shell` | `extra_consoles` | Raw Shell consoles |
| `tpm` | `measure`, `pcr_index`, `require_tpm`, `device` | measurement and cap |
| `driver_images` | `enable`, `images[]` | driver images |
| `splash` | `enable`, `background_image`, `background_location`, `font_path`, `dri_path` | splash (`image-splash`) |
| `signing` | `enable`, `enforce`, `algorithm`, `sig_path_suffix`, `uki` | signature policy (`secure-boot`) |
| `secure_boot` | `enable`, `enforce`, `priority_volume`, `signed_file_path`, `allowed_key_ids`, `sentinel_path`, `require_tpm`, `refuse_countdown_seconds` | priority gate and refuse (`secure-boot`) |
| `generation_image` | `enable`, `mountpoint`, `signature_path`, `state_root`, `stage1_store`, `automatic_rollback`, `track_state` | EROFS generations (`secure-boot`) |
| `staged` | `enable`, `image`, `fragment`, `sig` | staged boot (`staged-boot`) |
| `stateful` | `max_recovery_attempts`, `success_target` | persistent state (`stateful`) |

`activations[].kind` is one of `lvm`, `mdraid`, `luks-tpm`, `luks-keyfile`,
`luks-password` or `zfs`. `general.serial_console` is still accepted and
has no effect. Public signing keys are compiled into the binary and do not
appear in the TOML. Nix emits a feature-gated table only when it also
builds the binary with that feature.

### Validation

`Config::load` parses the file and calls `Config::validate`. It rejects a
`filesystems[].device` of the form `LABEL=`, `UUID=` or `PARTUUID=`. The
`/dev/disk/by-*` forms work, because NMBL creates those links from `blkid`.
It also rejects a `/dev/mapper/*` device that no activation can produce.

The build runs the same binary against the generated files with
`--validate-config` and `--validate-nix-filesystem-closure`
(`lib/config-toml.nix`). `--validate-config-fragment` checks a staged
fragment's schema. The installer runs `--validate-hardware` on the target.
If the config cannot be loaded at boot, `main` uses
`Config::recovery_default()` and routes the error to the failure handler.

## Storage activation

NMBL mounts with `mount(2)`. Everything between "module loaded" and "device
node exists" runs as an external static tool started by the activation
orchestrator.

`lib/modules/activation.nix` inspects `config.fileSystems` (`/dev/mapper/*`,
`/dev/md*`, `fsType = "zfs"`) and the `boot.nmbl.activation.*` options. It
produces `activationBlocks` (the `[[activations]]` rows),
`extraKernelModules`, `extraContents`, `prependCpios` and `assertions`.

At runtime `activation::run_all_activations` runs the blocks in order. For
each block it warns about `required_modules` missing from `/proc/modules`,
asks the `PasswordSupplier` for a `luks-password` passphrase and pipes it
to the tool's stdin in a `Zeroizing` buffer, runs the tool through
`sys::activation` with the poller's `waitpid`, and waits up to
`general.device_timeout_secs` for each `produces_devices` path. A non-zero
exit is fatal. Phase 2c runs `blkid` before the activations, so a
`/dev/disk/by-partlabel/...` LUKS device resolves.

| Kind | Command | Modules | Device |
|---|---|---|---|
| `mdraid` | `mdadm --assemble --scan` | `md_mod` (plus `raid0`, `raid1`, `raid10`, `raid456` in the initramfs) | `/dev/md*` |
| `lvm` | `vgchange -ay` | `dm_mod` | `/dev/mapper/<vg>-<lv>` |
| `luks-tpm` | `cryptsetup open --token-only <dev> <name>` | `dm_mod`, `dm-crypt`, `aesni_intel`, `xts`, `sha256_generic`, `tpm_crb`, `tpm_tis` | `/dev/mapper/<name>` |
| `luks-keyfile` | `cryptsetup open <dev> <name> --key-file=<file>` | the LUKS set without TPM | `/dev/mapper/<name>` |
| `luks-password` | `cryptsetup open <dev> <name> --key-file=-` | the LUKS set without TPM | `/dev/mapper/<name>` |
| `zfs` | `zpool import -N <pool>` | `zfs` | mounted in Phase 3b |

The blocks run in the order mdraid, LVM, LUKS, ZFS.

## Panic recovery

A panicking PID 1 has unknown state. NMBL resets it with `execve(2)`, which
keeps PID 1:

```
 early_init: install_panic_hook(/run)      before args and config
 main: install_panic_hook(general.panic_report_dir)
   |
   | --- panic ---
   v
 hook: build report, write <dir>/nmbl-panic-<pid>.txt (best effort)
   v
 execve("/proc/self/exe", ["nmbl-init", "--errored=<path>"])
   v
 main again: early_init, then recover_from_panic
   read report, load config leniently, verbose logging,
   handle_boot_failure(NmblError::Panicked { report_path })
```

The failure route is the same as for a phase error: automatic rescue or
the emergency menu on a tty console. If the report write fails, the hook
passes `--errored=<missing>`. If the `execve` fails, the hook calls
`libc::_exit(1)` and the kernel panics; that is the documented worst case.
The re-executed process installs the hook again in `early_init`.

## Project rules

- Release builds are static musl with `-Oz`, fat LTO, one codegen unit and
  stripped symbols. `+crt-static` is set in `nmbl-init-rs/flake.nix`.
- Production code contains no `unwrap`, `expect`, `panic!`, `todo!`,
  `unimplemented!`, `unreachable!`, indexing or `dbg!`. `[lints.clippy]` in
  `Cargo.toml` and the `nmbl-init-clippy` check (`--deny warnings`)
  enforce this. Test modules opt back in with an `#[allow]` and a reason.
- Every `unsafe` block carries a `// SAFETY:` comment. The blocks cover
  `fork` and post-fork code (activation runner, pty spawn, rescue child),
  syscalls without a safe wrapper (for example `kexec_file_load`,
  `finit_module`, `mknod`, DHCP sockets, and loop, VT and btrfs ioctls),
  and the final `_exit` calls. Run `grep -rn 'unsafe' nmbl-init-rs/src`
  for the current list.
- Every `std::process::Command` and `execve(` needs an inline
  `// execve safety: <why>` comment. The `nmbl-init-no-exec` check
  enforces it.
- Flake checks in `nmbl-init-rs/flake.nix` enforce the security
  invariants. `nmbl-init-must-seal` requires a `Sealed` witness or a
  `// seal-exempt:` comment at each shell spawn. `nmbl-init-no-cap-bypass`
  requires a `// cap-exempt:` or `// signing safety:` comment on each TPM
  cap degrade or verify downgrade. `nmbl-init-security-consts` compares
  `security_consts.rs` with `lib/security-consts.nix`.
- Cargo features are additive. A build without a feature compiles none of
  its optional dependencies. The TPM core and the seal terminus are always
  compiled.
- `overflow-checks = true` in release builds.
- Activation tools run with one environment variable, `DM_DISABLE_UDEV=1`,
  so libdevmapper creates the `/dev/mapper` nodes itself.

## Tests

Unit tests sit next to the code in `#[cfg(test)]` modules. They cover,
among other things, `modules.dep` parsing, mount options, generation
sorting, key handling, panic reports, ML-DSA round trips and cross-domain
rejection (`sig/`), golden PCR-11 vectors (`tpm/`), staged merge and
rollback (`staged/`), seal ordering (`policy/`), the rescue pins and host
data (`rescue/`), remote session lifetime under back-pressure and hang-up
(`ui/remote/tests.rs`), and the handover decoders. `tempfile` is the only
dev-dependency. Run `cargo test --all-features` in the `nmbl-init-rs`
devshell; the default features skip the secure-boot tests.

VM tests live under `testing/` and run as flake apps from
`sirati-nmbl/flake.nix` (`nix run .#<app>`), for example
`test-external-rescue`, `test-rescue-ssh`, `test-network-stage-vm`,
`test-stateful`, `generation-state-vm-test`, `test-boot-update-vm`, the
`test-secure-boot*` scenarios and `sb-install-test-secure-boot*`. They boot
NMBL under QEMU and assert on serial output. The secure-boot scenarios add
swtpm and a Secure Boot OVMF. While the interactive console is open, NMBL
does not print its `nmbl_*!` markers to serial, so these tests read
`signature verified` from the post-kexec journal, which imports NMBL's log
(`lib/modules/log-import.nix`).

`checks.x86_64-linux` in `sirati-nmbl/flake.nix` contains pure evaluation
checks such as `rescue-image-host-independent`, `rescue-storage-tools-eval`,
`rescue-compression-kernel-eval` and `rescue-ssh-welcome`. `nmbl-simbox`
runs the unmodified PID 1 in a rootless container with simulated syscalls
([`docs/nmbl-simbox.md`](docs/nmbl-simbox.md)).

## Companion tools

| Tool | Source | Role |
|---|---|---|
| `nmblctl` | `nmbl-init-rs/nmblctl` | root-only control and inspection on the running system ([`docs/nmblctl.md`](docs/nmblctl.md)) |
| `nmbl-sign` | `nmbl-init-rs/nmbl-host-tools` | ML-DSA signer for every signed role |
| `nmbl-boot-update` | `nmbl-init-rs/nmbl-boot-update` | atomic two-slot boot-set updates ([`docs/boot-set-updates.md`](docs/boot-set-updates.md)) |
| `nmbl-generation-mount`, `nmbl-generation-state` | `nmbl-init-rs/src/bin` | target-initrd image mount and generation state |
| `nmbl-erofsctl`, `nmbl-erofs-deploy`, `nmbl-erofs-receive` | `tools/`, `lib/erofs*.nix`, `nmbl-host-tools/src/receive.rs` | EROFS generation activation and remote deployment |
| `nmbl-tpm-enroll` | `lib/tpm-enroll.nix` | seals a LUKS key to the predicted PCR-11 |
| `nmbl-simbox`, `nmbl-ui-preview` | `nmbl-init-rs/` | development only |
