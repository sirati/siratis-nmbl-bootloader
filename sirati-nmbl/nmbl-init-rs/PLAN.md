# nmbl-init design

This document describes the design of `nmbl-init`, the statically linked Rust
binary that runs as PID 1 in the NMBL initramfs. It started as the port plan
for the old bash initramfs (`scripts/*.sh.nix` with busybox, kexec-tools and
kmod). That port is complete and the bash scripts are gone. The sections below
describe the crate as it is now. Section numbers are stable because code
comments and other documents link to them.

Topic documents in `../docs/` cover individual features in more depth:
`rescue-stages.md` (staged full-system rescue), `network-stage.md`,
`erofs-generations.md`, `external-config-signing.md`, `boot-set-updates.md`,
`nmblctl.md` and `nmbl-simbox.md`.

---

## 1. Goals and non-goals

### Goals

1. No panicking constructs in shipping code. The clippy lints in
   `Cargo.toml` deny `unwrap`, `expect`, `panic!`, indexing and slicing,
   `todo!`, `unimplemented!`, `unreachable!` and `dbg!` (see §6.2).
2. Every fallible operation returns `Result<_, NmblError>`. Errors name the
   path, device, module or stage that failed, so the operator on the
   emergency screen can act on them.
3. The binary does its own work through syscalls: mounts, module loading,
   loop devices, kexec, DHCP and HTTP for network rescue. External programs
   run only where a separate tool does the job: `/bin/blkid` for the
   `/dev/disk/by-*` sweep, storage activation tools (cryptsetup, lvm, mdadm,
   zfs), and the emergency shell (`paths.shell`, busybox by default).
4. Static musl build for `x86_64-unknown-linux-musl` with `opt-level = "z"`,
   fat LTO, one codegen unit, stripped symbols and `overflow-checks = true`.
5. Configuration without recompilation. The Nix module renders a TOML file
   that the binary reads at runtime. Changing most `boot.nmbl.*` options
   regenerates that file and leaves the binary untouched. Cargo features
   (§4.2) are the exception: they select optional subsystems at build time.
6. A `ratatui` boot menu that works on a VT, a serial console and the
   optional graphical splash.

### Non-goals

- Installing the bootstrapper (GRUB, systemd-boot, EFI stub). That runs on
  the installed system through `lib/install-bootloader.nix` and related Nix
  scripts.
- Filesystem drivers. NMBL calls `mount(2)` and the kernel module provides
  the filesystem.
- `LABEL=`, `UUID=` and `PARTUUID=` device specifiers in `[[filesystems]]`.
  `Config::validate` rejects them. Use the `/dev/disk/by-*` path; NMBL
  creates those links at boot from `blkid -o export` output.

---

## 2. Architectural overview

```
┌────────────────────────────────────────────────────────────────────┐
│                        NMBL initramfs                              │
│                                                                    │
│   /init                     nmbl-init (static musl, PID 1)         │
│   /etc/nmbl/config.toml     embedded config (configLocation        │
│                             = "embedded")                          │
│   /etc/nmbl/bootstrap.toml  bootstrap config (configLocation       │
│                             = "external"; full config on /boot)    │
│   /bin/blkid                util-linux blkid for /dev/disk/by-*    │
│   /bin/sh                   busybox, emergency menu shell only     │
│                             (rescue.mode embedded or external)     │
│   /lib/modules/<ver>/…      kernel module closure                  │
│   /etc/splash/*             font and optional background           │
└────────────────────────────────────────────────────────────────────┘
                              │
                              │  kexec_file_load(2), then
                              │  reboot(LINUX_REBOOT_CMD_KEXEC)
                              ▼
                     Selected NixOS generation
```

`nmbl-init` runs a fixed sequence of phases (§7). Phase 1 runs synchronously.
Everything after it runs inside one tokio current-thread `LocalRuntime`
(`ui::block_on_tui_with_poller`), so subprocess reaps and console input do not
block the single thread.

Inner layers return a `TerminalAction`
(`src/terminal.rs`): `Reboot`, `HaltWithBanner`, `Execve`, `Kexec` or
`RebootIntoRescue`. `main` passes it to `execute_terminal_action`
(`src/main_parts/dispatch.rs`) after the stack has unwound, so every `Drop`
(termios restore, `KD_TEXT`, framebuffer release) has run before the
`execve(2)` or `reboot(2)`.

A boot failure goes through `rescue::automatic`. With `[rescue].automatic`
set, NMBL enters the configured rescue. Otherwise it opens the emergency menu
(§6.3, §9).

---

## 3. Source tree

`nmbl-init-rs/` is a Cargo workspace. The root package is `nmbl-init`, with a
library target `nmbl_init` (`src/lib.rs`) and three binaries.

```
nmbl-init-rs/
├── Cargo.toml              workspace root and nmbl-init package
├── Cargo.lock
├── flake.nix               crane and fenix builds, checks, dev shell
├── rust-toolchain.toml     stable, with clippy, rustfmt, rust-src
├── .cargo/config.toml      default musl target, crt-static
├── PLAN.md                 this document
├── src/
│   ├── main.rs             entry point, early init, failure routing
│   ├── main_parts/         args, early-exit modes, boot runtime,
│   │                       phases, selection and terminal dispatch
│   ├── bin/                nmbl-generation-mount, nmbl-generation-state
│   ├── lib.rs              module list of the nmbl_init library
│   └── <modules>           see the table below
├── nmbl-host-tools/        nmbl-sign
├── nmbl-boot-update/       nmbl-boot-update
├── nmblctl/                nmblctl
├── nmbl-ui-preview/        nmbl-ui-preview
└── nmbl-simbox/            nmbl-simbox
```

### 3.1 Library modules

| Module | Purpose | Gate |
|--------|---------|------|
| `activation` | Storage activations (LVM, mdraid, LUKS via TPM, keyfile or passphrase, ZFS) | always |
| `boot` | Phase 6: signature check, TPM measure, kexec load, teardown | always |
| `boot_selection` | `boot-default` and `boot-once` files written by `nmblctl` | always |
| `config` | TOML schema and loader (§5) | always |
| `devices` | Device wait and system filesystem mounts (phase 3b) | always |
| `error` | `NmblError` (§6.1) | always |
| `generations` | Phase 4 generation scan, `gen_id`, readiness diagnosis | always |
| `generation_mount`, `generation_state`, `generation_store` | Signed EROFS generation images | `secure-boot` |
| `handover` | Decoders for the cmdline, initrd cpio fragment and log NMBL hands to the next kernel | always |
| `imageload` | Signed out-of-tree driver images | always (load body needs `secure-boot`) |
| `ipc` | Remote TUI socket client and server | `remote-tui` |
| `log` | Log ring, kmsg reader, verbosity | always |
| `mocking` | `--debug-tui` scenario runner | `mocking` |
| `modules` | Phase 2a and 2b module loading | always |
| `mount` | Phase 1 pseudo-filesystems | always |
| `net` | Interface bring-up, DHCPv4, HTTP/1.0 client | `network-rescue` |
| `panic` | Panic hook with re-exec into recovery | always |
| `policy` | Seal on rescue (cap the lock PCR, close TPM-unsealed mappers), priority gate, rescue sentinel, refuse screen | always |
| `rescue` | Rescue dispatch, external image, chrooted rescue child, network rescue | always (`rescue::net` needs `network-rescue`) |
| `security_consts` | Security defaults mirrored by `lib/security-consts.nix` | always |
| `shell` | Emergency entry point and menu actions | always |
| `sig` | Sidecar wire format (always) and ML-DSA verification | `secure-boot` for verification |
| `splash` | DRM framebuffer splash and compositor | `image-splash` |
| `staged` | Signed staged-boot fragment merge | `staged-boot` |
| `state` | `state.bin` persistent boot state | `stateful` |
| `sys` | Syscall wrappers: mount, module, kexec, loop devices, blkid, btrfs scan, cpio, pty, tty, vt, poller | always |
| `terminal` | `TerminalAction` | always |
| `tpm` | TPM 2.0 transport, PCR cap, presence; `tpm::measure` | always (`measure` needs `secure-boot`) |
| `ui` | Console backends, boot menu, emergency screen, modals, Pretty Shell, console relay, remote TUI | always (parts gated) |
| `util` | Hex and hash helpers | always (`hash` needs `network-rescue` or `secure-boot`) |
| `validate` | `--validate-hardware` and `--validate-nix-filesystem-closure` | always |

### 3.2 Binaries of the root package

| Binary | Source | Use |
|--------|--------|-----|
| `nmbl-init` | `src/main.rs` | PID 1 in the NMBL initramfs. Also the remote TUI client when run as a non-PID-1 process with `remote-tui`. |
| `nmbl-generation-mount` | `src/bin/nmbl-generation-mount.rs` | systemd-initrd helper that mounts a signed generation image in the booted system's stage 1 (`lib/modules/security/generation-image.nix`). Requires `secure-boot`. |
| `nmbl-generation-state` | `src/bin/nmbl-generation-state.rs` | Generation state helper for signed EROFS generations. Requires `secure-boot`. |

### 3.3 Sibling crates

| Crate | Binary | Target | Purpose |
|-------|--------|--------|---------|
| `nmbl-host-tools` | `nmbl-sign` | host | ML-DSA signer. Depends on `nmbl_init::sig` for the sidecar format. The dependency points one way: `nmbl-init` does not depend on it. |
| `nmbl-boot-update` | `nmbl-boot-update` | host | Prepares and requests whole boot-set updates (`../docs/boot-set-updates.md`). |
| `nmblctl` | `nmblctl` | host | Root-only control and inspection tool on the installed system (`../docs/nmblctl.md`). |
| `nmbl-ui-preview` | `nmbl-ui-preview` | host | Renders NMBL's boot UI in an X11 window with mock state. Development only. |
| `nmbl-simbox` | `nmbl-simbox` | musl | Runs the unmodified `nmbl-init` as PID 1 in a rootless podman container with simulated kernel syscalls (`../docs/nmbl-simbox.md`). Development only. |

---

## 4. Dependencies and features

### 4.1 Dependencies

| Crate | Use |
|-------|-----|
| `nix` 0.29 | Syscall wrappers (mount, fs, process, term, user, signal, reboot). `nix/net` only with `network-rescue`. |
| `rustix` 0.38 | fs, termios, event, net, process wrappers. |
| `libc` | Raw syscalls without a safe wrapper: `init_module`, `kexec_file_load`. |
| `tokio` 1 | Current-thread runtime with `rt`, `macros`, `time`, `net`, `io-util`. |
| `ratatui` 0.30 | TUI rendering, with the `termwiz` and `crossterm` backends. |
| `crossterm`, `termwiz`, `terminfo` | Terminal input and output for the tty console backend. |
| `serde`, `toml`, `serde_json` | Config parsing and structured payloads. |
| `thiserror` | `NmblError` derive. |
| `zeroize` | Wiping passphrases and key material. |
| `tpm2-protocol` | Pure-Rust TPM 2.0 marshaling. Always linked: the PCR cap in `policy` needs it in every build. |
| `lzma-rs`, `ruzstd`, `flate2` (`rust_backend`) | Userspace decompression of `.ko.xz`, `.ko.zst` and `.ko.gz` before `init_module(2)`. NixOS kernels lack `CONFIG_MODULE_DECOMPRESS`. |

Optional dependencies follow the features in §4.2: `alacritty_terminal`,
`drm`, `png`, `fontdue`, `oklab`, `dhcproto`, `sha2`, `getrandom`, `fips204`,
`ciborium`, `nonmax` and `futures-util`.

Every dependency is pure Rust. No `*-sys` crate and no C library is linked.

### 4.2 Cargo features

| Feature | Enables | Turned on by (`lib/signing-build.nix`) |
|---------|---------|------------------------------------------|
| `pretty-shell` (default) | `alacritty_terminal`; the Pretty Shell entry on the emergency menu | default build |
| `image-splash` | `drm`, `png`, `fontdue`, `oklab`, `pretty-shell`; the DRM splash console | `boot.nmbl.splash.enable` |
| `network-rescue` | `dhcproto`, `sha2`, `getrandom`, `nix/net`; `src/net` and `rescue::net` | `boot.nmbl.rescue.network` |
| `remote-tui` | `futures-util`; `src/ipc` and the remote TUI | `boot.nmbl.rescue.fullSystem.enable` |
| `rescue-stages` | `sha2`; SHA-512 pin checks for the stage-2 rescue image and the network stage | `rescue.mode = "external"` with `rescue.fullSystem.enable` |
| `stateful` | `ciborium`, `nonmax`; `src/state` and `state.bin` | `boot.nmbl.stateful.enable` |
| `secure-boot` | `fips204`, `sha2`; signature verification, TPM measure, signed generation images | any enabled security table (`mkSecureBootActive` in `lib/security-consts.nix`) |
| `staged-boot` | `secure-boot`; `src/staged` | `boot.nmbl.staged.enable` |
| `mocking` | `--debug-tui` entry point | test harnesses only |

A config that carries a rescue pin requires a binary built with
`rescue-stages` or `secure-boot`. A binary without either refuses a pinned
image.

`lib/signing-build.nix` picks the binary. With no features it uses the prebuilt
`nmbl-init` package. With only `image-splash` it uses `nmbl-init-splash`. All
other combinations, and every build with baked public keys, go through
`mkNmblInit { features, publicKeys, requireKeys }` from the crate flake.

---

## 5. Configuration

### 5.1 Where the config comes from

- `boot.nmbl.configLocation = "embedded"` (default): `lib/config-toml.nix`
  renders the full config and the initramfs carries it at
  `/etc/nmbl/config.toml`. `--config=<path>` overrides the path.
- `boot.nmbl.configLocation = "external"`: the initramfs carries only
  `/etc/nmbl/bootstrap.toml` (rendered by `lib/bootstrap-toml.nix`). Phase
  0.5 mounts the boot filesystem and reads the full config from the
  `config_path` it names. With signing enabled the bootstrap config also
  names a detached signature (`../docs/external-config-signing.md`). On
  whole boot-set updates the GRUB dispatcher passes
  `nmbl.config=/nmbl-boot-sets/A/config` or `.../B/config` on the cmdline,
  and NMBL accepts only those two values.

### 5.2 Schema

`Config` (`src/config/mod.rs`) and every table use
`#[serde(deny_unknown_fields)]`, so an unknown key fails the load. Every
top-level table has a default, so a minimal config is valid.

| Table | Rust type (file) | Gate |
|-------|------------------|------|
| `[general]` | `General` (`general.rs`) | always |
| `[kernel_modules]` | `KernelModules` (`general.rs`) | always |
| `[[filesystems]]` | `FilesystemEntry` (`entries.rs`) | always |
| `[[activations]]` | `Activation` (`entries.rs`) | always |
| `[tui]` | `Tui` (`tui.rs`) | always |
| `[paths]` | `Paths` (`paths.rs`) | always |
| `[splash]` | `Splash` (`splash.rs`) | `image-splash` |
| `[rescue]` with `[rescue.image]`, `[rescue.system]`, `[rescue.network_stage]`, `[rescue.tools]`, `[rescue.identity_volume]` | `RescueConfig` (`rescue_cfg.rs`) | always |
| `[emergency_shell]` | `EmergencyShellConfig` (`rescue_cfg.rs`) | always |
| `[stateful]` | `StatefulConfig` (`stateful_cfg.rs`) | `stateful` |
| `[driver_images]` | `DriverImagesConfig` (`driver_image.rs`) | always |
| `[tpm]` | `TpmConfig` (`tpm.rs`) | always |
| `[signing]` | `SigningConfig` (`signing.rs`) | `secure-boot` |
| `[generation_image]` | `GenerationImageConfig` (`generation_image.rs`) | `secure-boot` |
| `[secure_boot]` | `SecureBootConfig` (`secure_boot.rs`) | `secure-boot` |
| `[staged]` | `StagedConfig` (`staged.rs`) | `staged-boot` |

The Nix side emits a feature-gated table only when it also enables the
feature, so the binary always parses what Nix renders.

An abridged example of the always-present tables, with the defaults the Rust
loader applies:

```toml
[general]
verbosity              = "info"      # quiet | info | verbose
timeout_ms             = 5000        # selector countdown
device_timeout_secs    = 30
panic_report_dir       = "/run"
instant_boot           = false
# emergency_timeout_secs = 30        # unset: 30 s auto-reboot on the
                                     # emergency screen when unattended

[kernel_modules]
early       = ["virtio_gpu"]         # phase 2a, before the console opens
explicit    = ["virtio_blk", "ext4"] # phase 2b, after the console opens
blacklist   = ["nouveau"]
modules_dir = "/lib/modules"

[[filesystems]]
device     = "/dev/disk/by-partlabel/root"
mountpoint = "/"
fstype     = "ext4"
options    = "ro"
is_root    = true

[paths]
system_root      = "/mnt/system"
nix_profiles_dir = "/mnt/system/nix/var/nix/profiles"
shell            = "/bin/sh"

[tui]
enable_editor      = true
show_kernel_params = true

[rescue]
mode           = "embedded"          # embedded | external | none
automatic      = false
force_on_boot  = false
entrypoint     = "/bin/sh"           # "/init" for the full-system rescue
network        = false
default_url    = ""
default_sha256 = ""

[emergency_shell]
extra_consoles = []
```

`[general].serial_console` still parses and has no effect. Older configs that
set it keep loading.

The full-system rescue adds `[rescue.image]` (`format`, `sha512`),
`[rescue.system]` (`sshd_port`, `authorized_keys`, `host_key_path`, `modules`,
`network_profile`), `[rescue.tools]` (`path`, `sha512`) for the `nmblctl`
image, and optionally `[rescue.network_stage]` (`path`, `sha512`).
`../docs/rescue-stages.md` describes them.

### 5.3 Validation

`Config::load` parses the file and runs `Config::validate`, which rejects
`LABEL=`, `UUID=` and `PARTUUID=` devices and checks that every filesystem on
an activated device has a matching activation. `nmbl-init
--validate-config=<path>` runs the same load at build time, and
`--validate-config-fragment` checks a staged fragment.

### 5.4 Kernel command line

NMBL reads two tokens from `/proc/cmdline`:

- `nmbl.key_echo=1` opens a key-echo diagnostic screen in place of the boot
  menu, then the emergency screen.
- `nmbl.config=/nmbl-boot-sets/{A,B}/config` selects the boot-set config
  (§5.1).

---

## 6. Error handling

### 6.1 The `NmblError` enum

`src/error/mod.rs` defines `NmblError` with `thiserror`. The current variants
are `Config`, `Io`, `ConfigInvalid`, `Mount`, `Umount`, `Module`, `KexecLoad`,
`KexecReturned`, `DeviceTimeout`, `NoGenerations`, `SystemRootNotMounted`,
`ProfilesDirMissing`, `Tui`, `Activation`, `Bootstrap`, `Rescue`,
`DriverImage`, `Panicked`, `Shell`, `OperatorAborted`, `OperatorChoseReboot`,
`WrongPasswordShellExited`, `StateTooLarge`, `StateRoundtripMismatch`,
`Signature`, `TpmProto` and `PolicyRefused`. Wrapper variants such as
`Bootstrap { stage, source }` and `Rescue { stage, source }` name the stage
and keep the inner error as `source`. `error::format_chain` renders the whole
chain.

### 6.2 Discipline

- Lints. `Cargo.toml` sets `[lints.clippy]` to deny `unwrap_used`,
  `expect_used`, `panic`, `indexing_slicing`, `todo`, `unimplemented`,
  `unreachable` and `dbg_macro`, and allows `print_stderr` and
  `print_stdout`. There is no `clippy.toml` and no crate-level lint
  attribute. Test modules opt out with a scoped `#[allow(..., reason = ...)]`.
  The `nmbl-init-clippy` check runs `cargo clippy -p nmbl-init --all-targets
  -- --deny warnings`, so every warning fails CI. The sibling crates carry
  their own `[lints.clippy]` tables, and `nmbl-boot-update` also forbids
  `unsafe_code`.
- Context. Errors carry the path, device, module or stage that failed.
- Overflow. The release profile sets `overflow-checks = true`.
- Panics. The release profile keeps unwinding. `main` installs the panic hook
  (`src/panic.rs`) before parsing arguments. The hook writes a report under
  `general.panic_report_dir` and re-executes `/proc/self/exe
  --errored=<report>`, which keeps the PID. `main` sees `--errored`, loads the
  config leniently and routes `NmblError::Panicked` through the normal failure
  handling. If the hook itself fails, it calls `_exit(1)`.

### 6.3 Emergency entry and exec sites

`shell::drop_to_emergency` is the emergency entry point. It shows the
emergency screen (§9) on the console that is already open. Before any
interactive shell, `policy::seal_secrets` caps the lock PCR and closes every
TPM-unsealed LUKS mapper. The shell spawn helpers take the resulting `Sealed`
value as an argument, so a shell cannot start without it.

The Raw Shell forks one `paths.shell` onto a PTY and relays it to the consoles
the operator picks (`/dev/console` and `emergency_shell.extra_consoles`). NMBL
stays PID 1 and returns to the menu when the shell exits. The Pretty Shell
runs the same shell in an `alacritty_terminal` emulator inside the TUI frame.

Two CI checks in the crate flake guard process creation:

- `nmbl-init-no-exec` requires a `// execve safety: <why>` comment on or
  directly above every `execve(` and `Command::` in `src/`.
- `nmbl-init-must-seal` requires a `Sealed` witness, a `seal_secrets` call or
  a `// seal-exempt: <why>` comment in the same function as every shell spawn.

---

## 7. Phase-by-phase mapping

The order below is the order in `src/main.rs`,
`src/main_parts/boot_runtime.rs`, `src/main_parts/phases/` and
`src/main_parts/dispatch.rs`.

| Step | Where | What happens |
|------|-------|--------------|
| Early init | `main::early_init` | As PID 1: mount `/dev`, `/proc`, `/sys`; wire fds 0 to 2 to `/dev/console`; set `/` to mode 0755; arm the early key tap. Install the panic hook. |
| Arguments | `main_parts/args.rs`, `early_exit.rs` | Parse flags. Validation and installer modes exit here (§11). `--errored` enters panic recovery. |
| Config | `main` | Bootstrap mode if `/etc/nmbl/bootstrap.toml` exists. Otherwise load `--config` (default `/etc/nmbl/config.toml`). A load error is kept and reported after phase 1. |
| Phase 1 | `mount.rs` | Mount `/proc`, `/sys`, `/dev`, `/run`, `/tmp`. Runs before the runtime starts. |
| Phase 0.5 | `phases::run_bootstrap_phase` | Bootstrap mode only: load its modules, sweep blkid, mount the boot filesystem, load and verify the full config. |
| Generation state | `boot_runtime.rs` | `secure-boot` with `[generation_image]` state tracking: mount the state store and decide proceed, roll back or fail. |
| Forced rescue | `policy::should_force_rescue` | `rescue.force_on_boot` with `mode = "external"`, or the rescue sentinel, ends the runtime and enters rescue. |
| State mount | `phases::mount_state_twin` | `stateful` in bootstrap mode: bind a writable view of the boot filesystem for `state.bin`. |
| Phase 2a | `modules.rs` | Load `kernel_modules.early`. |
| Driver images | `imageload` | Load signed driver images when `driver_images.enable`. A failure refuses the boot. |
| Console | `ui::console::open_console` | Splash console with `image-splash` and `splash.enable`, tty console otherwise. `nmbl.key_echo=1` branches here. |
| Gate 1 | `post_console.rs` | `secure-boot`: priority gate on the plain boot filesystem. |
| Phase 2b | `modules.rs` | Load `kernel_modules.explicit`. |
| Phase 2c | `sys::blkid` | Create `/dev/disk/by-*` links. A failure is logged and boot continues. |
| Phase 3 | `activation` | Storage activations, with the passphrase modal for `luks-password`. |
| Gate 2 | `post_console.rs` | `secure-boot`: priority gate after unlock, then staged boot with `staged-boot`. |
| Phase 3b | `devices::mount_system_filesystems` | blkid sweep, btrfs device scan, device wait, mount `[[filesystems]]` under `paths.system_root`. |
| Sentinel | `dispatch::select_and_act` | Re-check the rescue sentinel now that `/boot` is mounted. |
| Phase 4 | `generations` | Scan generations. |
| Phase 5 | `ui`, `dispatch.rs` | Instant boot, skip-selector, stateful decision or the boot menu. |
| Phase 6 | `boot::kexec_into` | Verify, measure, load the image, sync, unmount. Returns `TerminalAction::Kexec`. |
| Terminal action | `dispatch::execute_terminal_action` | Flush the log and fire the syscall. |

### Phase 1: pseudo-filesystems (`src/mount.rs`)

| Target | Type | Options |
|--------|------|---------|
| `/proc` | `proc` | `nosuid,noexec,nodev` |
| `/sys` | `sysfs` | `nosuid,noexec,nodev` |
| `/dev` | `devtmpfs` | `mode=755,nosuid` |
| `/run` | `tmpfs` | `nosuid,nodev,mode=755` |
| `/tmp` | `tmpfs` | `nosuid,nodev` |

`early_init` mounts the first three before the config loads, so a panic in
argument parsing or config loading can still re-exec through `/proc`. Phase 1
treats `EBUSY` as already mounted. `sys::pty` mounts `devpts` on demand.

### Phase 0.5: bootstrap (`src/main_parts/phases/mod.rs`)

Runs only in bootstrap mode. It reads `/etc/nmbl/bootstrap.toml`, loads
`[bootstrap.kernel_modules]`, runs the blkid sweep, mounts `[bootstrap.boot_fs]`
at its `mountpoint`, verifies the full config signature when one is named, and
loads the full `Config`. It records the boot mountpoint in
`Config::runtime_boot_mountpoint` for the rescue and state paths. Failures
return `NmblError::Bootstrap { stage, source }`. The boot filesystem stays
mounted on failure so the emergency shell sees it.

### Phases 2a and 2b: kernel modules (`src/modules.rs`, `src/sys/module/`)

`sys::module` parses `<modules_dir>/<release>/modules.dep`, resolves
dependencies, skips blacklisted names, decompresses `.ko.xz`, `.ko.zst` and
`.ko.gz` in userspace and calls `init_module(2)`. `EEXIST` counts as loaded.
Phase 2a loads `early` before the console opens, so the splash backend finds
a DRM device. Phase 2b loads `explicit` with progress shown on the console.

### Phase 3: storage activations (`src/activation/`)

Each `[[activations]]` entry has a `kind` (`lvm`, `mdraid`, `luks-tpm`,
`luks-keyfile`, `luks-password` or `zfs`), the modules it needs, the binary and
arguments to run, and the devices it produces. The runner reaps children
through the poller. `luks-password` prompts through the TUI passphrase modal.
Unlocked key material that the next stage needs is returned as
`KeyInjection`s for phase 6.

### Phase 3b: system filesystems (`src/devices/`)

`mount_system_filesystems` repeats the blkid sweep, issues
`BTRFS_IOC_SCAN_DEV` for btrfs members, waits up to
`general.device_timeout_secs` per device and mounts each `[[filesystems]]`
entry in order. The entry with `is_root = true` mounts at `paths.system_root`.
Other mountpoints mount below it.

### Phase 4: generation discovery (`src/generations/`)

`scan_generations` reads `paths.nix_profiles_dir`, keeps `system-<N>-link`
entries, resolves each link under `system_root`, and reads `kernel`, `initrd`
and `kernel-params`. Generations sort by number, newest first. An empty result
is `NmblError::NoGenerations`, and `generations::readiness` explains the likely
cause on the emergency screen. Signed EROFS generations
(`../docs/erofs-generations.md`) use the same scan after the image is mounted.

### Phase 5: selection (`src/ui/`, `src/main_parts/dispatch.rs`)

In order of precedence:

1. With `stateful` on profile hosts, a one-use retry request written by
   `nmblctl`, or the single retry a ready rescue grants, picks the
   generation.
2. Instant boot (`general.instant_boot`) boots the default immediately when
   every health check passes and no key was pressed during early boot.
3. If the operator unlocked LUKS with "Select NixOS Generation" unchecked, the
   default boots without the menu.
4. Otherwise the boot menu runs a countdown of `general.timeout_ms`. With
   `stateful`, `state::decide` may honour the countdown, pick a known-good
   generation or report exhaustion.

The default index comes from `boot_selection` (`boot-default` and `boot-once`,
written by `nmblctl`). A one-shot selection is consumed once a generation is
chosen. In the menu the operator can pick a generation, edit its kernel
command line (`tui.enable_editor`), open the emergency shell or reboot.

The UI renders through the `Console` trait (`src/ui/console/`). The tty
backend opens `/dev/console` in raw mode and draws through termwiz and
terminfo. The splash backend (`image-splash`) draws on the DRM framebuffer
and falls back to the tty on any bring-up failure. Panic recovery always uses
the tty.

### Phase 6: kexec (`src/boot/`, `src/sys/kexec.rs`)

`boot::kexec_into`:

1. With `secure-boot`, verifies the generation signature over pinned file
   descriptors.
2. With `tpm.measure` or secure boot, extends the lock PCR with the kernel,
   initrd, cmdline and driver-image digests.
3. Builds an initrd cpio fragment in a memfd with the key injections and the
   NMBL log, appends it to the generation initrd, and calls
   `kexec_file_load(2)`.
4. Calls `sync(2)`, waits a short settle time, and lazily unmounts the
   filesystems in reverse order, then `system_root`, then the
   pseudo-filesystems.
5. Returns `TerminalAction::Kexec`. `execute_terminal_action` then calls
   `reboot(LINUX_REBOOT_CMD_KEXEC)`.

The booted system's stage 1 unpacks the fragment. `nmblctl status` decodes it
through `handover`.

---

## 8. Nix integration

The parent flake imports this directory as the `nmbl-init-rs` input. The crate
flake exports `packages.default` (`nmbl-init`), `nmbl-init-splash`,
`nmbl-sign`, `nmbl-boot-update`, `nmblctl`, `nmbl-ui-preview` and
`nmbl-simbox`, plus `legacyPackages.mkNmblInit` and
`legacyPackages.mkNmblCtl`.

`lib/config.nix` builds `system.build.nmblInitramfs` with `pkgs.makeInitrd`
and `gzip -9`. The contents are:

| Path | Source | Condition |
|------|--------|-----------|
| `/init` | `selectedNmblInit` (`lib/signing-build.nix`) | always |
| `/etc/nmbl/config.toml` | `lib/config-toml.nix` | `configLocation = "embedded"` |
| `/etc/nmbl/bootstrap.toml` | `lib/bootstrap-toml.nix` | `configLocation = "external"` |
| `/bin/sh` | busybox | `rescue.mode` is `embedded` or `external` |
| `/bin/blkid` | util-linux | always |
| `/lib/modules` | module closure | always |
| `/etc/modprobe.d/nixos.conf` | blacklist | always |
| `/etc/splash/font.ttf`, `/etc/splash/image.png` | splash options | `splash.enable`; the image only with `backgroundLocation = "initrd"` |
| activation tools | `cfg.activation.extraContents` | when filesystems need them |

Rescue images, network stages, signed generation images and the external
config are staged on the boot partition by the install scripts in `lib/`.

---

## 9. Boot-time error UX

A failed phase opens the emergency screen (`src/ui/emergency/`) on the console
that is already open, splash or tty. The screen shows the error chain and a
cause-specific hint, then offers:

| Entry | Action |
|-------|--------|
| Reboot | `TerminalAction::Reboot`. This is the default. |
| Pretty Shell | Shell in a terminal emulator inside the TUI (`pretty-shell`). |
| Raw Shell | Console picker, then a PTY shell relayed to the chosen consoles. |
| Retry boot from config | Re-run phases 3, 3b, 4 and the selector. |
| Verify kexec readiness | Skip phases 3 and 3b, scan generations, confirm, kexec. |

If no key has been pressed during this boot, the screen reboots after 30
seconds (`general.emergency_timeout_secs` overrides this). Once the operator
has pressed a key, the screen waits for a choice.

With `[rescue].automatic = true`, a failure that has nothing left to fall back
to enters the configured rescue (`rescue::automatic`). A rescue that fails
returns to the emergency screen. Signature and policy failures under secure
boot show the refuse screen and end in `TerminalAction::RebootIntoRescue`,
which needs a `Sealed` value to construct.

### Rescue modes (`src/rescue/`)

| `rescue.mode` | Behaviour |
|---------------|-----------|
| `embedded` (default) | The emergency menu shells use busybox at `/bin/sh` in the initramfs. |
| `external` | NMBL mounts the rescue image from the boot partition (default `nmbl-rescue.sfs`, or `rescue.sfs_path`) over one pinned loop device and runs `rescue.entrypoint` as a chrooted child. NMBL stays PID 1. |
| `none` | No rescue tools. Rescue halts with a banner. |

The flat external rescue is a busybox squashfs built from
`rescue.squashfsContents`. The staged full-system rescue uses an EROFS stage-2
image pinned by SHA-512 in `[rescue.image]`, with host data from
`[rescue.system]` written to `/etc/nmbl-rescue/` in the rescue overlay.
`../docs/rescue-stages.md` is the reference for it. Inside the rescue, `nmbl`
attaches to NMBL's remote TUI over the root-only socket at
`/nmbl-root/nmbl-run/tui.sock`.

With `network-rescue` and `rescue.network = true`, a failed disk rescue falls
back to the network. `rescue::net` brings up the first Ethernet link with a
carrier, runs DHCPv4, asks for a URL (prefilled from `rescue.default_url`),
streams the image over HTTP/1.0 into a memfd while hashing it with SHA-256,
asks the operator to confirm the hash against `rescue.default_sha256`, then
mounts the memfd and runs the rescue child. Network rescue supports HTTP only.

---

## 10. Testing strategy

| Layer | What exists |
|-------|-------------|
| Unit | `cargo test` in the dev shell (`cargo-nextest` is also available). Tests sit next to the code in `tests.rs` files and `tests/` directories, including ratatui `TestBackend` renders. Feature-gated tests run with the feature, for example `cargo test --features rescue-stages`. |
| Crate checks | `nix flake check` in `nmbl-init-rs/`: `nmbl-init-clippy`, `nmbl-init-fmt`, `nmbl-init-no-exec`, `nmbl-init-must-seal`, `nmbl-init-no-cap-bypass`, `nmbl-init-security-consts`, `zero-keys-rejected`, `nmbl-ui-preview-absent`, and clippy and test checks for the sibling crates. |
| Container | `nmbl-simbox` runs the real binary as PID 1 with simulated syscalls (`nix run .#test-nmbl-simbox`). |
| TUI harness | The `mocking` feature adds `nmbl-init --debug-tui -- <scenario>`, which runs one modal flow in the current terminal for test runners in a tmux pane. No Nix build enables it. `nmbl-ui-preview` renders the UI in X11. |
| VM | The parent flake's `nix run .#test-*` apps boot VMs for the bootstrapper matrix, rescue, secure boot, stateful and boot-set update scenarios (`testing/`). |

`nmbl-init-no-cap-bypass` requires a `// signing safety:` or `// cap-exempt:`
comment at every place that skips verification, downgrades to audit mode or
relaxes the PCR cap. `nmbl-init-security-consts` compares
`src/security_consts.rs` with `lib/security-consts.nix`.

---

## 11. Entry modes and terminal actions

`main` handles these modes before a normal boot:

| Flag | Mode |
|------|------|
| `--debug-tui -- <scenario>` | `mocking` builds only. Runs one modal flow on the current terminal. |
| `--validate-config=<path>` | Parse and validate a config, then exit. |
| `--validate-config-fragment=<toml>` | `staged-boot` builds only. Validate a staged fragment, then exit. |
| `--validate-hardware=<toml>` | Read-only check of a config against the running machine, then exit. `--tool=<kind>:<path>` supplies tool paths for it. |
| `--validate-nix-filesystem-closure=<json>` with `--config-toml=<toml>` | Build-sandbox closure check, then exit. |
| `--print-gen-id=<toplevel>` | Print the content-addressed generation id that `nmbl-sign` uses for signature paths. |
| `--init-state=<dir>` | `stateful`: the installer creates or validates `state.bin`. |
| `--boot-succeeded=<dir>` | `stateful`: a systemd unit marks the last boot as succeeded in `state.bin`. |
| `--errored=<report>` | Panic recovery (§6.2). |
| `--config=<path>` | Normal boot with a different config path. |

Every path ends in one `TerminalAction`:

| Variant | Syscall |
|---------|---------|
| `Reboot` | `reboot(RB_AUTOBOOT)` |
| `HaltWithBanner` | prints the banner, `reboot(RB_HALT_SYSTEM)` |
| `Execve` | `execve(2)` of the given path, used by the embedded rescue |
| `Kexec` | `reboot(LINUX_REBOOT_CMD_KEXEC)` after `boot::kexec_into` loaded the image |
| `RebootIntoRescue` | prints the banner, `reboot(RB_AUTOBOOT)`. The refuse path wrote the rescue sentinel and relocked LUKS before it built this value. |

`execute_terminal_action` flushes the log ring to `log::NMBL_LOG_PATH` before
the syscall.

---

## 12. Optional subsystems and open work

Each subsystem below has a `boot.nmbl` option and is off by default unless
noted.

| Subsystem | Option | Reference |
|-----------|--------|-----------|
| External config on `/boot` | `boot.nmbl.configLocation = "external"` | §5.1, `../docs/external-config-signing.md` |
| Rescue modes | `boot.nmbl.rescue.mode` | §9 |
| Automatic rescue | `boot.nmbl.rescue.automatic` | §9 |
| Staged full-system rescue | `boot.nmbl.rescue.fullSystem.enable` | `../docs/rescue-stages.md` |
| Rescue network stage | `boot.nmbl.rescue.fullSystem.networkStage.enable` | `../docs/network-stage.md` |
| Network rescue | `boot.nmbl.rescue.network` | §9 |
| Device wait timeout | `boot.nmbl.deviceTimeoutSeconds` | §7 phase 3b |
| Instant boot | `boot.nmbl.instantBoot.enable` | §7 phase 5, `../docs/nmblctl.md` |
| Stateful boot tracking and rollback | `boot.nmbl.stateful.enable` | §7 phase 5 |
| Graphical splash | `boot.nmbl.splash.enable` | §7 phase 5 |
| Signing, TPM measure, driver images, staged boot, signed generations | `boot.nmbl.signing`, `boot.nmbl.tpm`, `boot.nmbl.driverImages`, `boot.nmbl.staged`, `boot.nmbl.generationImage` | `../docs/erofs-generations.md`, `testing/secure-boot-matrix.md` |
| Whole boot-set updates | `boot.nmbl.bootUpdate.enable` | `../docs/boot-set-updates.md` |

The pre-kexec NMBL log reaches the booted system in the initrd fragment
(§7 phase 6).

Not implemented:

- `LABEL=`, `UUID=` and `PARTUUID=` device specifiers (§1).
- HTTPS, IPv6, Wi-Fi and PXE for network rescue.

---

## 13. Size deltas

This section is a measurement record from 2026-05-27 (commits `bff5530` and
`7fc905d`). The numbers have not been re-measured since. Two later changes
affect them: `lib/config.nix` now stages busybox as `/bin/sh` for
`rescue.mode = "external"` too, so the menu's Raw Shell has a binary, and the
crate has grown many features. Treat the table as historical.

Measured against four runtime configurations from
`testing/build_configurations.nix`. All numbers are bytes from `du -b` of the
built store paths. The `initramfs` column is the gzip-9 initrd in `/boot`. The
`nmbl-init` column is the static-musl PID 1 ELF. The `rescue.sfs` column is
the zstd-19 squashfs staged at `/boot/nmbl-rescue.sfs` (only with
`rescue.mode = "external"`).

| Configuration                       | initramfs (bytes) | nmbl-init (bytes) | rescue.sfs (bytes) |
|-------------------------------------|------------------:|------------------:|-------------------:|
| embedded (default)                  |        37,570,137 |         1,351,920 |                n/a |
| external-config                     |        37,570,319 |         1,351,920 |                n/a |
| external-rescue                     |        36,914,256 |         1,351,920 |            700,416 |
| external-rescue with network        |        37,546,091 |         1,810,704 |            700,416 |

Notes on the measurement:

- embedded is `test-gpt-uefi-grub` with `configLocation = "embedded"` and
  `rescue.mode = "embedded"`.
- external-config (`test-external-config`) keeps the embedded rescue and moves
  the full config to the boot partition. The 182-byte difference is the
  bootstrap TOML in place of the full TOML.
- external-rescue (`test-external-rescue`) set `rescue.mode = "external"` with
  a squashfs of `busybox-sandbox-shell` and `pkgsStatic.strace`. At that time
  the initramfs omitted busybox in external mode, which made it 655,881 bytes
  smaller than embedded and met the plan's 500 KiB target. Current builds
  stage busybox in external mode, so this saving no longer applies.
- external-rescue with network (`test-external-rescue-network`) added
  `rescue.network = true`. The binary grew by about 448 KiB from the
  `network-rescue` feature (`dhcproto`, `sha2`, `getrandom` and the in-crate
  HTTP client). The rest came from the `virtio_net` driver.
- The `nmbl-init` binary was identical across the first three rows because
  those builds used the same feature-free package.
- The staged full-system rescue (EROFS stage 2) did not exist when these
  numbers were taken.
