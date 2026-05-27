# nmbl-init — Rust port plan

This document is the source-of-truth design for replacing the bash-based NMBL
initramfs (`scripts/*.sh.nix` + busybox + kexec-tools + kmod) with a single
statically-linked Rust binary.

It is intended to be implemented in small, reviewable PRs that map 1:1 to the
sections of this document (see §11 Roadmap).

---

## 1. Goals and non-goals

### Goals

1. **Production-grade reliability.** No `panic!`, `unwrap`, `expect`, raw
   indexing, `todo!`, `unimplemented!`, or `unreachable!` anywhere in the
   shipping code path. Enforced by `clippy::deny` (see `Cargo.toml`).
2. **First-class error handling.** Every fallible operation returns
   `Result<_, NmblError>`. Errors carry enough context that the operator
   dropped into the emergency shell can fix the system without guessing.
3. **Zero external runtime dependencies.** The binary is the only executable
   the initramfs needs, with one exception: `busybox sh` (or any
   `/bin/sh`-equivalent) is `exec`d as the emergency shell. No `modprobe`,
   no `kexec`, no `mount`, no `cat`, etc. — all done via syscalls.
4. **Static musl, `-Oz`.** `x86_64-unknown-linux-musl` target, fat LTO,
   `panic = "abort"`, single codegen unit, stripped. Goal: < 1 MiB binary.
5. **Configuration without recompilation.** Everything currently injected by
   Nix string interpolation (`cfg.mountPrefix`, `cfg.kernelModules`,
   `cfg.fileSystems`, `cfg.timeoutSeconds`, `cfg.serialConsole`,
   `cfg.verbose`, `cfg.blacklistedKernelModules`, …) is read at runtime from
   a single TOML config file embedded in the initramfs. Changing options
   regenerates the config file but does **not** rebuild the binary.
6. **A TUI that does not suck.** Replace the "auto-boots in N seconds, no
   actual menu" current behavior with a real `ratatui`-based menu that
   supports keyboard selection, kernel-param editing, passthrough toggling,
   countdown abort, and an "emergency shell" exit.

### Non-goals

- We are not changing the Nix module's external interface
  (`boot.nmbl.*` options stay the same).
- We are not implementing the bootstrapper installer (`grub`/`systemd-boot`
  installation in `lib/install-bootloader.nix`). That runs on the *system*
  during `nixos-rebuild`, not inside the initramfs — it stays as Nix-built
  shell.
- We are not implementing arbitrary filesystem support — we just call
  `mount(2)` and let the kernel handle the filesystem driver (which the
  initramfs has loaded as a module).

---

## 2. Architectural overview

```
┌────────────────────────────────────────────────────────────────────┐
│                        NMBL Initramfs                              │
│                                                                    │
│   /init  ────────────►  nmbl-init   (static musl, PID 1)           │
│   /etc/nmbl/config.toml  ──┐                                       │
│                            └─►  read at startup                    │
│   /lib/modules/<ver>/…   (kernel modules, from makeModulesClosure) │
│   /bin/sh  ──────────────►  busybox sh   (ONLY for emergency exec) │
└────────────────────────────────────────────────────────────────────┘
                              │
                              │  kexec_file_load(2) + reboot(LINUX_REBOOT_CMD_KEXEC)
                              ▼
                     Selected NixOS generation
```

The binary runs through five sequential phases, each in its own module:

| Phase | Module       | Replaces                          |
|-------|--------------|-----------------------------------|
|   1   | `mount`      | `scripts/mount-and-kernel.sh.nix` (mounts only) |
|   2   | `modules`    | `scripts/mount-and-kernel.sh.nix` (modprobe block) |
|   3   | `devices`    | `scripts/mount-and-kernel.sh.nix` (device-wait loop) + system fs mount |
|   4   | `generations`| `scripts/find-generations.sh.nix`  |
|   5   | `ui`         | `scripts/selection-ui.sh.nix`      |
|   6   | `kexec`      | `scripts/kexec-boot.sh.nix`        |

Any failure in any phase produces a `NmblError`. The top-level handler in
`main` formats the error, calls `shell::drop_to_emergency(err)`, and only
that path may exec an external binary (`/bin/sh`).

---

## 3. Source tree

```
nmbl-init-rs/
├── flake.nix                 # Crane + fenix; builds static musl binary
├── rust-toolchain.toml       # pinned stable + clippy + rustfmt + rust-src
├── .cargo/config.toml        # default --target + +crt-static
├── Cargo.toml                # -Oz, panic=abort, lint denies
├── PLAN.md                   # this document
├── src/
│   ├── main.rs               # phase orchestration, top-level error sink
│   ├── error.rs              # NmblError enum + helpers
│   ├── log.rs                # tiny verbose/quiet logger (no `log` crate)
│   ├── config.rs             # TOML schema + loader
│   ├── sys/
│   │   ├── mod.rs
│   │   ├── mount.rs          # safe wrappers around mount(2)/umount2(2)
│   │   ├── module.rs         # finit_module(2) loader with dep resolution
│   │   ├── kexec.rs          # kexec_file_load(2) + reboot(2)
│   │   ├── uname.rs          # uname(2) for kernel version → /lib/modules/...
│   │   └── tty.rs            # termios raw mode, serial-friendly init
│   ├── mount.rs              # phase 1: pseudo-fs (proc/sys/dev/...)
│   ├── modules.rs            # phase 2: load explicit kernel modules
│   ├── devices.rs            # phase 3: poll for /dev/* then mount cfg fs
│   ├── generations.rs        # phase 4: scan /nix/var/nix/profiles
│   ├── ui/
│   │   ├── mod.rs            # phase 5 entry: spawn TUI app
│   │   ├── app.rs            # state machine, key handling
│   │   ├── view.rs           # ratatui draw fns
│   │   └── timeout.rs        # countdown w/ first-key cancellation
│   ├── boot.rs               # phase 6: build cmdline, unmount, kexec
│   └── shell.rs              # the only legal external-exec site
```

No `lib.rs`. Everything lives in the binary crate; no published API surface.

---

## 4. Crate inventory

Every dependency must justify its size cost and its trust footprint
(this code runs as PID 1 before the rest of the OS exists).

| Crate        | Purpose                                  | Notes |
|--------------|------------------------------------------|-------|
| `nix`        | safe syscall wrappers (mount, umount, sync, reboot, uname, termios, signal) | Default-features off; only enable `mount`, `term`, `reboot`, `fs`, `feature`, `process`, `signal`. |
| `libc`       | raw `syscall(SYS_finit_module, …)`, `syscall(SYS_kexec_file_load, …)` (not in `nix`) | Already a transitive dep. |
| `rustix`     | *alternative* to `nix` — pure-Rust, smaller. Evaluate during phase 1. | Decision deferred; see §12. |
| `ratatui`    | TUI renderer | `default-features = false, features = ["crossterm"]`. |
| `crossterm`  | terminal backend for ratatui; works over serial | We patch its tty detection so `/dev/console` works. |
| `serde`      | config deserialization | `derive`. |
| `toml`       | config file format | Smaller and saner than `serde_json` for human-edited files. |
| `thiserror`  | error type derive | Zero runtime cost. |

Explicitly **not** depended on: `anyhow` (we want enumerated, matchable
errors), `tokio` (we are strictly single-threaded), `tracing` /
`env_logger` / `log` (overkill; we ship our own one-screen logger),
any HTTP/JSON/async crate.

Decompression: kernel 5.17+ (we ship `linux_6_6`) supports
`MODULE_INIT_COMPRESSED_FILE` on `finit_module`, so the kernel
decompresses `.ko.xz`/`.ko.zst` for us. We therefore do **not** need
`xz2` / `zstd` / `flate2`, which would each cost ~100 KiB.

---

## 5. Configuration: `/etc/nmbl/config.toml`

### 5.1 Generation

Generated at Nix evaluation time by a new file `lib/config-toml.nix` that
serializes the relevant subset of `cfg` and the resolved `nmblFileSystems`
+ `explicitKernelModules`. The TOML is then dropped into the initramfs
under `/etc/nmbl/config.toml` (`makeInitrd { contents = [ … ]; }`).

### 5.2 Schema

```toml
# /etc/nmbl/config.toml — read by nmbl-init at PID 1.

mount_prefix       = "/mnt"
timeout_seconds    = 3
verbose            = false
serial_console     = "ttyS0,115200"   # null/omitted = vt console
emergency_shell    = "/bin/sh"        # the only external binary path

# Modules to explicitly load (resolved from boot.nmbl.kernelModules +
# config.boot.initrd.kernelModules, with blacklist applied at build time).
explicit_modules = [
    "virtio_pci",
    "virtio_blk",
    "ext4",
]

# Modules to refuse loading at runtime even if asked.
blacklist_modules = [
    "nouveau",
]

# All filesystems needed for boot, in mount order.
# Mirrors utils.fsNeededForBoot(config.fileSystems) from Nix.
[[filesystems]]
mount_point = "/"
device      = "/dev/vda2"
fs_type     = "ext4"
options     = ["ro"]          # already filtered of x-* / nofail / _netdev

[[filesystems]]
mount_point = "/boot"
device      = "/dev/vda1"
fs_type     = "vfat"
options     = ["rw"]          # /boot and /efi default rw, others ro

# Diagnostics-only: what the build thought it was doing. nmbl-init prints
# these into the TUI footer and into emergency-shell context, but does not
# act on them at runtime.
[build_info]
boot_mode      = "uefi"
loader         = "grub"
nmbl_version   = "0.1.0"
build_timestamp = "2026-05-26T00:00:00Z"
```

### 5.3 Parsing rules

- Unknown keys → `#[serde(deny_unknown_fields)]` so a typo fails closed.
- Missing required keys → typed error with the field name.
- `options` is a list of strings, **already filtered** by Nix to drop the
  systemd-only options (`x-initrd.mount`, `nofail`, `_netdev`, `x-systemd.*`).
  The Rust loader does no further filtering.

### 5.4 Determinism

The TOML file path is `/etc/nmbl/config.toml` and only that. No CLI flags,
no environment variables, no `/proc/cmdline` parsing — except one
documented kernel parameter:

- `nmbl.shell` on `/proc/cmdline` → skip the TUI, go straight to emergency
  shell. (Existing behavior from `selection-ui.sh.nix:18-33`.)

---

## 6. Error handling

### 6.1 The `NmblError` enum

```rust
// src/error.rs
#[derive(Debug, thiserror::Error)]
pub enum NmblError {
    #[error("config file {path} could not be read: {source}")]
    ConfigRead { path: PathBuf, #[source] source: std::io::Error },

    #[error("config file {path} is not valid: {source}")]
    ConfigParse { path: PathBuf, #[source] source: toml::de::Error },

    #[error("mount({src} -> {dst}, {fstype}) failed: {source}")]
    Mount { src: String, dst: PathBuf, fstype: String, #[source] source: nix::Error },

    #[error("required block device {device} did not appear within {timeout_ms}ms")]
    DeviceTimeout { device: PathBuf, timeout_ms: u64 },

    #[error("kernel module {name} (path {path}) failed to load: {source}")]
    ModuleLoad { name: String, path: PathBuf, #[source] source: nix::Error },

    #[error("no NixOS generations found under {profiles_dir}")]
    NoGenerations { profiles_dir: PathBuf },

    #[error("TUI failed: {0}")]
    Tui(#[from] std::io::Error),

    #[error("kexec_file_load failed (kernel={kernel}, initrd={initrd}): {source}")]
    KexecLoad { kernel: PathBuf, initrd: PathBuf, #[source] source: nix::Error },

    #[error("reboot(LINUX_REBOOT_CMD_KEXEC) returned (this should never happen)")]
    KexecReturned,
    // ... extend as phases land ...
}
```

### 6.2 Discipline

- **No `unwrap`, `expect`, indexing, or `panic!`.** Enforced by clippy
  denies in `Cargo.toml` and by the `--deny warnings` clippy check in
  `flake.nix`.
- **No `?` without context.** Every `?` wraps with a variant that names the
  offending path/device/module. We don't want bare `Errno(2)` errors.
- **Panic hook installed in `main`** before any work. Even though we should
  never panic, if a bug slipped through we want to print a stable banner and
  drop to the shell, not silently abort. Because we use `panic = "abort"`,
  `std::panic::set_hook` is the right tool — the hook runs to completion
  before `abort()` is called.
- **`overflow-checks = true`** in release. We prefer abort to silent wrap.

### 6.3 The "emergency shell" contract

`shell::drop_to_emergency(err: NmblError) -> !` is the only function
allowed to call `Command::exec` / `execve(2)`. It:

1. Prints a fat banner with the error chain (`std::error::Error::source`
   walk).
2. Prints the diagnostics that the current bash script's `fallback_shell`
   prints — `uname`, `/lib/modules` listing, block devices, mount table.
3. `execve("/bin/sh", &["sh"], &env)`. We must `execve`, not `spawn`, so
   the shell inherits PID 1.

A grep across the entire crate for `Command::` should yield exactly one
hit, inside `src/shell.rs`. A future CI grep can enforce this.

---

## 7. Phase-by-phase mapping

For each phase: the bash file being replaced, the Rust module replacing it,
the syscalls used, and the error variants it can produce.

### Phase 1 — Pseudo-filesystems  (`src/mount.rs`)

Replaces the top half of `scripts/mount-and-kernel.sh.nix:42-65`.

| FS | Source | Target | Type | Flags |
|----|--------|--------|------|-------|
| proc    | `proc`    | `/proc`     | `proc`     | `MS_NOSUID \| MS_NOEXEC \| MS_NODEV` |
| sysfs   | `sys`     | `/sys`      | `sysfs`    | `MS_NOSUID \| MS_NOEXEC \| MS_NODEV` |
| devtmpfs| `dev`     | `/dev`      | `devtmpfs` | `MS_NOSUID` |
| devpts  | `devpts`  | `/dev/pts`  | `devpts`   | `MS_NOSUID \| MS_NOEXEC` |
| tmpfs   | `tmpfs`   | `/tmp`      | `tmpfs`    | `MS_NOSUID \| MS_NODEV` |

Syscall: `nix::mount::mount`. Errors: `NmblError::Mount`.

### Phase 2 — Kernel modules  (`src/modules.rs` + `src/sys/module.rs`)

Replaces `scripts/mount-and-kernel.sh.nix:67-95`.

1. `uname(2)` → kernel release string `R`.
2. Open `/lib/modules/R/modules.dep`. Parse it once (tiny file). Build
   `name → (path, [dependency_names])` map.
3. For each `m` in `config.explicit_modules`:
   - Skip if `m` is in `config.blacklist_modules`.
   - Skip if `/sys/module/<m>` already exists.
   - Topologically order `m`'s dependencies, then load each via
     `finit_module(fd, "", flags)`:
     - `flags = 0` for `.ko`
     - `flags = MODULE_INIT_COMPRESSED_FILE` for `.ko.xz` / `.ko.zst`
       (kernel ≥ 5.17 — we ship 6.6).
   - If `finit_module` returns `EEXIST`, swallow it (already loaded).
   - Any other errno → `NmblError::ModuleLoad`.

`MODULE_INIT_COMPRESSED_FILE = 4`, defined in
`<linux/module.h>`. We hard-code the constant in `sys/module.rs` with
a comment citing the kernel header.

Syscalls: `finit_module(2)` via `libc::syscall(SYS_finit_module, …)`.
`nix` does not wrap this.

### Phase 3 — Device wait + system filesystems  (`src/devices.rs`)

Replaces `scripts/mount-and-kernel.sh.nix:97-185`.

1. Build the set of required block-device paths from
   `config.filesystems[*].device` where it starts with `/dev/`.
2. Poll-loop with `Instant::now()` budget (default 10 s, configurable):
   - `std::fs::metadata(dev)` to existence-check.
   - `std::thread::sleep(Duration::from_millis(25))`.
   - Once per real-time second, log "still waiting: a, b, c".
3. For each filesystem in declaration order:
   - `mkdir_p(mount_prefix + mount_point)`.
   - `mount(device, prefix+mountpoint, fs_type, flags_from_options, data_from_options)`.
   - Failure → `NmblError::Mount`. (This is fatal — the bash currently
     `exit 1`s here too, which trips the `EXIT` trap and drops to shell.)

### Phase 4 — Generation discovery  (`src/generations.rs`)

Replaces `scripts/find-generations.sh.nix` in full.

```rust
pub struct Generation {
    pub label:        String,          // "current" | "123"
    pub kernel:       PathBuf,         // already prefix-resolved
    pub initrd:       PathBuf,
    pub kernel_params: String,         // raw contents of kernel-params file
    pub system_link:  PathBuf,         // for diagnostics
    pub build_time:   Option<SystemTime>, // mtime of the link, for display
}
```

1. Read dir entries of `<prefix>/nix/var/nix/profiles/`.
2. Keep entries matching `system-<NUM>-link` (regex-free; trim prefix/suffix).
3. For each:
   - `readlink` → resolve symlink target.
   - If target is absolute, re-anchor under `mount_prefix`.
   - Open `<system>/kernel`, `<system>/initrd`, `<system>/kernel-params`.
     The first two are symlinks (verified by `symlink_metadata`); the
     params file is plain text.
4. Sort by parsed generation number, **descending**.
5. Prepend the `system` (current) symlink if it resolves successfully and
   is distinct from the highest-numbered system-link.
6. Empty result → `NmblError::NoGenerations`.

Path-prefix re-anchoring is identical to the bash `resolve_system_path`
function (`find-generations.sh.nix:30-48`), but done with `Path::join`
rather than string concat so trailing-slash bugs don't bite us.

### Phase 5 — TUI  (`src/ui/`)

Replaces `scripts/selection-ui.sh.nix` (which is, by the user's own
admission, terrible — it just sleeps and picks generation 0).

#### Library choice
`ratatui` + `crossterm`. Reasons:

- Most actively maintained Rust TUI (`tui-rs`'s successor).
- Pure Rust, no C deps.
- `crossterm` works with non-tty fds if we initialize termios manually,
  which is what we need for serial-console environments.

#### Layout (proposed)

```
┌── NMBL · NixOS Minimal BootLoader ────────────────────── 6.6.71 ──┐
│                                                                    │
│  Generations                                          (auto-boot 3) │
│  ┌──────────────────────────────────────────────────────────────┐  │
│  │ ▶ current   2026-05-26 14:02   /nix/store/xxx-nixos-system-… │  │
│  │   123       2026-05-25 09:11   /nix/store/yyy-nixos-system-… │  │
│  │   122       2026-05-24 18:30   /nix/store/zzz-nixos-system-… │  │
│  │   …                                                          │  │
│  └──────────────────────────────────────────────────────────────┘  │
│                                                                    │
│  Kernel parameters (editable: 'e')                                 │
│  ┌──────────────────────────────────────────────────────────────┐  │
│  │ init=/nix/store/…/init console=ttyS0,115200 quiet loglevel=4 │  │
│  └──────────────────────────────────────────────────────────────┘  │
│                                                                    │
│  [x] passthrough generation params   (p toggles)                   │
│                                                                    │
│  ↑/↓ select  Enter boot  e edit cmdline  p toggle passthrough     │
│  s shell     q reboot                                              │
└────────────────────────────────────────────────────────────────────┘
```

#### State machine

```rust
enum Screen {
    List { selected: usize },
    Editing { buffer: String, cursor: usize },
}

struct App {
    screen: Screen,
    generations: Vec<Generation>,
    passthrough: bool,         // include selected gen's own kernel_params
    cmdline_override: Option<String>,
    countdown: Option<Instant>, // Some until first key, then None
    decision: Option<Decision>,
}

enum Decision { Boot(usize), Shell, Reboot }
```

- Any keypress cancels `countdown`.
- `Enter` (or countdown expiry) sets `decision = Boot(selected)` and
  the run loop returns.
- `s` sets `decision = Shell` → emergency shell.
- `q` sets `decision = Reboot` → `reboot(LINUX_REBOOT_CMD_RESTART)`.

#### Serial console adaptation (`src/sys/tty.rs`)

- Open `/dev/console` (the kernel console — works whether the actual
  device is `tty1` or `ttyS0`).
- `tcgetattr` → save original termios.
- `cfmakeraw` → put into raw mode for ratatui.
- On exit (or panic hook), `tcsetattr` restores original termios so the
  user can see kernel messages in the emergency shell.

If `/dev/console` cannot be put into raw mode (very unusual), we fall
back to a line-based selection menu printed via `println!`. This keeps
the binary usable on extremely odd consoles.

### Phase 6 — Kexec  (`src/boot.rs` + `src/sys/kexec.rs`)

Replaces `scripts/kexec-boot.sh.nix` in full.

1. Build final cmdline string:
   - if `passthrough` and no override → `generation.kernel_params`
   - if `cmdline_override` set → use it verbatim
2. Re-anchor kernel/initrd paths under `mount_prefix` (same logic as
   bash's `resolve_file_path`, `kexec-boot.sh.nix:37-55`). Statically
   verify both files exist before touching kexec.
3. Open kernel + initrd as `O_RDONLY` file descriptors.
4. Invoke `kexec_file_load(kernel_fd, initrd_fd, cmdline.len()+1,
   cmdline_ptr, 0)` via `libc::syscall(SYS_kexec_file_load, …)`. Why
   `kexec_file_load` instead of `kexec_load`:
   - Kernel parses ELF/bzImage itself; no userspace parser.
   - Supports IMA / kexec signature verification (future hardening).
   - Available since Linux 3.17, so universal.
5. Unmount everything in *reverse* declaration order
   (`umount2(target, MNT_DETACH)`).
6. `sync(2)`.
7. Write `"3\n"` to `/proc/sys/vm/drop_caches` — matches current bash
   behavior. Use `OpenOptions::write(true)`, not a syscall, because this
   is a procfs file.
8. `reboot(LINUX_REBOOT_CMD_KEXEC)` — does not return on success. If it
   *does* return, that's `NmblError::KexecReturned` and we drop to shell.

---

## 8. Nix integration

The existing Nix wiring in `lib/config.nix:138-180` currently builds the
init script *as a string* via `scripts/script.nix`. The Rust port replaces
that with **two** Nix-level pieces:

1. A `nmblInitBin` derivation: `import nmbl-init-rs { inherit pkgs; }`
   wrapping `flake.nix:packages.default`. Output: a single
   `result/bin/nmbl-init` ELF.
2. A `nmblConfigToml` derivation: a new file `lib/config-toml.nix` that
   serializes the relevant subset of `cfg` and `nmblFileSystems` into TOML
   via `lib.generators.toTOML` and writes it with `pkgs.writeText`.

`config.nix` then becomes:

```nix
system.build.nmblInitramfs = pkgs.makeInitrd {
  contents = [
    { object = "${nmblInitBin}/bin/nmbl-init"; symlink = "/init"; }
    { object = nmblConfigToml;                 symlink = "/etc/nmbl/config.toml"; }
    { object = pkgs.busybox;                   symlink = "/bin/sh"; }  # emergency only
    { object = "${kernelModulesManager.modulesClosure}/lib/modules";
      symlink = "/lib/modules"; }
    { object = kernelModulesManager.modprobeConf;
      symlink = "/etc/modprobe.d/nixos.conf"; }
  ];
  compressor = "gzip -9";
};
```

The current `scripts/` directory becomes dead code and can be deleted in
the final cleanup PR. The Nix module options in `lib/options.nix` stay
**unchanged** — users keep configuring `boot.nmbl.*` exactly as today.

### Why busybox is still in the initramfs

Because the emergency shell promise still has to be honored, and it's the
*only* `/bin/sh`-providing thing we can drop in without dragging glibc.
`busybox` is ~1 MiB statically linked and gives the operator a full
toolbox once they land in the shell.

---

## 9. Boot-time error UX

Failures in the bash version drop to a "shell" that's actually `sh` with
some echoed hints. The Rust version does the same but with structured info.

```
╔══════════════════════════════════════════════════════════════════════╗
║  NMBL: BOOT FAILED — DROPPING TO EMERGENCY SHELL                     ║
╠══════════════════════════════════════════════════════════════════════╣
║  Phase:   devices                                                    ║
║  Error:   required block device /dev/nvme0n1p2 did not appear        ║
║           within 10000ms                                             ║
║  Caused by: (no source)                                              ║
║                                                                      ║
║  System state:                                                       ║
║    kernel:  6.6.71                                                   ║
║    modules: 23 loaded                                                ║
║    block:   /dev/vda  /dev/vda1  /dev/vda2                           ║
║    mounts:  /proc /sys /dev /dev/pts /tmp                            ║
║                                                                      ║
║  Suggested actions:                                                  ║
║    - inspect:   ls /sys/class/block/                                 ║
║    - load mod:  /init does not exist here; use insmod manually       ║
║    - resume:    exit  (re-runs from start of phase 'devices')        ║
╚══════════════════════════════════════════════════════════════════════╝
$
```

Where reasonable, the suggested-actions block is variant-specific (e.g.
the `ModuleLoad` variant lists the failed module and reminds the user
which modules `lib/modules` actually contains).

---

## 10. Testing strategy

| Layer | Approach |
|-------|----------|
| Unit  | `cargo nextest` — pure functions only (modules.dep parser, kernel-params splitter, generation sorter, options→mount-flag mapper). |
| Phase | Mock `/proc`, `/sys`, `/lib/modules`, `/nix/var/nix/profiles` via a `TempDir` and a trait-injected `Fs` for the high-level orchestrator. |
| TUI   | `ratatui`'s `TestBackend` — snapshot tests of the rendered frames per state. |
| Integration | The existing `testing/` harness already builds VMs with `vm-serial-man`. Plug `nmbl-init` into `system.build.nmblInitramfs` and reuse the existing `nix run .#test-*` apps. |

A new check is added: a CI grep that fails if `Command::` appears anywhere
outside `src/shell.rs`.

---

## 11. Roadmap (PR-sized chunks)

| PR | Scope | Exit criteria |
|----|-------|---------------|
| 1  | This document, `flake.nix`, `Cargo.toml`, skeleton `main.rs`. | `nix build .#packages.x86_64-linux.nmbl-init` succeeds; binary is < 200 KiB and runs `--help`. |
| 2  | `error.rs`, `config.rs`, `log.rs`. | `nmbl-init /path/to/config.toml` parses & echoes config; deny-list of clippy lints active. |
| 3  | `sys/mount.rs` + phase 1 (`mount.rs`). | Runs as PID 1 in a tiny test VM; mounts proc/sys/dev and exits cleanly. |
| 4  | `sys/module.rs` + phase 2 (`modules.rs`). | Loads `virtio_pci` + `virtio_blk` inside QEMU; `/sys/module/virtio_blk` exists afterwards. |
| 5  | Phase 3 (`devices.rs`) — wait + mount system filesystems. | Existing `test-mbr-serial` config boots through to "mounted" and exits. |
| 6  | Phase 4 (`generations.rs`). | Lists ≥1 generation on a working install. |
| 7  | `sys/kexec.rs` + phase 6 (`boot.rs`), still with the placeholder UI from PR 6 (auto-pick first). | `test-gpt-uefi-grub` kexecs into NixOS. |
| 8  | `sys/tty.rs` + phase 5 (`ui/`) ratatui MVP: list + countdown + Enter-to-boot. | Manual boot selection works in QEMU. |
| 9  | UI polish: editable cmdline, passthrough toggle, emergency-shell key. | All `selection-ui.sh.nix` features replicated. |
| 10 | Delete `scripts/*.sh.nix`, update `lib/config.nix` to use Rust binary by default. | All `nix run .#test-*` configurations pass. |
| 11 | Documentation pass: update `README.md` and `ARCHITECTURE.md`. | — |

PRs 1–7 can land before any user-visible change because `lib/config.nix`
keeps using the bash init until PR 10 flips the switch.

---

## 12. Open decisions

These are flagged for explicit review before implementation, not silently
made:

1. **`nix` vs `rustix`** — `rustix` is pure Rust and slightly smaller.
   `nix` has wider coverage of the syscalls we need. Recommendation:
   start with `nix` for breadth, revisit in PR 11 if size budget is tight.
2. **TOML vs JSON for config** — TOML is friendlier for humans hand-debugging
   in the emergency shell; JSON has a smaller Rust parser. Recommendation:
   TOML for ergonomics, the parser cost (~30 KiB) is acceptable.
3. **Should kernel-cmdline parsing be supported?** Currently we only honor
   `nmbl.shell`. Worth deferring; could add `nmbl.timeout=N`,
   `nmbl.default=<gen>` later without churning the config schema.
4. **Should we drop `busybox` entirely** and ship a self-built static `sh`
   replacement? Almost certainly not worth it: busybox is small, has a
   well-known interface, and the operator already knows it.

---

## 13. Size deltas

Measured against the four meaningful runtime configurations from
`testing/build_configurations.nix`. All numbers are bytes as reported
by `du -b` of the built store paths. The `initramfs` column is the
gzip-9 `initrd` blob shipped in `/boot`. The `nmbl-init` column is
the static-musl PID 1 ELF inside that initramfs. The `rescue.sfs`
column is the external zstd-19 squashfs staged at
`/boot/nmbl-rescue.sfs` (only present when
`boot.nmbl.rescue.mode = "external"`).

| Configuration                       | initramfs (bytes) | nmbl-init (bytes) | rescue.sfs (bytes) |
|-------------------------------------|------------------:|------------------:|-------------------:|
| embedded (back-compat default)      |        37,570,137 |         1,351,920 |                n/a |
| external-config                     |        37,570,319 |         1,351,920 |                n/a |
| external-rescue                     |        36,914,256 |         1,351,920 |            700,416 |
| external-rescue + network           |        37,546,091 |         1,810,704 |            700,416 |

Notes:

- **embedded** is `test-gpt-uefi-grub` — the back-compat baseline
  with `configLocation = "embedded"` and `rescue.mode = "embedded"`
  (the defaults). Everything ships inside the initramfs, including
  busybox under `/bin/sh`.
- **external-config** (`test-external-config`) keeps
  `rescue.mode = "embedded"` (busybox still on the initramfs) but
  moves the runtime config TOML onto the boot partition via
  `lib/install-bootloader.nix`. The +182-byte delta is just the
  bootstrap.toml header overhead vs the full config.toml — both
  files live under `/etc/nmbl/` and compress nearly identically
  with gzip -9.
- **external-rescue** (`test-external-rescue`) sets
  `boot.nmbl.rescue.mode = "external"` and bundles a squashfs at
  install time with `pkgs.busybox-sandbox-shell` + `pkgs.pkgsStatic.strace`.
  The initramfs SHRINKS by ~640 KiB versus embedded: busybox is
  dropped from `baseContents` (the rescue path now switch_roots
  into the squashfs, which carries its own /bin/sh) and that saving
  outweighs the ~54 KiB pulled in for the `loop`+`squashfs` kernel
  modules. The squashfs itself adds ~684 KiB of staged content to
  the boot partition.
- **external-rescue + network** (`test-external-rescue-network`)
  flips `rescue.network = true` on top of external-rescue. The
  initramfs grows by ~617 KiB versus external-rescue: ~448 KiB
  comes from compiling the `network-rescue` Cargo feature into
  `nmbl-init` (the binary jumps from 1.29 MiB to 1.73 MiB — pulls
  in `smoltcp` + the embedded HTTP client), and the rest comes from
  the auto-pulled `virtio_net` driver and DHCP wiring closure.
- The `nmbl-init` binary is byte-identical across the first three
  rows because Nix's store-path dedup keeps the feature-free build
  shared whenever `rescue.network = false` (see `lib/config.nix`'s
  `resolvedNmblInit` identity-equal short-circuit).

### Plan's "external mode at least 500 KiB smaller" claim

**PASS.** With busybox gated on `rescue.mode == "embedded"` in
`lib/config.nix`, the external-rescue initramfs is **655,881 bytes
(~640 KiB) smaller** than the embedded baseline, exceeding the
500 KiB goal. The fix scopes the `/bin/sh` symlink under a
`lib.optional (cfg.rescue.mode == "embedded")` so the
`external` and `none` modes ship no in-initramfs shell at all —
the rescue dispatcher either pivots into `nmbl-rescue.sfs` (which
carries its own `/bin/sh`) or halts via `halt_with_banner` without
ever invoking a shell. Activation tooling (LUKS / LVM / mdadm /
zfs) remains conditional on `boot.nmbl.activation.*`, which the
test configs do not trigger, so those binaries do not move the
needle here.

The external-config row still saves ~no bytes (the only delta is
the bootstrap-vs-full TOML swap), because external-config keeps
`rescue.mode = "embedded"` — the two options are orthogonal. The
500 KiB goal is met specifically by `rescue.mode = "external"`.
