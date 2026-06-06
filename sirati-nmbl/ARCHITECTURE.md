# NMBL Architecture

NMBL (NixOS Minimal BootLoader) is a "Linux-as-bootloader" for NixOS. A
small kernel + a single static-musl Rust binary boot first; that binary
(`nmbl-init`, PID 1) reads the existing NixOS generations out of
`/nix/var/nix/profiles/` on the system filesystem, lets the operator
pick one, and `kexec`s into it.

## System overview

### Why Linux as a bootloader

The architectural anchor is this: each NixOS generation already ships
its own kernel and initrd inside the Nix store, symlinked under
`/nix/var/nix/profiles/system-<N>-link/{kernel,initrd}`. A second-stage
loader that can mount the system filesystem can therefore read those
files in place. There is no need to copy kernels and initrds to `/boot`
on every `nixos-rebuild`, no need to manage GRUB/systemd-boot entries
per generation, no fragile bootspec rewriting.

The cost is that NMBL has to do everything an early-userspace init
does: mount pseudo-filesystems, load storage drivers, activate
LVM/LUKS/mdraid/ZFS, mount the system filesystem, then enumerate
generations and hand off via `kexec_file_load(2)` +
`reboot(LINUX_REBOOT_CMD_KEXEC)`. The whole point of writing
`nmbl-init` in Rust is that this code path needs to be small (it sits
in the initramfs), fast (boot path), and reliable enough that a failure
drops the operator into a clearly-diagnosed emergency shell instead of
a kernel panic.

### Boot chain

```
 firmware (BIOS / UEFI)
        |
        v
 first-stage loader (GRUB / systemd-boot / qemu -kernel)
        |   loads:   nmbl kernel + nmbl initramfs
        v
 nmbl-init  (PID 1, static musl Rust binary)
        |
   +----+-----------------------------------------------------------+
   | 1.  mount pseudo-fs (/proc, /sys, /dev, /run, /tmp)            |
   |                                                                |
   | 0.5 bootstrap (ONLY when configLocation = "external"):         |
   |       load bootstrap.toml -> bootstrap modules -> blkid sweep  |
   |       -> mount boot partition -> load full Config              |
   |                                                                |
   | S.  rescue sentinel: if /boot/nmbl/rescue exists, SEAL         |
   |       (cap PCR-11) and go straight to rescue  [secure boot]    |
   | G1. priority gate (PrePlainBoot): verify the signed file on a  |
   |       non-LUKS priority volume                [secure boot]    |
   | 2a. load early/explicit kernel modules (finit_module)          |
   | D.  driver images: verify sig -> loop-mount -> load modules    |
   |                                               [secure boot]    |
   | 3.  run storage activations (LVM / LUKS / mdraid / ZFS)        |
   | G2. priority gate (PostUnlock) on an inside-LUKS volume, then  |
   |       staged boot: verify + transactionally merge a signed     |
   |       config fragment, re-run its effects     [secure boot]    |
   | 3b. wait for + mount system filesystems under /mnt/system      |
   | 4.  scan /nix/var/nix/profiles/system-*-link                   |
   | 5.  console (splash OR tty): countdown -> pick gen / shell     |
   | 6.  handoff: VERIFY sig -> MEASURE PCR-11 -> kexec_file_load   |
   +----------------------------------------------------------------+
        |
        v
 chosen NixOS generation's kernel + initrd
        |
        v
 normal NixOS stage-1 / systemd
```

The phases marked `[secure boot]` only run under the relevant
`boot.nmbl.{signing,tpm,secureBoot,staged,driverImages}` options; with
the whole secure-boot surface off, NMBL collapses to the classic
sentinel-free, unsigned, unmeasured boot. Phase 5's console is the
graphical splash when `boot.nmbl.splash.enable` is set and a DRM device
is reachable, otherwise the text TUI (see **Console backends and the
graphical splash**). The full secure-boot machinery — what is verified,
what is measured, and the lock-on-rescue invariant — is described under
**Verified loading, measurement, and lock-on-rescue** below.

Phase 0.5 is numbered out of order on purpose: it sits between
pseudo-fs mount (Phase 1) and explicit module load (Phase 2) so the
bootstrap stage has `/proc`, `/sys`, and `/dev` available, but it
does NOT run when the build embedded the full config inline
(`configLocation = "embedded"`, the default). In that case the
runtime config comes from `/etc/nmbl/config.toml` inside the
initramfs and Phase 2 starts immediately. The probe is a
`try_exists()` on `/etc/nmbl/bootstrap.toml`; presence means
two-tier mode, absence means single-tier.

Phase 0.5 leaves the boot mount in place on the error path so the
operator's emergency shell can inspect (and fix) the on-disk
config. Failures are wrapped in
`NmblError::Bootstrap { stage, source }`, with `stage` strings
(`probe`, `load-modules`, `blkid-sweep`, `mount-boot`,
`read-config`) that the banner surfaces verbatim.

Any phase that returns `Err` routes to `shell::drop_to_emergency`,
which dispatches based on `boot.nmbl.rescue.mode` (see
**Rescue dispatch** below) — for `embedded` the legacy `execve` of
`/bin/sh` from the initramfs runs, for `external` the squashfs is
loop-mounted as a writable overlay and run as a chrooted child while
NMBL stays PID 1, for `none` the system halts with a structured
banner. Neither `drop_to_emergency` nor the rescue dispatcher fires a
no-return syscall itself: every path returns a `TerminalAction` that
the single `main::execute_terminal_action` site performs once the
stack has unwound (see **Async TUI and remote attach** below).

Two refinements apply once secure boot is on. First, `rescue::dispatch`
itself **seals** (caps PCR-11 and closes TPM-unsealed mappers) on entry,
so reaching any rescue mode is dominated by the lock. Second, a
`PolicyRefused` error (a failed signature, priority-file, or staged check
under enforce) bypasses the rescue dispatcher entirely and routes to the
non-interactive refuse terminus described under **Verified loading,
measurement, and lock-on-rescue** — which relocks storage and caps the
TPM before offering only reboot-into-rescue.

### Rescue dispatch

```
 any phase returns Err
        |
        v
 shell::drop_to_emergency(config, err)
        |
        v
 rescue::dispatch(config, console, cause)   (Result<TerminalAction>)
        |
        +-- mode = "embedded" -----------+
        |                                |
        |                                v
        |                       TerminalAction::Execve(cfg.paths.shell)  // /bin/sh from initramfs
        |
        +-- mode = "external"  (rescue::dispatch_external)
        |        |
        |        v
        |   rescue::disk::prepare_disk_rescue
        |     locate_sfs(config)                                // <runtime_boot_mountpoint>/<sfs_path>
        |     allocate_loop_device   (LOOP_CTL_GET_FREE)
        |     open_loop_device       (/dev/loopN, O_RDWR)
        |     open(sfs, O_RDONLY | CLOEXEC)
        |     configure_loop_device  (LOOP_CONFIGURE, RO)
        |     mount_overlay_root(/dev/loopN)                     // live-CD overlay:
        |        mount squashfs ro -> /run/nmbl-rescue/lower
        |        mount tmpfs       -> /run/nmbl-rescue/rw {upper,work}
        |        mount overlay     -> /rescue   (writable)
        |        |
        |        +-- success --> run_chrooted_external(/rescue)   // see below
        |        |
        |        +-- failure
        |              |
        |              +-- rescue.network = false --> halt_with_banner
        |              |
        |              +-- rescue.network = true (network-rescue feature)
        |                    |
        |                    v
        |              rescue::net::try_network_rescue
        |                ui::pick_source                         // ratatui (or console fallback)
        |                  [Network]/[Reboot]/[Halt]
        |                iface::enumerate + bring_up + wait_for_link
        |                dhcp::acquire   (DISCOVER/OFFER/REQUEST/ACK)
        |                apply_lease     (SIOCSIFADDR/SIOCSIFNETMASK/SIOCADDRT)
        |                ui::prompt_url  (pre-filled from rescue.defaultUrl)
        |                http::get  +  Sha256::update  -->  memfd_create
        |                ui::confirm_hash (pre-filled from rescue.defaultSha256)
        |                loop-mount the memfd, mount_overlay_root, run_chrooted_external
        |                  on hash mismatch / operator abort: loop back to source picker
        |                  on any fatal error: halt_with_banner
        |
        +-- mode = "none" ---------------> halt_with_banner
                                                  |
                                                  v
                                  TerminalAction::HaltWithBanner
                                  -> main: banner + reboot(RB_HALT_SYSTEM)
                                     fallback: libc::_exit(1)
```

The source-picker UI only appears in the network branch — disk
rescue is tried unconditionally first when `mode = "external"`. If
the disk path fails AND `rescue.network = true`, the network arm
shows the picker with the disk-failure reason embedded so the
operator knows why they were promoted to the network flow. Both
paths converge on the same writable `/rescue` overlay and the same
`run_chrooted_external` runner.

#### Chrooted child — NMBL stays PID 1

There is no `switch_root` / `execve` handoff. Instead of detaching
the initramfs and replacing PID 1, NMBL runs the rescue system as a
**chrooted child** while PID 1 stays put on the initramfs rootfs.
`run_chrooted_external` drops the live boot console (so its backend
`Drop` restores KD_TEXT/termios before the child paints), then crosses
into the async runtime via `block_on_tui_with_poller` and calls
`rescue::child::run_external_rescue_child`. Its sequence:

```text
// pre-fork, PID 1, safe nix wrappers (rescue::child::mount_plan):
mkdir -p /mnt /rescue/nmbl-root /rescue/mnt
bind        /rescue/mnt -> /rescue/mnt       // self-bind so it is a mount
make-shared /rescue/mnt                      // MS_SHARED
rbind       /rescue/mnt -> /mnt              // PID 1 observes the child's mounts
rbind       /           -> /rescue/nmbl-root // expose NMBL root + TUI socket

fork()
  // child — async-signal-safe only (mirrors sys::pty):
  chroot("/rescue"); chdir("/"); setsid()
  open("/dev/console"); dup2 -> 0,1,2
  execve(rescue /init, [argv0], [TERM, PATH, NMBL_TUI_SOCK])

  // PID 1 — reaps the child via the poller's non-blocking
  // waitpid(WNOHANG) op, CONCURRENTLY with the remote-attach server;
  // on child exit it tears the binds down (lazy MNT_DETACH) and
  // returns TerminalAction::Reboot.
```

The `/mnt` shared-subtree (bind → make-shared → rbind) lets the
child's `/mnt` mounts propagate back so PID 1 can observe them, and
the `rbind /` → `/rescue/nmbl-root` exposes NMBL's own root inside
the chroot — which is what makes the root-only TUI socket reachable at
`/nmbl-root/nmbl-run/tui.sock` from within the rescue system (see
**Async TUI and remote attach**).

A subtlety: NMBL's rescue squashfs uses `pkgs.busybox-sandbox-shell`
(a statically-linked busybox) rather than `pkgs.busybox`. The
dynamically-linked default would carry an ELF interpreter path like
`/nix/store/<hash>/lib/ld-musl-x86_64.so.1` that the `execve` after
`chroot` cannot resolve — the overlay root has no `/nix/store` tree.
A static binary has no interpreter, so `execve` succeeds.

## Async TUI and remote attach

The interactive menu, the emergency recovery menu, and the chrooted
rescue runner all share one async runtime so they can be driven
concurrently without threads. The remote-attach half is gated behind
the `remote-tui` Cargo feature.

### Single-threaded runtime

NMBL is one OS thread (PID 1, fork-safe), so the TUI runs on a tokio
**current-thread** `LocalRuntime` (`ui/runtime.rs`): every task is
`spawn_local`'d, no worker threads are ever spawned, and every existing
`fork()` site stays fork-safe. The synchronous orchestrator
(`main.rs`, `shell.rs`, the rescue dispatcher) crosses into the async
phase through `block_on_tui` / `block_on_tui_with_poller`.

Two mechanisms feed the runtime:

- The `Console` trait's `poll_event` is `async`: the real backends
  register the console fd with `tokio::io::unix::AsyncFd` and `.await`
  readiness (`ui/console/await_fd_readable`), so the menu yields the
  thread while idle.
- `sys/poller` is a custom single-threaded poller for syscall-style ops
  that have **no** async wrapper. It is `spawn_local`'d at runtime
  startup with a 1 ms tokio-timer pacer (`TokioPacer`). Its first real
  consumer is the rescue child reap: a non-blocking `waitpid(WNOHANG)`
  op (`sys/poller/waitpid.rs`) lets PID 1 await the chrooted child
  asynchronously, concurrently with the remote server.

### Root-only control socket

```
 PID 1 (server)                          non-PID-1 nmbl-init (client)
 ipc::tui_socket::bind_listener          ipc::tui_socket::connect_and_serve
   mkdir  /nmbl-run        (0700)          open /dev/tty (controlling terminal)
   bind   /nmbl-run/tui.sock (0600)        connect /nmbl-run/tui.sock
        |                                  sendmsg: Handshake (TERM, winsize)
        |                                    + SCM_RIGHTS pty fd
        v                                          |
 authenticate_and_receive                          |
   SO_PEERCRED  -> uid 0 ? --no--> "N" + reason ----+--> client prints, exits 1
        | yes                                       |
   recvmsg pty fd + handshake                       |
   write "K"  --------------------------------------+--> client goes quiescent,
        |                                                 blocks on socket EOF
        v
 serve_session: drive an independent TUI on the received pty
```

The socket dir is `0700`, the socket `0600`, and every peer is gated by
an `SO_PEERCRED` root check; the controlling-terminal pty is passed as
an `SCM_RIGHTS` fd. A **non-PID-1** `nmbl-init` invocation auto-detects
`getpid() != 1` (`main_parts/early_exit.rs`) → CLIENT mode: it connects,
passes its controlling terminal, and goes quiescent while PID 1 drives
an independent TUI on that pty. PID 1 itself (the boot path) never takes
this branch.

### Concurrent recovery sessions

In recovery (`shell/recovery.rs`), PID 1 runs the **local** emergency
menu AND a **remote** accept loop (`ui/remote/`) concurrently on the one
runtime, raced with `tokio::select!`; whichever produces a
`TerminalAction` first wins, the other is torn down, and the socket is
unlinked. Because the recovery state (`config`, the boot error) is
borrowed rather than `'static`, the per-connection futures cannot be
`spawn_local`'d (that bound is `'static`); instead the accept driver
holds the live session futures in a boxed `FuturesUnordered` and polls
`accept`, that set, and a wakeable `Shutdown` flag together each turn. A
stuck session just stays `Pending` and never starves `accept` — the same
guarantee `spawn_local` would give, without `'static` and without a
worker thread.

Each remote connection gets a **fully independent** session: its own
`SessionInteraction` latch and its own `TtyConsole` built on the
received pty — it never touches the local console's DRM/printk/stderr.
Within a remote session, `Ctrl+E` ends it with no action
(`app.exit_session`), and `Ctrl+L` opens a scrollable full-boot-log
viewer (a snapshot of the boot transcript, popped back with Esc /
`Ctrl+L`). A render/poll error on a remote pty means the client vanished
and just ends that session — it must never silently commit a
machine-wide `Reboot`.

### Attaching from inside the chrooted rescue

Because NMBL stays PID 1 during the external rescue and rbinds `/` →
`/rescue/nmbl-root`, the control socket is visible inside the chroot at
`/nmbl-root/nmbl-run/tui.sock`. The rescue system's init exports
`NMBL_TUI_SOCK=/nmbl-root/nmbl-run/tui.sock` and ships a `nmbl-tui`
shim (a `/bin` entry onto NMBL's own static binary, which auto-detects
non-PID-1 → client mode), so an operator who ssh'd into the running
rescue system can attach to NMBL's live TUI while the rescue runs. The
client resolves its socket path as `$NMBL_TUI_SOCK`, else
`/nmbl-run/tui.sock`, else the chroot view `/nmbl-root/nmbl-run/tui.sock`.

### Actions funnel back through PID 1

A remote session never `execve`s in PID 1. When it picks a terminal
action (reboot / shell / kexec) it stores the `TerminalAction` into a
shared first-committer `ActionSink` (`Rc<RefCell<Option<…>>>`) and
signals shutdown; the single `main::execute_terminal_action` site then
performs the no-return syscall after the stack unwinds, exactly as it
does for a locally-chosen action.

## Console backends and the graphical splash

The TUI is written once against a `Console` trait (`ui/console/`) and
runs over one of two backends, chosen at boot by `decide_backend`:

- **`TtyConsole`** — `ratatui` + `crossterm` on a hand-managed
  `/dev/console` fd (VT or serial). The default and the universal
  fallback.
- **`SplashConsole`** (`ui/console/splash/`, gated behind the
  `image-splash` Cargo feature and `boot.nmbl.splash.enable`) — renders
  the *same* `ratatui` screens onto a DRM/KMS framebuffer over a PNG
  background.

`open_console` is called once, after the early-module phase (so DRM
drivers are up), and the resulting `Box<dyn Console>` is threaded
through every later phase. Backend selection is fail-safe: a panic
recovery always forces `TtyConsole` (a panic may have come from splash
code); otherwise `SplashConsole::open` is tried and any failure — no
`/dev/dri/card*`, no usable connector, font-load error, framebuffer
allocation failure — logs a warning and falls back to `open_tty()`. A
serial console therefore always lands on the text TUI.

The splash render path reuses the existing widgets rather than
re-implementing them. Each frame: `ratatui` draws into a byte buffer via
`CrosstermBackend`; those vt100 bytes are fed through a headless
`alacritty_terminal::Term` (no PTY, no child) to recover a cell grid;
the compositor then blits the PNG background, draws a blurred contrast
"halo" under inked glyphs, and alpha-blends `fontdue`-rasterized glyphs
over it into an XRGB8888 dumb buffer (`src/splash/`). Keyboard input
comes from `/dev/tty1` in raw `K_XLATE` mode, with shift-state recovered
out-of-band via `TIOCLINUX` (the kernel VT collapses modified cursor
keys). The VT is put in `KD_GRAPHICS` mode and `log::set_tui_active`
suppresses the `nmbl_*!` stderr branch so kernel/userspace chatter never
smears the framebuffer. The grid is fixed by the DRM mode, so the splash
never emits `Resize`/`Scroll` events.

The splash uses the kernel's generic `simpledrm` / EFI framebuffer by
default. Real GPU framebuffers (virtio-gpu, amdgpu, …) deregister
`simpledrm`, so their DRM driver must be loaded via
`boot.nmbl.earlyKernelModules` before the console phase.

## Verified loading, measurement, and lock-on-rescue

When the `boot.nmbl.signing` / `tpm` / `secureBoot` options are set,
NMBL layers a full secure-boot posture on top of UEFI Secure Boot. The
machinery lives in `src/sig` (signatures), `src/tpm` (measurement + the
irreversible lock), `src/policy` (the gate, the seal guard, the refuse
terminus), and `src/boot/handoff.rs` (the ordered handoff).

### The handoff: verify → measure → load

`boot/handoff.rs::verify_measure_then_load` runs a fixed three-step
sequence on every generation boot, in this order and no other:

1. **VERIFY** — the chosen generation's kernel and initrd are checked
   against the baked trust anchor over pinned `O_RDONLY` fds, *before*
   anything is staged into the kexec slot. An enforce-mode failure
   returns `NmblError::PolicyRefused` and never loads.
2. **MEASURE** — the same SHA-512 digests (reused, never recomputed)
   plus an NMBL identity marker, the kexec cmdline, and each verified
   driver image are extended into PCR-11.
3. **LOAD** — `kexec_file_load(2)` is given the *same* verified pinned
   fd and the byte-identical cmdline that was measured.

Because verify and measure share one streamed hash over one pinned fd,
the bytes that are verified are exactly the bytes that are measured and
exactly the bytes that boot. The cpio fragment NMBL injects (the typed
passphrase, the boot log) is deliberately outside the verified and
measured set.

### Post-quantum signatures (`src/sig`)

Signatures are **FIPS-204 ML-DSA** (ML-DSA-65 default, ML-DSA-87
optional) via the pure-Rust `fips204` crate, over each blob's SHA-512
digest. Each signed blob carries a detached `NMBLSIG1` sidecar (a frozen
wire format shared with the host signer) holding the algorithm id, a
32-byte domain tag, and the signature. Verification recomputes the
role's domain tag and rejects a mismatch *before* touching any key, then
tries every baked key of the sidecar's algorithm, accepting on the first
valid signature and erroring only after all fail — fail-closed "any-of",
with no allow-unsigned path. Per-role domains (`nmbl:gen-kernel:v1`,
`nmbl:gen-initrd:v1`, `nmbl:driver-image:v1`, `nmbl:staged-fragment:v1`,
`nmbl:priority-file:v1`, `nmbl:rescue-sfs:v1`) mean a signature minted
for one role can never verify for another.

The trust anchor is a set of **public** keys compiled into the binary
(`src/sig/baked_keys.rs`, regenerated by the Nix builder
`mkNmblInit { publicKeys }`, which `include_bytes!`s each key and flips
`REQUIRE_KEYS`). It is committed empty; an enforcing build with zero
keys fails to compile. Because the anchor is in the binary, nothing on
the writable boot partition can swap it.

Signing happens at **install time** with the `nmbl-sign` host tool
(`nmbl-host-tools/`), reading a private key from a runtime *path string*.
An eval-time assertion in `lib/install-signing.nix` aborts the build if
any private key resolves under the Nix store, so the secret can never
become a derivation input. `signing.deferInstallSigning` skips the
in-installer signing step (for sealed disk-image builds that cannot see
the key) while leaving the baked anchor and runtime enforcement intact.

### Measured boot and the TPM cap (`src/tpm`)

`tpm/measure.rs` extends **PCR-11** with the handoff events in fixed
order (identity marker, kernel digest, initrd digest, cmdline, each
driver image), each event being `SHA-256(domain || 0x00 || body)`. A
host-side predictor reproduces the value off-box so an operator can seal
a secret to the PCR-11 the next measured boot will produce. NMBL itself
performs no `TPM2_Unseal`; LUKS auto-unlock is delegated to
`systemd-cryptenroll` + `cryptsetup --token-only` bound to PCR 11+7 (see
the README's *Sealing a LUKS volume to the TPM*).

The TPM **core** (`src/tpm/mod.rs`, `commands.rs`, …) is *always*
compiled — even with `secure-boot` off — because the lock-on-rescue cap
needs it. It talks to `/dev/tpmrm0` through the pure-Rust, no-C-FFI
`tpm2-protocol` crate. The cap is a TPM2 `PcrExtend` of a fixed poison
value `SHA-256("nmbl:relock-poison:v1")` into PCR-11; once extended, a
secret sealed to the pre-cap PCR-11 can no longer be unsealed until the
next reset.

### The seal-before-shell invariant (`src/policy/guard`)

`policy::seal_secrets` is the load-bearing security primitive. In strict
order it (1) caps PCR-11, (2) closes every registered TPM-unsealed LUKS
mapper (`cryptsetup close`), and only then (3) mints an unforgeable
`Sealed` token. The shell / PTY / remote-attach / `execve` spawn helpers
take `Sealed` *by value*, so a shell literally cannot be spawned without
having sealed first; the `nmbl-init-must-seal` flake check fails the
build if any spawn site lacks a `Sealed` witness (or an explicit
`// seal-exempt:` justification). The cap maps `Capped → proceed`,
`NoTpm → proceed only if !requireTpm`, and `Failed` (present but
uncappable) `→ always fail closed`. Every interactive terminus — the
emergency shell, the wrong-password recovery shell, the rescue handoff,
a remote session, and the policy refuse — flows through this guard, so
there is no path to a console that leaves the TPM open.

### The priority-file gate and refuse screen (`src/policy`)

`boot.nmbl.secureBoot` mounts a configured priority volume read-only and
verifies a signed file on it before NMBL will proceed to a measured or
staged boot. The gate runs at one of two phases — `PrePlainBoot` for a
file on the boot FS, or `PostUnlock` for a file on an inside-LUKS volume
that only appears after activation. On success it hands back an
`AttestedVolume` — an unforgeable witness with no public constructor,
consumed *by value* by the staged-boot apply path, so staged boot is
structurally unreachable on an unattested volume.

On a missing or wrong-signed file under enforce, the gate returns
`PolicyRefused`, which is *deferred* to one shared refuse terminus
(`policy::relock_and_refuse` → `policy::run_refuse_screen`) rather than
taken inline. That terminus, in order: caps PCR-11, closes every
TPM-unsealed mapper, **writes the rescue sentinel** (while the boot FS is
still writable), then relocks LUKS/LVM/mdraid (`cryptsetup close`,
`vgchange -an`, `mdadm --stop`). It then scrubs the boot log to a single
fixed banner and renders a non-interactive countdown whose only actions
are reboot-now and view-(scrubbed)-logs — no shell, no rescue handoff.
The terminal action is `RebootIntoRescue`, a variant constructible only
from a `Sealed` witness; the dispatcher fires `reboot(RB_AUTOBOOT)` only
after the whole stack has unwound (so every console/termios `Drop` ran).
Because the sentinel was written, the next boot lands in rescue.

### The rescue sentinel

`policy::should_force_rescue` is consulted at the very start of the boot
path, before the early-module phase. An empty sentinel file (default
`/boot/nmbl/rescue`) makes NMBL seal (cap PCR-11) and divert straight to
`rescue::dispatch`, skipping the measured boot entirely with the TPM kept
locked. The refuse terminus above writes this file, so a refused boot
reliably — and only — reaches rescue next cycle. Removing it restores
normal measured boot.

## Driver-image preload and staged boot

Two features add capability out-of-band, both held to the same signature
bar as the kexec target.

### Driver-image preload (`src/imageload`)

`boot.nmbl.driverImages` ships out-of-tree modules (and firmware) in
signed squashfs blobs on the boot partition. After the early-module
phase and before the console, `load_driver_images` walks each declared
image over a single pinned `O_RDONLY` fd: verify the detached signature
under `nmbl:driver-image:v1`, loop-mount the *same* fd read-only,
register the image's `lib/firmware` with the kernel's `firmware_class`
path (NMBL has no udev), and load the declared modules via the shared
module loader honouring per-image blacklists. Each loaded image's digest
feeds the PCR-11 measurement. On the normal pre-kexec terminus the images
are torn down in reverse order; on a divert to the capped emergency shell
they are deliberately left mounted for inspection (they carry no
secrets). Enabling driver images requires an active secure-boot posture —
a Nix assertion makes "load unsigned drivers" unbuildable.

### Staged boot (`src/staged`)

`boot.nmbl.staged` (Cargo feature `staged-boot`, which implies
`secure-boot`) lets a signed **config fragment** plus an extra-driver
image live on the inside-LUKS priority volume, invisible to an offline
attacker. After the `PostUnlock` gate yields its `AttestedVolume`,
`apply_staged_boot` consumes it by value and: verifies both the staged
image (`nmbl:driver-image:v1`) and the fragment
(`nmbl:staged-fragment:v1`) over pinned fds; parses the fragment
(`deny_unknown_fields`); **transactionally merges** it onto the base
`Config` — each table swapped in, the whole config re-validated, and on
any failure every table restored byte-for-byte; then re-runs the merged
effects in the same order as the base post-console phase (explicit
modules, then driver images appended to the shared accumulator, then
activations). The fragment may override any table *except* the
`signing` / `secure_boot` / `staged` policy tables, which are absent
from the fragment type — including one is a hard parse error — so a
fragment can neither relax enforcement nor re-point its own source. Any
failure maps to `PolicyRefused`, and the pristine-base guarantee means
the refuse relocks against an untainted config.

## Source layout

The Rust crate lives in `nmbl-init-rs/`. The binary entrypoint is
`src/main.rs`; all logic lives in modules under `src/lib.rs` so it can
also be unit-tested.

```
nmbl-init-rs/
|-- Cargo.toml             # -Oz, fat LTO, lint denies, feature flags
|-- flake.nix              # crane + fenix static-musl build + checks + key baking
|-- rust-toolchain.toml    # pinned stable + clippy + rustfmt
|-- nmbl-host-tools/       # `nmbl-sign` install-time host signer (workspace member)
`-- src/
    |-- main.rs            # arg parse, panic-hook install; thin entry
    |-- main_parts/        # boot driver, phase orchestration, client-mode early exit
    |-- lib.rs             # module roots
    |
    |-- config/           # serde TOML schema (split per table) + load / validate
    |-- error/            # NmblError enum + format_chain helper
    |-- log/              # quiet/info/verbose logger + tui-active stderr gate
    |-- panic.rs          # std::panic::set_hook + execve-into-recovery
    |-- security_consts.rs # single source: LOCK_PCR=11, poison, sentinel, countdown
    |-- terminal.rs       # TerminalAction (incl. Sealed-gated RebootIntoRescue)
    |
    |-- mount.rs          # phase 1: pseudo-filesystems
    |-- modules.rs        # phase 2: explicit kernel-module loader
    |-- activation/       # phase 3: LVM/mdraid/LUKS/ZFS orchestrator + TPM-mapper registry
    |-- devices/          # phase 3b: wait_for + mount system filesystems (+ loop-mount)
    |-- generations/      # phase 4: scan /nix/var/nix/profiles + gen-id
    |-- boot/             # phase 6: handoff.rs (verify -> measure -> kexec)
    |
    |-- sig/              # ML-DSA verify, NMBLSIG1 wire format, baked trust anchor
    |-- tpm/              # always-compiled TPM core + PCR-11 measure (secure-boot)
    |-- policy/           # priority gate, seal guard, sentinel, refuse screen, relock
    |-- imageload/        # driver-image verify -> loop-mount -> load modules
    |-- staged/           # staged-boot fragment verify, transactional merge, re-run
    |
    |-- shell/            # emergency-shell dispatch + recovery menu
    |-- rescue/           # rescue dispatch (embedded/external/none) + chrooted child
    |-- ipc/              # root-only TUI control socket (SO_PEERCRED + SCM_RIGHTS)
    |-- net/              # DHCP + HTTP/1.0 client for network rescue (network-rescue)
    |-- state/            # boot-attempt / stateful-fallback bookkeeping
    |-- validate/         # --validate-initrm dry-run entry
    |-- mocking/          # test/mock seams ; util/ # shared helpers
    |
    |-- ui/
    |   |-- mod.rs        # terminal lifecycle, supplier
    |   |-- app.rs        # state machine: List/Editing/Passphrase/Rescue
    |   |-- runtime.rs    # tokio current-thread LocalRuntime
    |   |-- console/      # Console trait + tty and splash backends
    |   `-- remote/       # remote-attach accept loop (remote-tui)
    |
    |-- splash/           # DRM/KMS framebuffer renderer + compositor (image-splash)
    |
    `-- sys/              # syscall wrappers (no policy)
        |-- mount.rs       # mount(2)/umount2(2), option-string parser
        |-- module.rs      # finit_module(2), modules.dep parser
        |-- kexec.rs       # kexec_file_load(2), reboot wrapper
        |-- loopdev/       # LOOP_CONFIGURE / LOOP_CTL_GET_FREE
        |-- pty/           # post-fork async-signal-safe pty spawn
        |-- poller/        # single-threaded poller (waitpid(WNOHANG), …)
        |-- ops/           # FsOps/TpmOps/ExecOps seams + dry-run impls
        |-- tty.rs         # open /dev/console + termios raw-mode guard
        `-- activation.rs  # pure fork/execve runner for activation tools
```

Grouped by role:

- **Syscall wrappers (`sys/*`)** — thin, policy-free. Anything in here
  is one syscall away from `libc`/`rustix`/`nix`.
- **Phase modules (`mount.rs`, `modules.rs`, `activation/`, `devices/`,
  `generations/`, `boot/`)** — boot stages in execution order. Each is
  invoked at most once per boot from the driver.
- **Secure boot (`sig/`, `tpm/`, `policy/`, `imageload/`, `staged/`,
  `security_consts.rs`)** — signature verify, TPM measure/cap, the
  priority gate + seal guard + refuse terminus, and the out-of-band
  driver/staged loaders. `tpm/` and `policy/` are always compiled (the
  lock-on-rescue cap is not optional); the rest are `secure-boot`-gated.
- **Console (`ui/*`, `splash/`)** — `ratatui` driven over the `Console`
  trait, on either the `/dev/console` text backend or the DRM splash
  backend; the runtime, remote-attach, and recovery menus live here.
- **Orchestration (`main.rs`, `main_parts/`, `lib.rs`)** — phase driver,
  panic-recovery branch, decision dispatch, client-mode early exit.
- **Cross-cutting (`config/`, `error/`, `log/`, `panic.rs`,
  `terminal.rs`, `shell/`)** — read by every phase.

## Module dependency graph

```
                +--------+
                |  main  |
                +---+----+
                    |
       +------------+------------+--------------+
       |            |            |              |
       v            v            v              v
   panic.rs   config.rs     run_phases     select_and_act
       |            \         /    \             |
       |             \       /      \            v
       |              v     v        v       generations.rs
       |          mount.rs modules.rs        |
       |              |    activation.rs --->|
       |              |     |   devices.rs   v
       |              |     |     |        ui/ (App, view, timeout,
       |              v     v     v               TuiPasswordSupplier)
       |          +--------- sys/ ---------+      |
       |          | mount  module  uname   |      |
       |          | kexec  tty     activation     v
       |          +-------------------------+   boot.rs
       |                                          |
       +----- shell.rs <--- (any phase Err) ------+
                  ^                               |
                  |                               v
              error.rs                       sys/kexec.rs
```

The graph shows the historical flat module names and the core
generation-boot path; several nodes are now directories (`config/`,
`boot/handoff.rs`, …) and the secure-boot clusters (`sig/`, `tpm/`,
`policy/`, `imageload/`, `staged/`) hang off the driver and the
`boot/handoff.rs` waist — but the dependency shape is unchanged.

Notable contracts:

- `main` is the only caller of `install_panic_hook` and
  `drop_to_emergency`; `kexec_into` is invoked only through
  `select_and_act` dispatch, which itself is only called from `main`.
- `activation.rs` calls `devices::wait_for` after each activation step
  so the next phase finds the new `/dev/mapper/...` nodes ready.
- `boot.rs` calls `devices::resolve_mountpoint` for its unmount targets
  so the teardown path can't drift from the mount path.
- The TUI's `TuiPasswordSupplier` lives in `ui/mod.rs` but implements
  the `PasswordSupplier` trait defined in `activation.rs`, so the
  activation orchestrator depends only on the trait.

## Runtime configuration

Everything `nmbl-init` needs at runtime is read from a single TOML file
embedded in the initramfs at `/etc/nmbl/config.toml`. There are no CLI
flags except `--config=<path>` and `--errored=<path>` (the latter set
by the panic hook itself), and no environment variables.

### Generation

The file is generated at Nix evaluation time by `lib/config-toml.nix`,
which serializes the relevant subset of `config.boot.nmbl` plus the
resolved `fileSystems` list and computed activation blocks into TOML
via `pkgs.formats.toml`. The result is then placed in the initramfs
by `lib/config.nix`:

```nix
system.build.nmblInitramfs = pkgs.makeInitrd {
  contents = [
    { object = "${nmblInit}/bin/nmbl-init"; symlink = "/init"; }
    { object = nmblConfigToml;              symlink = "/etc/nmbl/config.toml"; }
    { object = "${pkgs.busybox}/bin/busybox"; symlink = "/bin/sh"; }
    { object = "${kernelModulesManager.modulesClosure}/lib/modules";
      symlink = "/lib/modules"; }
    { object = kernelModulesManager.modprobeConf;
      symlink = "/etc/modprobe.d/nixos.conf"; }
  ] ++ cfg.activation.extraContents;
  compressor = "gzip -9";
};
```

### Round-trip

A NixOS `nixos-rebuild` regenerates the TOML and rebuilds the
initramfs, but it does **not** rebuild the `nmbl-init` binary unless
the crate source changes. Config-only changes are cheap.

### Schema

The schema is defined in the modules under `src/config/` as
`#[derive(serde::Deserialize)]`
structs with `#[serde(deny_unknown_fields)]`, so a typo fails the boot
loudly rather than silently doing the wrong thing. Top-level tables:

| Table             | Fields                                                    | Purpose                                       |
|-------------------|-----------------------------------------------------------|-----------------------------------------------|
| `general`         | `verbosity`, `timeout_secs`, `panic_report_dir`, `serial_console` | log level, TUI auto-boot, panic file location, serial-mode toggle |
| `kernel_modules`  | `explicit`, `blacklist`, `modules_dir`                    | which modules to `finit_module` and where to find them |
| `filesystems[]`   | `device`, `mountpoint`, `fstype`, `options`, `is_root`    | system filesystems to mount under `paths.system_root` |
| `activations[]`   | `kind`, `required_modules`, `binary`, `argv`, `produces_devices`, `description`, `prompt_label?` | per-kind activation step |
| `tui`             | `enable_editor`, `show_kernel_params`                     | TUI feature toggles                           |
| `paths`           | `nix_profiles_dir`, `system_root`, `shell`                | where to scan for generations, where to mount system, what to exec on failure |
| `splash`          | `enable`, `background_*`, font paths                      | graphical-splash backend (`image-splash`) |
| `signing`         | `enable`, `enforce`, `algorithm`, `sig_path_suffix`       | signature-verify policy (public keys are baked into the binary, not here) |
| `secure_boot`     | `enable`, `enforce`, `priority_volume`, `signed_file_path`, `allowed_key_ids`, `sentinel_path` | priority-file gate + refuse policy |
| `tpm`             | `measure`, `pcr_index`, `require_tpm`, `device`           | measured-boot / TPM-cap policy |
| `staged`          | `enable`, `image`, `fragment`, `sig`                      | staged-boot source pointers |
| `driver_images`   | `enable`, `images[]`                                      | driver-image preload list |

`activations[].kind` is one of `lvm`, `mdraid`, `luks-tpm`,
`luks-keyfile`, `luks-password`, `zfs` (`#[serde(rename_all = "kebab-case")]`).

The `splash`, `signing`, `secure_boot`, `staged`, and `driver_images`
tables are emitted only when their feature is enabled — `nmbl-init` is
built without the matching Cargo feature otherwise, and its
`deny_unknown_fields` parser would reject a table it does not understand.
The signing/secure-boot **public keys** are never in the TOML: they are
compiled into the binary (see *Verified loading*).

### Validation

`Config::load` parses the file, then calls `Config::validate`, which
rejects any `filesystems[].device` starting with `LABEL=`, `UUID=`, or
`PARTUUID=` — NMBL runs without udev, so the `/dev/disk/by-*` symlinks
are never populated. Devices must be raw `/dev/*` paths.

If config loading fails entirely, `main` falls back to
`Config::recovery_default()` and routes the load error straight to the
emergency shell so the operator still has a working `paths.shell` to
land on.

## Storage activation hooks

NMBL only knows how to call `mount(2)`. Anything that has to happen
between "kernel module loaded" and "device node exists" — LVM volume
activation, LUKS unlock, mdraid assemble, ZFS pool import — runs as an
external static binary fork+exec'd by the activation orchestrator.

### Mechanism

1. The Nix module `lib/modules/activation.nix` inspects
   `config.fileSystems` (looking for `/dev/mapper/*`, `/dev/md*`,
   `fsType = "zfs"`) plus explicit `boot.nmbl.activation.*` options.
2. From that it computes four things, each consumed elsewhere:
   - `cfg.activation.activationBlocks` — the `[[activations]]` rows in
     the TOML.
   - `cfg.activation.extraKernelModules` — module names merged into the
     initramfs module set (so e.g. `dm_mod` lands in `/lib/modules`).
   - `cfg.activation.extraContents` — `makeInitrd` content entries for
     the static tool binaries (`cryptsetup`, `vgchange`, `mdadm`,
     `zpool`/`zfs`).
   - `cfg.activation.assertions` — fail the eval if a requested tool
     is unavailable, or warn if only the dynamic build is on the
     system.
3. At runtime, `activation::run_all_activations` walks the blocks in
   declaration order. For each block it:
   - warns about any `required_modules` not present in `/proc/modules`
     (built-in modules don't show up there, so a hard check would
     false-positive),
   - prompts the `PasswordSupplier` for `luks-password` and pipes the
     bytes into the child's stdin (held in `Zeroizing<Vec<u8>>` so it
     wipes on drop),
   - forks and execs the binary via `sys::activation::run` (a
     non-zero exit is fatal),
   - polls every `produces_devices` path until it appears as a
     block-or-char device, with a 15-second per-device budget.

The orchestrator is policy; `sys::activation` is pure mechanism (a
fork/exec helper that can pipe stdin and translate `WaitStatus` into a
`ProcessOutcome`).

### Per-kind matrix

| Kind            | Activation command                                       | Kernel modules (auto-added)                 | Where the device appears |
|-----------------|----------------------------------------------------------|---------------------------------------------|--------------------------|
| `lvm`           | `vgchange -ay`                                           | `dm_mod`                                    | `/dev/mapper/<vg>-<lv>`  |
| `mdraid`        | `mdadm --assemble --scan`                                | `md_mod`, `raid0`, `raid1`, `raid10`, `raid456` | `/dev/md*`           |
| `luks-tpm`      | `cryptsetup open --token-only <dev> <name>`              | `dm_mod`, `dm-crypt`, `aes`, `tpm_crb`, `tpm_tis` | `/dev/mapper/<name>` |
| `luks-keyfile`  | `cryptsetup open <dev> <name> --key-file=<file>`         | `dm_mod`, `dm-crypt`, `aes`                 | `/dev/mapper/<name>`     |
| `luks-password` | `cryptsetup open <dev> <name> --key-file=-` (stdin)      | `dm_mod`, `dm-crypt`, `aes`                 | `/dev/mapper/<name>`     |
| `zfs`           | `zpool import -N <pool>`                                 | `zfs`                                       | `/<pool>` (mounted later)|

The blocks are emitted in dependency order: mdraid first (lowest level),
then LVM, then LUKS, then ZFS — so a `/dev/mapper` device backed by an
mdraid array is producible by the time a later filesystem entry asks
for it.

## Error handling and emergency shell

### `NmblError`

All fallible code returns `Result<T, NmblError>`. The enum lives in
`src/error.rs` and is `#[derive(thiserror::Error)]`. Current variants:

- `Config { source: toml::de::Error, path }` — TOML parse failure.
- `Io { source: std::io::Error, context }` — anything that wraps an
  IO error; `context` names the operation.
- `ConfigInvalid { reason, context }` — schema-valid but rejected by
  `Config::validate`.
- `Mount { src, dst, fstype, source }` / `Umount { dst, source }`.
- `Module { name, path, source }` — `finit_module` failed.
- `KexecLoad { kernel, initrd, source }` / `KexecReturned { stage, source }`.
- `DeviceTimeout { device, timeout_ms }`.
- `NoGenerations { searched }`.
- `Tui { source }` — anything bubbled out of `ratatui`/`crossterm`.
- `Activation { kind, source: Box<NmblError> }` — wraps an inner
  error with which activation step failed.
- `Panicked { report_path }` — synthesized by the recovery path.
- `Shell { source }` — `execve` of `/bin/sh` itself failed.

Discipline:

- No `unwrap` / `expect` / `panic!` / `todo!` / `unimplemented!` /
  `unreachable!` / `indexing_slicing` / `dbg!` in production. Enforced
  by clippy denies in `Cargo.toml` (the test modules opt back in with
  `#[allow(clippy::expect_used, …, reason = "tests can panic")]`).
- Every `?` adds context — bare `errno` errors are useless to a sleep-
  deprived operator at 3am.

### Drop-to-emergency contract

`shell::drop_to_emergency(config, err) -> Infallible`:

1. Prints a banner with the full `format_chain(err)` walk
   (`Error::source` chain).
2. Prints a one-line variant-specific suggested action (an exhaustive
   `match` on `NmblError`, so a new variant becomes a compile error
   here rather than a silently missing diagnostic at boot).
3. `execve("/bin/sh", ["sh"], &[])`. The shell inherits PID 1.

If `execve` itself fails (shell binary missing, ENOEXEC, EACCES), the
function prints one last diagnostic and calls
`reboot(RB_HALT_SYSTEM)`. The return type is `Infallible` so any caller
that drops the result fails to compile.

## Panic recovery

A panicking PID 1 has unknown state (terminal in raw mode, partial
mounts, leaked console fd). NMBL's recovery path is built around
`execve(2)` to reset the process while keeping PID 1.

```
 main()
   |
   | install_panic_hook()  --- first thing
   v
 (normal phases)
   |
   | --- panic somewhere ---
   v
 hook: build_report(info)             # location, payload, timestamp
   |   write_report("/run/nmbl-panic-<pid>.txt")   # best-effort
   v
 execve("/proc/self/exe", [argv0, "--errored=/run/nmbl-panic-<pid>.txt"])
   |
   v
 main()  re-entered (same PID, fresh stack, fresh fd table)
   |
   | sees --errored=<path>, branches into recover_from_panic
   v
 read report, load config leniently, log everything,
 drop_to_emergency(NmblError::Panicked { report_path })
```

Why `execve` instead of unwinding: it cleans up the process without
the kernel panicking over "init terminated unexpectedly". The PID is
preserved by the syscall, so the kernel never notices PID 1 died.

If the report write fails the hook re-execs with
`--errored=<missing>` so the recovery branch still runs. If the
`execve` itself fails the hook calls `libc::_exit(1)` — at that point a
kernel panic is the documented worst case.

The hook is installed once at the top of `main` and is **not**
re-installed in the recovery branch: a panic *during* recovery should
crash hard, not loop.

## Project rules

- **Static musl, `-Oz`, fat LTO, single codegen unit, stripped.** The
  release profile in `Cargo.toml` plus `+crt-static` in
  `flake.nix`'s `CARGO_BUILD_RUSTFLAGS`. The binary must stay under
  ~1 MiB stripped; `cargo-bloat` is shipped in the devshell.
- **No `unwrap` / `expect` / `panic!` / `todo!` / `unimplemented!` /
  `unreachable!` / `indexing_slicing` / `dbg!` in production code.**
  Enforced by `[lints.clippy]` in `Cargo.toml` and by the
  `nmbl-init-clippy` flake check (`--all-targets -- --deny warnings`).
- **Minimize `unsafe`.** `rustix` is preferred over `nix` for fd-safe
  primitives; `nix` is used where its coverage is broader (mount,
  module, reboot, uname). Every remaining `unsafe` block carries a
  `// SAFETY:` comment explaining why the safe wrappers don't apply.
  Run `grep -RIn unsafe src/` to enumerate; current blocks are:
  - `unsafe { fork() }` in `sys/activation.rs` plus the two
    `libc::_exit` post-fork child / exec-failed bailouts in the same
    file.
  - `libc::syscall(SYS_kexec_file_load, …)` in `sys/kexec.rs` — neither
    `nix` nor `rustix` wraps this syscall.
  - `libc::syscall(SYS_finit_module, …)` in `sys/module.rs` — same
    reason; the syscall is wrapped by no portable Rust crate.
  - `libc::_exit(1)` in `panic.rs` (halt fallback when re-exec fails)
    and `shell.rs` (halt after the emergency-shell exec failure).
  - **`src/splash/` adds zero new NMBL-side `unsafe`.** The
    `image-splash` Cargo feature pulls in `drm`, `png`, `fontdue`, and
    `alacritty_terminal`; the latter contains three `unsafe` blocks we
    execute (`TabStops::clear_all`, `grid::storage::swap`,
    `Poller::register`), all optimization-justified and accepted as
    vendored dependencies. Splash DRM bring-up uses a
    closure-shaped `SplashDrm::render(|fb, dims| …)` rather than
    storing the dumb-buffer mapping next to the card, so the
    self-reference problem that would have required a lifetime
    `transmute` doesn't arise.
- **`std::process::Command::` and any `execve(` require an inline
  `// execve safety: <why>` comment** on, or directly above, the call.
  Enforced by the `nmbl-init-no-exec` flake check, which scans the source
  tree and fails the build on any unjustified hit.
- **Security invariants are enforced by flake checks, not convention.**
  `nmbl-init-must-seal` requires a `Sealed` witness (or a
  `// seal-exempt:` justification) in any function that spawns a shell;
  `nmbl-init-no-cap-bypass` requires a `// cap-exempt:` / `// signing
  safety:` comment on any TPM-cap degrade or signature-verify downgrade;
  `nmbl-init-security-consts` pins the lock PCR, poison preimage,
  sentinel path, and refuse countdown byte-for-byte against
  `lib/security-consts.nix`.
- **Cargo features are additive and audited.** `secure-boot` pulls in
  `fips204` + `sha2`; `staged-boot` implies `secure-boot`; `image-splash`
  pulls in `drm`/`png`/`fontdue` and reuses `alacritty_terminal`;
  `network-rescue` and `remote-tui` are independent. The TPM core and the
  `policy` seal/relock terminus are *always* compiled. Nix store dedup
  keeps the binary byte-identical across configs that don't change the
  feature set.
- **`overflow-checks = true` in release.** Aborting on overflow is
  strictly better than silently wrapping.
- **Empty environment when exec'ing children.** Activation tools are
  NixOS-built static binaries that don't depend on `PATH` or locale,
  and PID 1's env is barely populated anyway.

## Test surface

Unit tests live in `#[cfg(test)] mod tests` blocks alongside each
module, covering (among much else): `modules.dep` parsing, mount
option-string folding, generation-link sorting, TUI key handling,
panic-report shape, emergency-shell suggested-action rendering, and —
for the secure-boot surface — ML-DSA verify round-trips and the
domain-cross-reject property (`sig/`), golden PCR-11 measurement vectors
and the poison self-check (`tpm/`), the staged-merge apply/rollback and
fragment-rejects-policy-tables tests (`staged/`), and the seal-order /
reach-rescue-is-cap-dominated tests (`policy/`). The suite has no
external services; `tempfile` is the only dev-dep. Run them with
`cargo test --all-features` (the default feature set skips the
secure-boot tests) inside the `nmbl-init-rs/` flake devshell, or via
`cargo nextest run`.

VM smoke tests live under `sirati-nmbl/testing/` and drive
`vm-serial-man` via flake apps (`nix run .#test-*`) defined in
`sirati-nmbl/flake.nix`. They build a full NMBL initramfs, boot it under
QEMU, and assert on serial-console output. The secure-boot scenarios
(`test-secure-boot`, `-driver`, `-staged`, `-tpm-roundtrip`, `-enroll`,
`check-sb-unsigned-uki`) add a software TPM (swtpm) and a
Secure-Boot-enforcing OVMF, install via `nixos-anywhere`, and assert on
signed/refused boots. One observability caveat shapes these tests: while
NMBL holds the interactive console it suppresses its `nmbl_*!` markers
from live serial, so `signature verified` is read from the post-kexec
`nmbl-init` journal, the refuse markers (console dropped first) are read
from serial, and the PCR-11 measurement is proven by the golden-vector
unit tests + the TPM round-trip rather than a serial line.

## Status and roadmap

### Working in v1

- Static-musl Rust `nmbl-init` builds via crane, embedded into the
  initramfs as `/init` alongside the runtime TOML.
- All seven phases (pseudo-fs, modules, activations, system filesystems,
  generation scan, TUI, kexec) wired end-to-end — `run_phases` covers
  1, 2, 3, and 3b; `select_and_act` runs 4, 5, and the dispatch into
  phase 6 (kexec, emergency shell, or reboot).
- TUI: list view, kernel-parameter editor (`e`), passthrough toggle
  (`p`), emergency-shell exit (`s`), reboot (`q`), auto-boot
  countdown with key-cancel and per-state dirty redraw.
- Serial-console fallback (line-oriented) when
  `general.serial_console = true`.
- Auto-detected activations for LVM / mdraid / ZFS, plus explicit LUKS
  with TPM / keyfile / passphrase unlock kinds.
- Panic hook with `execve`-into-recovery.
- Emergency shell with chained error display + variant-specific hints.
- `nmbl-init-no-exec` flake check enforcing inline `// execve safety:` justifications.

### Implemented post-v1

- **External configuration on the boot partition.**
  `boot.nmbl.configLocation = "external"` ships a tiny
  `/etc/nmbl/bootstrap.toml` inside the initramfs and reads the full
  `config.toml` from the boot partition at Phase 0.5. Operators can
  edit `/boot/nmbl/config.toml` directly and reboot to apply changes.
- **External rescue squashfs.**
  `boot.nmbl.rescue.mode = "external"` builds `nmbl-rescue.sfs` at
  install time from `rescue.squashfsContents` (default:
  `busybox-sandbox-shell`, `cryptsetup`, `lvm2`, `mdadm`) and stages
  it on the boot partition. The Rust /init loop-mounts it on demand
  via `LOOP_CTL_GET_FREE` + `LOOP_CONFIGURE`, layers a writable
  overlay (tmpfs upper) at `/rescue`, and runs its `/init` as a
  chrooted child while NMBL stays PID 1. `rescue.mode = "none"` halts
  with a structured banner instead.
- **Network rescue fallback.** `boot.nmbl.rescue.network = true`
  enables the `network-rescue` Cargo feature, bundles NIC drivers +
  DHCP + an HTTP/1.0 client, and offers an in-band download flow
  (with operator-confirmed SHA-256) when the disk copy of
  `nmbl-rescue.sfs` is unavailable.
- **Graphical splash.** `boot.nmbl.splash.enable` renders the same
  `ratatui` menu on a DRM/KMS framebuffer over a PNG background
  (`image-splash` feature), with transparent fallback to the text TUI on
  serial or any failure.
- **Post-quantum generation signing.** `boot.nmbl.signing` verifies each
  generation's kernel+initrd with FIPS-204 ML-DSA against keys baked into
  the binary; fail-closed under `enforce`, signed at install time by
  `nmbl-sign`, the private key never a derivation input.
- **Measured boot + lock-on-rescue.** `boot.nmbl.tpm.measure` extends
  PCR-11 with the exact handoff before kexec; every shell/rescue path
  first caps PCR-11 and closes TPM-unsealed mappers, enforced by the
  `Sealed` type and the `nmbl-init-must-seal` check.
- **Priority-file gate + rescue sentinel.** `boot.nmbl.secureBoot`
  verifies a signed file before measured/staged boot; a missing/bad file
  relocks LUKS, caps the TPM, writes the sentinel, and offers only
  reboot-into-rescue. An empty `/boot/nmbl/rescue` forces
  straight-to-rescue with the TPM kept locked.
- **Driver-image preload + staged boot.** `boot.nmbl.driverImages` loads
  signed out-of-band modules before kexec; `boot.nmbl.staged` merges a
  signed config fragment from behind LUKS — both held to the same
  signature bar.

### Roadmap

- **erofs as the preferred read-only image format.** NMBL currently
  builds and loop-mounts every image it owns (rescue, driver-image,
  staged) as squashfs; the Rust mounts hardcode `"squashfs"` and the Nix
  builders call `mksquashfs`. The next goal is to support — and prefer —
  **erofs** for these images (smaller, faster random reads, mature
  mainline support), keeping squashfs as a fallback. erofs is already a
  valid generic `fileSystems` fsType for operator-provided volumes (the
  fstype is passed verbatim to `mount(2)`); this goal is about the images
  NMBL produces and mounts itself.

### Deferred

- `LABEL=` / `UUID=` / `PARTUUID=` device specifiers in the operator's
  full config (currently rejected by `Config::validate`; the Phase 0.5
  blkid sweep populates `/dev/disk/by-*` only for the bootstrap stage's
  own boot device).
- Broader VM-level smoke coverage of the Rust path in the
  `testing/` harness; the existing scaffolding works but only a subset
  of configurations exercise activation and panic-recovery.
