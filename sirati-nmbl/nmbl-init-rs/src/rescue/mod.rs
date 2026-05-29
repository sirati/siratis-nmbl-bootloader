//! Rescue dispatcher (PLAN.md §6.3, Option 2 + Phase C.1).
//!
//! The emergency-shell entrypoint in `shell::drop_to_emergency` decides
//! between three rescue modes based on the operator's
//! [`RescueConfig::mode`]:
//!
//! * [`RescueMode::Embedded`] — legacy: busybox lives inside the
//!   initramfs cpio. [`exec_embedded`] performs the bare `execve(2)`
//!   into `cfg.paths.shell`, mirroring what `shell::drop_to_emergency`
//!   has always done.
//! * [`RescueMode::External`] — `nmbl-rescue.sfs` lives on the boot
//!   partition; we loop-mount it on demand, `pivot_root` into it, and
//!   exec its `/bin/sh`. See [`disk::try_disk_rescue`].
//! * [`RescueMode::None`] — no rescue toolkit shipped. Print a
//!   structured banner and halt the system via `reboot(RB_HALT_SYSTEM)`.
//!
//! The public surface is the contract that C.3 (shell.rs refactor) and
//! Phase E.1 (`net::try_network_rescue`) build against, so the shapes
//! must not change without orchestrator coordination.

pub mod disk;
#[cfg(feature = "network-rescue")]
pub mod net;

use std::ffi::CString;
use std::io;
use std::path::{Path, PathBuf};

use nix::mount::MsFlags;
use nix::unistd::{chdir, chroot};
use serde::Deserialize;

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::terminal::TerminalAction;
use crate::ui::console::Console;

/// Default basename of the rescue squashfs on the boot partition. Used
/// when `[rescue].sfs_path` is absent from the operator's runtime
/// config.
const DEFAULT_SFS_BASENAME: &str = "nmbl-rescue.sfs";

/// How [`crate::shell::drop_to_emergency`] reaches the operator. Comes
/// from the runtime [`Config`]'s `[rescue]` section; persists to TOML
/// as kebab-case strings (`"embedded"`, `"external"`, `"none"`).
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RescueMode {
    /// Legacy: busybox baked into the initramfs.
    /// [`crate::shell::drop_to_emergency`] execs `cfg.paths.shell`
    /// directly via [`exec_embedded`].
    #[default]
    Embedded,
    /// `nmbl-rescue.sfs` on the boot partition; loop-mounted on demand
    /// by [`disk::try_disk_rescue`].
    External,
    /// No rescue tools shipped; halt with a structured banner via
    /// [`halt_with_banner`].
    None,
}

/// Decide whether to drop into the embedded shell, mount the external
/// squashfs, or halt. `cause` is the error that triggered the rescue
/// (surfaced in the banner). Returns a [`TerminalAction`] the
/// dispatcher in `main` performs after every stack-allocated
/// resource has been dropped via normal unwinding.
///
/// The `External` arm tries the disk-rescue path first; on failure it
/// falls through to the network-rescue path when the `network-rescue`
/// Cargo feature is compiled in AND `config.rescue.network` is true.
/// Anything else collapses to [`halt_with_banner`] so the operator
/// sees a structured diagnostic instead of a silent reboot loop.
///
/// `console` is the live boot console the orchestrator holds, passed
/// BY OWNERSHIP so the External arm can keep it alive for the
/// network-rescue UI. Embedded / halt arms drop it on entry — they
/// never interact with the operator further. Either way, the
/// returned `TerminalAction` carries no console reference: the box
/// is dropped before the function returns, which is the whole point
/// of routing every syscall through `main`.
pub fn dispatch(
    config: &Config,
    console: Box<dyn Console>,
    cause: NmblError,
) -> Result<TerminalAction> {
    // `console` is owned by this function and drops by normal scope
    // exit before the dispatcher in `main` fires any syscall. The
    // External arm threads it down into `dispatch_external` for the
    // network-rescue UI; Embedded and None drop it at their match
    // arm's closing brace.
    match config.rescue.mode {
        RescueMode::Embedded => exec_embedded(config, cause),
        RescueMode::External => dispatch_external(config, console, cause),
        RescueMode::None => Ok(halt_with_banner(cause)),
    }
}

/// Internal helper: try disk-rescue, then network-rescue (when
/// compiled in + enabled), then halt-with-banner. Split out so the
/// `dispatch` match stays a single line per arm.
fn dispatch_external(
    config: &Config,
    console: Box<dyn Console>,
    cause: NmblError,
) -> Result<TerminalAction> {
    // Phase 1: mount the rescue squashfs. We hold onto the console
    // across this call so a mount failure can fall through to the
    // network-rescue UI without re-opening /dev/console.
    let disk_err = match disk::prepare_disk_rescue(config, &cause) {
        Ok(rescue_dir) => {
            // Mount succeeded. `console` falls out of scope when this
            // arm returns, so the backend's Drop runs (KD_TEXT
            // restore, termios reset) before the dispatcher in
            // `main` fires the execve.
            return switch_root_and_exec(rescue_dir, &config.rescue.entrypoint);
        }
        Err(e) => e,
    };

    #[cfg(feature = "network-rescue")]
    {
        if config.rescue.network {
            // Bind `console` into this arm so it drops on the closing
            // brace below — after the rescue UI finishes, before we
            // evaluate the halt-with-banner branch. The same ratatui
            // screens drive the rescue flow on every console kind:
            // serial UARTs receive the same vt100/xterm output as the
            // framebuffer console, and terminal emulators (tmux,
            // xterm, picocom) render it identically.
            let mut console = console;
            let mut ui = crate::ui::rescue::make_rescue_ui(&mut *console);
            let net_result = net::try_network_rescue(config, &mut ui, &disk_err.to_string());
            let net_err = match net_result {
                Ok(action) => return Ok(action),
                Err(e) => e,
            };
            // Both disk AND network paths failed. Surface the
            // network error (it's the more recent attempt) chained
            // under the original `cause` so the banner shows every
            // step.
            return Ok(halt_with_banner(NmblError::Rescue {
                stage: "network-rescue-failed",
                source: Box::new(net_err),
            }));
        }
    }

    // Either the feature was off or the operator disabled network
    // rescue — fall back to the structured halt with the disk-rescue
    // error surfaced. `console` falls out of scope on the function
    // return so VT/termios are restored before the dispatcher fires
    // the halt syscall.
    let _ = console;
    let _ = &disk_err; // silence unused warning when feature is off
    Ok(halt_with_banner(NmblError::Rescue {
        stage: "disk-rescue-failed",
        source: Box::new(disk_err),
    }))
}

/// Build a [`TerminalAction::Execve`] for the operator-configured
/// shell (`cfg.paths.shell`) with an empty environment. Mirrors the
/// pre-refactor `exec_embedded` body byte-for-byte in terms of which
/// argv/env it constructs — the only difference is that the syscall
/// itself is deferred to the dispatcher in `main`.
///
/// The `cause` is moved into the [`crate::terminal::EmergencyBanner`]
/// so the dispatcher can render the operator-facing banner
/// immediately before the execve.
///
/// Returns `Err(NmblError::Rescue { stage, ... })` on the rare path
/// where the configured shell path or argv contains an interior NUL
/// — execve cannot proceed and the caller halts with a banner.
pub fn exec_embedded(config: &Config, cause: NmblError) -> Result<TerminalAction> {
    let shell_path = config.paths.shell.as_path();
    let argv0_bytes: Vec<u8> = shell_path
        .file_name()
        .map(|n| n.as_encoded_bytes().to_vec())
        .unwrap_or_else(|| shell_path.as_os_str().as_encoded_bytes().to_vec());

    // Interior NUL in a config-supplied path is astronomically unlikely
    // but still has to be handled. Surface as Rescue{stage:"shell-path-nul"}
    // so the banner makes the failure mode obvious.
    let path_c =
        CString::new(shell_path.as_os_str().as_encoded_bytes()).map_err(|_| NmblError::Rescue {
            stage: "shell-path-nul",
            source: Box::new(NmblError::ConfigInvalid {
                reason: "shell path contains interior NUL".to_string(),
                context: format!("preparing execve of {}", shell_path.display()),
            }),
        })?;
    let argv0_c = CString::new(argv0_bytes).map_err(|_| NmblError::Rescue {
        stage: "shell-argv0-nul",
        source: Box::new(NmblError::ConfigInvalid {
            reason: "shell argv0 contains interior NUL".to_string(),
            context: format!("preparing execve of {}", shell_path.display()),
        }),
    })?;

    Ok(TerminalAction::Execve {
        path: path_c,
        argv: vec![argv0_c],
        env: Vec::new(),
        banner: Some(crate::terminal::EmergencyBanner::new(config, cause)),
        rescue_handoff: true,
    })
}

/// Build a [`TerminalAction::HaltWithBanner`] for the no-rescue path.
/// Used for [`RescueMode::None`] installs where no toolkit ships and
/// the kindest UX is to stop — rather than leave the operator at an
/// inert PID 1.
///
/// The banner text is rendered by the dispatcher; this constructor
/// only packages the cause so every halt-with-banner producer goes
/// through the same code path.
pub fn halt_with_banner(cause: NmblError) -> TerminalAction {
    TerminalAction::HaltWithBanner { cause }
}

/// Switch from the initramfs root into `new_root` and produce a
/// [`TerminalAction::Execve`] for `/bin/sh`.
///
/// Mirrors the busybox `switch_root(8)` dance: `chdir(new_root)` →
/// `mount --move . /` (MS_MOVE) → `chroot(.)` → `chdir(/)`. The
/// actual `execve` is deferred to the dispatcher in `main` so any
/// console handles still on the stack have been dropped first.
///
/// Replaces `pivot_root(2)`, which always returns `EINVAL` when the
/// outgoing root is the initramfs rootfs pseudo-filesystem. After
/// MS_MOVE the initramfs is detached and no longer reachable via any
/// path.
pub(crate) fn switch_root_and_exec(new_root: &Path, entrypoint: &Path) -> Result<TerminalAction> {
    // Step 1: cd into the new root (the mounted squashfs).
    chdir(new_root).map_err(|source| NmblError::Rescue {
        stage: "switch-root",
        source: Box::new(NmblError::Io {
            source: io::Error::from_raw_os_error(source as i32),
            context: format!("chdir({})", new_root.display()),
        }),
    })?;

    // Step 2: Move the new-root mount to /, replacing the initramfs
    // rootfs. MS_MOVE reassigns the mount point atomically.
    nix::mount::mount(
        Some("."),
        "/",
        Option::<&str>::None,
        MsFlags::MS_MOVE,
        Option::<&str>::None,
    )
    .map_err(|source| NmblError::Rescue {
        stage: "switch-root",
        source: Box::new(NmblError::Io {
            source: io::Error::from_raw_os_error(source as i32),
            context: "mount --move . /".to_string(),
        }),
    })?;

    // Step 3: chroot into the new `/` (the squashfs).
    chroot(".").map_err(|source| NmblError::Rescue {
        stage: "switch-root",
        source: Box::new(NmblError::Io {
            source: io::Error::from_raw_os_error(source as i32),
            context: "chroot(.)".to_string(),
        }),
    })?;

    // Step 4: Update the cwd to the new root.
    chdir("/").map_err(|source| NmblError::Rescue {
        stage: "switch-root",
        source: Box::new(NmblError::Io {
            source: io::Error::from_raw_os_error(source as i32),
            context: "chdir(/) after chroot".to_string(),
        }),
    })?;

    // Step 5: Populate /dev in the new root. The MS_MOVE above detached
    // the initramfs devtmpfs, so the rescue root's /dev is an empty
    // mountpoint with no /dev/console. The dispatcher in `main` re-opens
    // /dev/console to redirect the child's stdio before execve, and the
    // full-system entrypoint (`/init`) also does its own `exec bash <
    // /dev/console` — both need a populated /dev. Mount devtmpfs here so
    // the device nodes exist before either consumer runs. Non-fatal: the
    // entrypoint's own `mount -t devtmpfs ... || true` tolerates a stale
    // mount, and the dispatcher's stdio redirect is soft on this path.
    mount_dev_in_new_root();

    build_rescue_shell_action(entrypoint)
}

/// Mount `devtmpfs` at `/dev` in the freshly switched-root rescue root
/// so `/dev/console` (and friends) exist before the dispatcher's stdio
/// redirect and before the rescue entrypoint runs.
///
/// Best-effort by design: any failure is logged at warn level and the
/// caller proceeds. The full-system `/init` re-mounts devtmpfs itself
/// (`mount -t devtmpfs ... || true`), and the busybox image's stdio
/// only needs the node to exist, so a partial setup here never strands
/// the operator. `EBUSY` (already mounted) is treated as success.
fn mount_dev_in_new_root() {
    use crate::{nmbl_info, nmbl_warn};

    let dev = Path::new("/dev");
    match std::fs::create_dir_all(dev) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => nmbl_warn!("rescue: could not create /dev in new root: {e}"),
    }
    match crate::sys::mount::mount_fs(None, dev, "devtmpfs", "mode=755,nosuid") {
        Ok(()) => nmbl_info!("rescue: mounted /dev in new root"),
        Err(NmblError::Mount {
            source: nix::errno::Errno::EBUSY,
            ..
        }) => nmbl_info!("rescue: /dev already mounted in new root (EBUSY)"),
        Err(e) => nmbl_warn!("rescue: could not mount /dev in new root: {e}"),
    }
}

/// Construct the [`TerminalAction::Execve`] for the rescue entrypoint
/// inside the freshly switched-root rescue root with a minimal
/// `TERM=linux` + `PATH` environment. Shared by the disk and network
/// rescue paths. The entrypoint is `config.rescue.entrypoint`: the flat
/// busybox image leaves it at the default `/bin/sh`; the full recovery
/// system pins it to `/init` (a bash PID-1 script). No banner: the
/// rescue UI has already taken the operator through its own screens, so
/// a second emergency banner would be redundant.
fn build_rescue_shell_action(entrypoint: &Path) -> Result<TerminalAction> {
    let entry_bytes = entrypoint.as_os_str().as_encoded_bytes();
    let path_c = CString::new(entry_bytes).map_err(|_| NmblError::Rescue {
        stage: "exec-shell",
        source: Box::new(NmblError::ConfigInvalid {
            reason: "rescue entrypoint path contains interior NUL".to_string(),
            context: format!("preparing execve of {}", entrypoint.display()),
        }),
    })?;
    // argv0 = basename of the entrypoint (e.g. "sh" or "init"), falling
    // back to the full path if it has no file name component.
    let argv0_bytes: Vec<u8> = entrypoint
        .file_name()
        .map(|n| n.as_encoded_bytes().to_vec())
        .unwrap_or_else(|| entry_bytes.to_vec());
    let argv0_c = CString::new(argv0_bytes).map_err(|_| NmblError::Rescue {
        stage: "exec-shell",
        source: Box::new(NmblError::ConfigInvalid {
            reason: "rescue argv0 contains interior NUL".to_string(),
            context: format!("preparing execve of {}", entrypoint.display()),
        }),
    })?;
    let term_c = CString::new("TERM=linux").map_err(|_| NmblError::Rescue {
        stage: "exec-shell",
        source: Box::new(NmblError::ConfigInvalid {
            reason: "TERM environment string contains interior NUL".to_string(),
            context: format!("preparing execve of {}", entrypoint.display()),
        }),
    })?;
    let path_env_c =
        CString::new("PATH=/bin:/sbin:/usr/bin:/usr/sbin").map_err(|_| NmblError::Rescue {
            stage: "exec-shell",
            source: Box::new(NmblError::ConfigInvalid {
                reason: "PATH environment string contains interior NUL".to_string(),
                context: format!("preparing execve of {}", entrypoint.display()),
            }),
        })?;

    Ok(TerminalAction::Execve {
        path: path_c,
        argv: vec![argv0_c],
        env: vec![term_c, path_env_c],
        banner: None,
        rescue_handoff: true,
    })
}

/// Resolve the on-disk path of the external rescue squashfs.
///
/// `rescue.sfs_path` is interpreted as a path RELATIVE TO THE BOOT
/// PARTITION ROOT; a leading `/` is tolerated and stripped so the
/// mountpoint join keeps the runtime mountpoint instead of replacing
/// it. When `sfs_path` is absent the basename
/// [`DEFAULT_SFS_BASENAME`] is used.
///
/// The runtime mountpoint comes from
/// [`Config::runtime_boot_mountpoint`], which Phase 0.5 populates after
/// `mount_boot` succeeds. In legacy embedded-config mode that field is
/// `None` — there is no NMBL-mounted boot partition, so external rescue
/// is not supported and this function surfaces a
/// `NmblError::Rescue { stage: "locate-sfs", … }` instead of fabricating
/// a path that would not resolve.
pub fn locate_sfs(config: &Config) -> Result<PathBuf> {
    let mountpoint =
        config
            .runtime_boot_mountpoint
            .as_deref()
            .ok_or_else(|| NmblError::Rescue {
                stage: "locate-sfs",
                source: Box::new(NmblError::ConfigInvalid {
                    reason:
                        "external rescue requires bootstrap mode: the runtime boot mountpoint is \
                         only known after Phase 0.5 mounts the boot partition, but this NMBL \
                         instance is running in legacy embedded-config mode"
                            .to_string(),
                    context: "resolving rescue.sfs_path against the runtime boot mountpoint"
                        .to_string(),
                }),
            })?;

    let relative: PathBuf = match config.rescue.sfs_path.as_deref() {
        Some(p) => strip_leading_slash(p).to_path_buf(),
        None => PathBuf::from(DEFAULT_SFS_BASENAME),
    };
    Ok(mountpoint.join(relative))
}

/// Strip a single leading `/` so [`Path::join`] keeps the mountpoint
/// instead of replacing it. Mirrors the helper in
/// [`crate::config::resolve_full_config_path`].
fn strip_leading_slash(p: &Path) -> &Path {
    p.strip_prefix("/").unwrap_or(p)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;
    use crate::config::RescueConfig;
    use std::path::Path;

    #[test]
    fn rescue_mode_default_is_embedded() {
        assert_eq!(RescueMode::default(), RescueMode::Embedded);
    }

    #[test]
    fn rescue_mode_parses_kebab_case() {
        #[derive(Debug, Deserialize)]
        struct W {
            mode: RescueMode,
        }
        let parse = |s: &str| -> RescueMode {
            let w: W = toml::from_str(s).expect("parses");
            w.mode
        };
        assert_eq!(parse(r#"mode = "embedded""#), RescueMode::Embedded);
        assert_eq!(parse(r#"mode = "external""#), RescueMode::External);
        assert_eq!(parse(r#"mode = "none""#), RescueMode::None);
    }

    #[test]
    fn rescue_mode_rejects_unknown_string() {
        #[derive(Debug, Deserialize)]
        struct W {
            #[allow(dead_code)]
            mode: RescueMode,
        }
        toml::from_str::<W>(r#"mode = "bogus""#).expect_err("unknown mode must reject");
    }

    fn cfg_with(rescue: RescueConfig, mountpoint: Option<PathBuf>) -> Config {
        let mut c = Config::recovery_default();
        c.rescue = rescue;
        c.runtime_boot_mountpoint = mountpoint;
        c
    }

    #[test]
    fn locate_sfs_defaults_to_mountpoint_plus_basename() {
        let c = cfg_with(RescueConfig::default(), Some(PathBuf::from("/mnt/boot")));
        assert_eq!(
            locate_sfs(&c).expect("default sfs path resolves"),
            Path::new("/mnt/boot/nmbl-rescue.sfs"),
        );
    }

    #[test]
    fn locate_sfs_joins_relative_override() {
        let c = cfg_with(
            RescueConfig {
                mode: RescueMode::External,
                sfs_path: Some(PathBuf::from("foo.sfs")),
                ..RescueConfig::default()
            },
            Some(PathBuf::from("/mnt/boot")),
        );
        assert_eq!(
            locate_sfs(&c).expect("relative override resolves"),
            Path::new("/mnt/boot/foo.sfs"),
        );
    }

    #[test]
    fn locate_sfs_strips_leading_slash_on_override() {
        let c = cfg_with(
            RescueConfig {
                mode: RescueMode::External,
                sfs_path: Some(PathBuf::from("/foo.sfs")),
                ..RescueConfig::default()
            },
            Some(PathBuf::from("/mnt/boot")),
        );
        assert_eq!(
            locate_sfs(&c).expect("leading-slash override resolves"),
            Path::new("/mnt/boot/foo.sfs"),
        );
    }

    #[test]
    fn locate_sfs_joins_nested_override() {
        let c = cfg_with(
            RescueConfig {
                mode: RescueMode::External,
                sfs_path: Some(PathBuf::from("/custom/r.sfs")),
                ..RescueConfig::default()
            },
            Some(PathBuf::from("/mnt/boot")),
        );
        assert_eq!(
            locate_sfs(&c).expect("nested override resolves"),
            Path::new("/mnt/boot/custom/r.sfs"),
        );
    }

    #[test]
    fn build_rescue_shell_action_honours_entrypoint() {
        // The full recovery system pins /init; the default flat image
        // leaves it at /bin/sh. argv0 is the basename in both cases.
        for (entry, argv0) in [("/init", "init"), ("/bin/sh", "sh")] {
            let action = build_rescue_shell_action(Path::new(entry)).expect("action must build");
            match action {
                TerminalAction::Execve {
                    path,
                    argv,
                    banner,
                    rescue_handoff,
                    ..
                } => {
                    assert_eq!(path.as_bytes(), entry.as_bytes());
                    let argv0_c = argv.first().expect("argv must have argv0");
                    assert_eq!(argv0_c.as_bytes(), argv0.as_bytes());
                    assert!(banner.is_none(), "rescue exec carries no banner");
                    assert!(
                        rescue_handoff,
                        "rescue exec must mark the handoff so a failed \
                         /dev/console redirect is non-fatal",
                    );
                }
                other => panic!("expected Execve, got {other:?}"),
            }
        }
    }

    #[test]
    fn dispatch_embedded_returns_execve_action() {
        // mode=Embedded must yield TerminalAction::Execve pointed at
        // config.paths.shell. No syscall fires — this is the whole
        // point of the type-driven flow.
        let mut cfg = Config::recovery_default();
        cfg.rescue.mode = RescueMode::Embedded;
        cfg.paths.shell = PathBuf::from("/bin/test-embedded-shell");
        let console: Box<dyn Console> = Box::new(crate::ui::console::NoopConsole::new());
        let cause = NmblError::Io {
            source: std::io::Error::other("synthetic"),
            context: "rescue test".to_string(),
        };

        let action = dispatch(&cfg, console, cause).expect("embedded dispatch must succeed");
        match action {
            TerminalAction::Execve {
                path,
                banner,
                rescue_handoff,
                ..
            } => {
                assert_eq!(path.as_bytes(), b"/bin/test-embedded-shell");
                let banner = banner.expect("embedded execve must carry a banner");
                assert_eq!(banner.shell_path, PathBuf::from("/bin/test-embedded-shell"),);
                assert!(
                    rescue_handoff,
                    "embedded rescue exec must also mark the handoff",
                );
            }
            other => panic!("expected Execve from Embedded mode, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_none_returns_halt_with_banner() {
        // mode=None must yield TerminalAction::HaltWithBanner; the
        // dispatcher in main is responsible for printing + halting.
        let mut cfg = Config::recovery_default();
        cfg.rescue.mode = RescueMode::None;
        let console: Box<dyn Console> = Box::new(crate::ui::console::NoopConsole::new());
        let cause = NmblError::ConfigInvalid {
            reason: "synthetic".to_string(),
            context: "rescue test".to_string(),
        };

        let action = dispatch(&cfg, console, cause).expect("none dispatch must succeed");
        match action {
            TerminalAction::HaltWithBanner { cause } => match cause {
                NmblError::ConfigInvalid { reason, .. } => {
                    assert_eq!(reason, "synthetic");
                }
                other => panic!("HaltWithBanner cause should round-trip, got {other:?}"),
            },
            other => panic!("expected HaltWithBanner from None mode, got {other:?}"),
        }
    }

    #[test]
    fn locate_sfs_without_mountpoint_is_locate_sfs_error() {
        let c = cfg_with(RescueConfig::default(), None);
        let err = locate_sfs(&c).expect_err("missing mountpoint must error");
        match err {
            NmblError::Rescue { stage, source } => {
                assert_eq!(stage, "locate-sfs");
                match *source {
                    NmblError::ConfigInvalid { reason, .. } => {
                        assert!(
                            reason.contains("bootstrap mode") || reason.contains("embedded-config"),
                            "diagnostic should explain the mode constraint, got: {reason}",
                        );
                    }
                    other => panic!("expected ConfigInvalid inside Rescue, got {other:?}"),
                }
            }
            other => panic!("expected Rescue variant, got {other:?}"),
        }
    }
}
