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

mod embedded;
mod locate;
mod switch_root;
mod types;

pub use embedded::{exec_embedded, halt_with_banner};
pub use locate::locate_sfs;
pub(crate) use switch_root::switch_root_and_exec;
pub use types::RescueMode;

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::terminal::TerminalAction;
use crate::ui::console::Console;

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

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;
    use crate::config::RescueConfig;
    use serde::Deserialize;
    use std::path::{Path, PathBuf};

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
            let action = switch_root::build_rescue_shell_action(Path::new(entry))
                .expect("action must build");
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
