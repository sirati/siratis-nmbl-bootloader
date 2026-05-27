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
   +----+----------------------------------------------------+
   | 1. mount pseudo-fs (/proc, /sys, /dev, /run, /tmp)      |
   | 2. load explicit kernel modules (finit_module)          |
   | 3. run storage activations (LVM / LUKS / mdraid / ZFS)  |
   | 3b. wait for + mount system filesystems under /mnt/system|
   | 4. scan /nix/var/nix/profiles/system-*-link             |
   | 5. ratatui TUI: countdown -> operator picks gen / shell |
   | 6. kexec_file_load + reboot(LINUX_REBOOT_CMD_KEXEC)     |
   +---------------------------------------------------------+
        |
        v
 chosen NixOS generation's kernel + initrd
        |
        v
 normal NixOS stage-1 / systemd
```

Any phase that returns `Err` routes to `shell::drop_to_emergency`,
which `execve`s `/bin/sh` (busybox) with PID 1 preserved.

## Source layout

The Rust crate lives in `nmbl-init-rs/`. The binary entrypoint is
`src/main.rs`; all logic lives in modules under `src/lib.rs` so it can
also be unit-tested.

```
nmbl-init-rs/
|-- Cargo.toml             # -Oz, fat LTO, lint denies
|-- flake.nix              # crane + fenix static-musl build + checks
|-- rust-toolchain.toml    # pinned stable + clippy + rustfmt
`-- src/
    |-- main.rs            # arg parsing, panic-hook install, phase driver
    |-- lib.rs             # module roots
    |
    |-- config.rs          # serde TOML schema + Config::load / validate
    |-- error.rs           # NmblError enum + format_chain helper
    |-- log.rs             # quiet/info/verbose macro logger
    |-- panic.rs           # std::panic::set_hook + execve-into-recovery
    |-- shell.rs           # emergency-shell exec site (one of three)
    |
    |-- mount.rs           # phase 1: pseudo-filesystems
    |-- modules.rs         # phase 2: explicit kernel-module loader
    |-- activation.rs      # phase 3: LVM/mdraid/LUKS/ZFS orchestrator
    |-- devices.rs         # phase 3b: wait_for + mount system filesystems
    |-- generations.rs     # phase 4: scan /nix/var/nix/profiles
    |-- boot.rs            # phase 6: build cmdline, unmount, kexec
    |
    |-- ui/
    |   |-- mod.rs         # terminal lifecycle, serial fallback, supplier
    |   |-- app.rs         # state machine: List/Editing/Passphrase
    |   |-- view.rs        # ratatui render functions
    |   `-- timeout.rs     # auto-boot countdown + tick callback
    |
    `-- sys/               # syscall wrappers (no policy)
        |-- mod.rs
        |-- mount.rs       # mount(2)/umount2(2), option-string parser
        |-- module.rs      # finit_module(2), modules.dep parser
        |-- kexec.rs       # kexec_file_load(2), reboot wrapper
        |-- uname.rs       # uname(2)
        |-- tty.rs         # open /dev/console + termios raw-mode guard
        `-- activation.rs  # pure fork/execve runner for activation tools
```

Grouped by role:

- **Syscall wrappers (`sys/*`)** — thin, policy-free. Anything in here
  is one syscall away from `libc`/`rustix`/`nix`.
- **Phase modules (`mount.rs`, `modules.rs`, `activation.rs`,
  `devices.rs`, `generations.rs`, `boot.rs`)** — boot stages in
  execution order. Each is invoked at most once per boot from `main`.
- **TUI (`ui/*`)** — `ratatui` + `crossterm` on top of a hand-managed
  `/dev/console` fd; falls back to line-oriented IO when
  `general.serial_console = true`.
- **Orchestration (`main.rs`, `lib.rs`)** — phase driver,
  panic-recovery branch, decision dispatch.
- **Cross-cutting (`config.rs`, `error.rs`, `log.rs`, `panic.rs`,
  `shell.rs`)** — read by every phase.

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

The schema is defined in `src/config.rs` as `#[derive(serde::Deserialize)]`
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

`activations[].kind` is one of `lvm`, `mdraid`, `luks-tpm`,
`luks-keyfile`, `luks-password`, `zfs` (`#[serde(rename_all = "kebab-case")]`).

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
    vendored per `docs/splash-research.md`. Splash DRM bring-up uses a
    closure-shaped `SplashDrm::render(|fb, dims| …)` rather than
    storing the dumb-buffer mapping next to the card, so the
    self-reference problem that would have required a lifetime
    `transmute` doesn't arise.
- **`std::process::Command::` and any `execve(` are forbidden outside
  three files: `src/shell.rs`, `src/panic.rs`, `src/sys/activation.rs`.**
  Enforced by the `nmbl-init-no-exec` flake check, which `grep`s the
  source tree and fails the build on any out-of-place hit.
- **`overflow-checks = true` in release.** Aborting on overflow is
  strictly better than silently wrapping.
- **Empty environment when exec'ing children.** Activation tools are
  NixOS-built static binaries that don't depend on `PATH` or locale,
  and PID 1's env is barely populated anyway.

## Test surface

Unit tests live in `#[cfg(test)] mod tests` blocks alongside each
module — currently ~78 tests covering: `modules.dep` parsing, mount
option-string folding, generation-link sorting, mountpoint resolution,
TUI key handling, passphrase supplier coercion, panic-report shape,
emergency-shell suggested-action rendering, `parse_loaded_modules`
edge cases, and (where the host filesystem cooperates) the `sys::*`
wrappers. The suite has no external dependencies — `tempfile` is the
only dev-dep.

Run them with `cargo test` inside the `nmbl-init-rs/` flake devshell,
or via `cargo nextest run`.

VM smoke tests live under `sirati-nmbl/testing/` and drive
`vm-serial-man` via flake apps (`nix run .#test-*`) defined in
`sirati-nmbl/flake.nix`. They build a full NMBL initramfs, boot it
under QEMU, and assert on serial-console output. Coverage of the new
Rust path inside these end-to-end runs is still being expanded.

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
- `nmbl-init-no-exec` flake check enforcing the exec allowlist.

### Deferred

- `LABEL=` / `UUID=` / `PARTUUID=` device specifiers (currently
  rejected by `Config::validate`; would need either udev or a
  blkid-style scanner in PID 1).
- External configuration on `/boot` (today the TOML is baked into the
  initramfs; an `/etc/nmbl/config.toml` override on the boot partition
  would let operators tweak timeouts without a rebuild).
- A pre-built squashfs rescue image as an additional emergency target
  when `/bin/sh` itself is broken.
- Broader VM-level smoke coverage of the Rust path in the
  `testing/` harness; the existing scaffolding works but only a subset
  of configurations exercise activation and panic-recovery.
