//! Emergency-shell entrypoint (PLAN.md §6.3 / §9).
//!
//! This module is one of the few crate sites permitted to replace the
//! current process via `execve(2)` (see also `src/panic.rs` and
//! `src/sys/activation.rs`). When any top-level phase returns `Err`,
//! `main` routes the error through [`drop_to_emergency`], which:
//!
//! 1. Runs the [`crate::ui::run_emergency_screen`] TUI over the splash
//!    backend (or a tty/serial fallback) to ask the operator whether
//!    they want to reboot or get a shell.
//! 2. On [`EmergencyChoice::Reboot`] calls `reboot(RB_AUTOBOOT)`.
//! 3. On [`EmergencyChoice::Shell`] prints the operator-facing banner
//!    including the full error chain and a variant-specific hint, then
//!    hands off to [`crate::rescue::dispatch`]. The dispatcher decides
//!    whether to `execve(2)` the embedded busybox, loop-mount the
//!    external rescue squashfs, fetch one over HTTP, or halt — see
//!    `src/rescue/mod.rs`.
//!
//! On success the function does not return — either `reboot` reboots
//! the machine or the chosen shell process inherits PID 1. The
//! signature uses [`std::convert::Infallible`] to document that
//! contract at the type level: a caller that does
//! `let _: Infallible = drop_to_emergency(...);` will never proceed.
//!
//! Failure modes are themselves "no-return": if `rescue::dispatch`
//! returns `Err` (no rescue path reachable, embedded execve failed,
//! …) we print one last diagnostic and halt the system via
//! `reboot(RB_HALT_SYSTEM)` — a kernel panic is preferable to silently
//! returning to a caller that expects `Infallible`.

use std::convert::Infallible;

use nix::sys::reboot::{RebootMode, reboot};

use crate::config::Config;
use crate::error::{NmblError, format_chain};
use crate::nmbl_warn;
use crate::rescue;
use crate::ui::console::{Console, open_console};
use crate::ui::{EmergencyChoice, run_emergency_screen};

/// Print the operator-facing emergency banner and hand off to the
/// rescue dispatcher. Does not return on success; on dispatch failure
/// halts the system rather than returning to the caller.
///
/// `console` is the live boot console when the orchestrator still has
/// one — phase-failure paths pass it down BY OWNERSHIP so the rescue
/// dispatcher can DROP it before any `execve(2)` (and so restore VT
/// text mode + termios via the backend's `Drop` impl). Without that
/// drop the kernel keeps the VT in `KD_GRAPHICS` and the operator's
/// shell runs invisibly under a frozen splash frame. When `None`
/// (panic-recovery re-exec, or initial console bring-up failed) we
/// open a tty console as a last resort; that path forces
/// `panic_recovery=true` so the splash code is never re-entered.
pub fn drop_to_emergency(
    console: Option<Box<dyn Console>>,
    config: &Config,
    err: NmblError,
) -> Infallible {
    // Take ownership of the boot console so the rescue dispatcher can
    // DROP it before any `execve(2)` into the rescue shell. Without
    // that drop the TtyConsole / SplashConsole Drop impl never runs,
    // so the VT stays in KD_GRAPHICS and the operator's shell renders
    // invisibly under a frozen splash frame.
    //
    // When the caller has no live console (panic-recovery re-exec, or
    // early-phase failure before open_console), open a fresh tty
    // console; `panic_recovery=true` keeps us out of the splash code
    // path which may itself be the cause of the failure we are
    // recovering from. The same handle is reused for the emergency
    // TUI and the network-rescue screens — never two parallel
    // terminals.
    let mut console = match console {
        Some(c) => c,
        None => match open_console(config, true) {
            Ok(c) => c,
            Err(open_err) => {
                nmbl_warn!(
                    "emergency console bring-up failed: {}; defaulting to reboot",
                    format_chain(&open_err as &dyn std::error::Error),
                );
                eprintln!("[nmbl] operator (or timeout) chose reboot");
                let _ = reboot(RebootMode::RB_AUTOBOOT);
                halt_with("reboot(RB_AUTOBOOT) returned; halting");
            }
        },
    };

    // Pretty Shell is re-entrant: when the operator exits the
    // emulated shell we drop back to the emergency picker so they can
    // choose again. The loop terminates on any "no-return" choice
    // (Reboot, Shell) because `handle_choice` returns `Infallible`.
    //
    // Without the `image-splash` feature the loop has no `continue`
    // branch — every iteration diverges via `handle_choice` — so
    // clippy's `never_loop` lint fires. Suppress it here: the loop
    // shape is intentional and matches the feature-on path so the
    // diff between builds stays minimal.
    #[cfg_attr(not(feature = "image-splash"), allow(clippy::never_loop))]
    loop {
        let choice = run_emergency_screen(&mut *console, &err);
        #[cfg(feature = "image-splash")]
        if matches!(choice, crate::ui::EmergencyChoice::PrettyShell) {
            if let Err(e) = crate::ui::pretty_shell::run_pretty_shell(&mut *console, config) {
                nmbl_warn!(
                    "pretty-shell session failed: {}",
                    format_chain(&e as &dyn std::error::Error)
                );
            }
            // Re-display the emergency menu.
            continue;
        }
        // All remaining choices diverge inside `handle_choice` (it
        // returns `Infallible`). The empty match consumes the
        // uninhabited type without re-entering the loop.
        match handle_choice(choice, console, config, &err) {}
    }
}

/// Act on the operator's emergency-screen choice. `console` is the
/// live boot console (owned) the caller routed down; on the shell
/// branch it is threaded into `rescue::dispatch` so the network-rescue
/// screens paint through the same backend (no second `/dev/console`
/// grab, no flicker between splash and tty) and the dispatcher drops
/// the box before `execve`.
fn handle_choice(
    choice: EmergencyChoice,
    console: Box<dyn Console>,
    config: &Config,
    err: &NmblError,
) -> Infallible {
    match choice {
        EmergencyChoice::Reboot => {
            // Tear down the boot console before reboot so the VT mode
            // and termios are restored even on this path.
            drop(console);
            eprintln!("[nmbl] operator (or timeout) chose reboot");
            let _ = reboot(RebootMode::RB_AUTOBOOT);
            // reboot() returned Err — fall through to the same halt
            // path execve uses, so we still preserve Infallible.
            halt_with("reboot(RB_AUTOBOOT) returned; halting");
        }
        EmergencyChoice::Shell => exec_shell(console, config, err),
        // The PrettyShell branch is intercepted by the outer loop in
        // `drop_to_emergency` before it ever reaches here — that path
        // is non-diverging and re-enters the picker. Pinning it as
        // `unreachable!()` would violate the `panic`/`unreachable` lint
        // bans; instead we fall through to the regular shell exec,
        // which is the safest "we got into a bad state" default.
        #[cfg(feature = "image-splash")]
        EmergencyChoice::PrettyShell => exec_shell(console, config, err),
    }
}

/// Execute the chosen-shell path: print the banner with the error
/// chain so the operator has context, then hand off to the rescue
/// dispatcher. Does not return on success; on dispatch failure halts
/// the system.
fn exec_shell(console: Box<dyn Console>, config: &Config, err: &NmblError) -> Infallible {
    print_banner(config, err);

    // rescue::dispatch returns Result<Infallible>: success is the
    // noreturn path (process image is replaced), so the Ok arm matches
    // against an uninhabited type. Any Err means no rescue strategy
    // could complete — we log the failure chain and halt. The Box is
    // moved into the dispatcher which drops it before any execve so
    // the backend's Drop impl restores VT text mode and termios.
    match rescue::dispatch(config, console, err) {
        Ok(infallible) => match infallible {},
        Err(dispatch_err) => {
            eprintln!(
                "[nmbl] EMERGENCY RESCUE DISPATCH FAILED: {}",
                format_chain(&dispatch_err as &dyn std::error::Error)
            );
            halt_with("rescue dispatch failed; halting")
        }
    }
}

/// Print the full operator-facing banner: header, suggested action,
/// the error chain. Plain ASCII — the early-userspace console may not
/// have UTF-8 box-drawing glyphs.
fn print_banner(config: &Config, err: &NmblError) {
    let separator = "=".repeat(72);
    eprintln!("{separator}");
    eprintln!("NMBL: dropped to emergency shell");
    eprintln!("{separator}");
    eprintln!();
    eprintln!("Suggested action:");
    eprintln!("  {}", suggested_action(err));
    eprintln!();
    eprintln!("Error chain:");
    let chain = format_chain(err as &dyn std::error::Error);
    for line in chain.lines() {
        eprintln!("  {line}");
    }
    eprintln!();
    eprintln!(
        "Shell: {}  (will execve next)",
        config.paths.shell.display()
    );
    eprintln!("Type `exit` to reboot, or fix the issue and re-exec /init.");
    eprintln!("{separator}");
}

/// One-line operator hint per error variant. Exhaustive `match` so
/// adding a new variant to [`NmblError`] becomes a compile error here
/// rather than a silently missing diagnostic at boot.
fn suggested_action(err: &NmblError) -> String {
    match err {
        NmblError::Config { .. } => "Check /etc/nmbl/config.toml syntax.".to_string(),
        NmblError::Io { context, .. } => format!("Filesystem op failed: {context}."),
        NmblError::ConfigInvalid { reason, context } => {
            format!("Config invalid: {context}: {reason}.")
        }
        NmblError::Mount {
            src, dst, fstype, ..
        } => {
            let src_display = match src {
                Some(p) => p.display().to_string(),
                None => "<none>".to_string(),
            };
            format!(
                "Try: mount -t {fstype} {src_display} {dst}.",
                dst = dst.display()
            )
        }
        NmblError::Umount { dst, .. } => format!("Try: umount -l {}.", dst.display()),
        NmblError::Module { name, path, .. } => {
            format!("Try: insmod {} for {name}.", path.display())
        }
        NmblError::KexecLoad { .. } => {
            "Check the chosen generation's kernel/initrd paths are valid files.".to_string()
        }
        NmblError::KexecReturned { .. } => {
            "kexec actually executed but returned — kernel rejected the image.".to_string()
        }
        NmblError::DeviceTimeout { device, timeout_ms } => format!(
            "Device {} didn't appear in {timeout_ms}ms. Check /dev and activation logs.",
            device.display()
        ),
        NmblError::NoGenerations { searched } => format!(
            "No system-N-link entries found under {}. Verify the system filesystem is mounted.",
            searched.display()
        ),
        NmblError::Tui { .. } => "TUI failed — fall back to serial mode in config.".to_string(),
        NmblError::Activation { kind, .. } => {
            format!("Activation '{kind}' failed; check the relevant tool's stderr above.")
        }
        NmblError::Bootstrap { stage, .. } => format!(
            "Bootstrap stage '{stage}' failed; check bootstrap.toml and the boot partition."
        ),
        NmblError::Rescue { stage, .. } => {
            format!("Rescue stage '{stage}' failed; check the rescue squashfs/network state.")
        }
        NmblError::Panicked { report_path } => format!(
            "Recovered from a panic; report at {}.",
            report_path.display()
        ),
        NmblError::Shell { .. } => "Failed to exec the emergency shell itself. Reboot.".to_string(),
    }
}

/// Print a one-line final-fallback message and halt. Returns
/// `Infallible` because [`reboot`] with `RB_HALT_SYSTEM` does not
/// return on success and we route the error case into a second halt
/// attempt (then `_exit(1)`).
fn halt_with(reason: &str) -> ! {
    eprintln!("[nmbl] {reason}");
    let _ = reboot(RebootMode::RB_HALT_SYSTEM);
    // reboot() returned Err — kernel refused (lacking CAP_SYS_BOOT in
    // a test/sandbox, or we are not PID 1). Best we can do is _exit.
    // SAFETY: libc::_exit is async-signal-safe and unconditionally
    // terminates the process; no crate wraps it (rustix issue #844).
    unsafe { libc::_exit(1) };
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn io_err(ctx: &str) -> NmblError {
        NmblError::Io {
            source: std::io::Error::other("test"),
            context: ctx.to_string(),
        }
    }

    #[test]
    fn suggested_action_for_io_mentions_context() {
        let s = suggested_action(&io_err("mounting /tmp"));
        assert!(s.contains("mounting /tmp"), "{s}");
    }

    #[test]
    fn suggested_action_for_device_timeout_includes_device_and_time() {
        let e = NmblError::DeviceTimeout {
            device: PathBuf::from("/dev/nvme0n1p2"),
            timeout_ms: 15_000,
        };
        let s = suggested_action(&e);
        assert!(s.contains("/dev/nvme0n1p2"), "{s}");
        assert!(s.contains("15000ms"), "{s}");
    }

    #[test]
    fn suggested_action_for_mount_renders_command_hint() {
        let e = NmblError::Mount {
            src: Some(PathBuf::from("/dev/sda1")),
            dst: PathBuf::from("/mnt/system"),
            fstype: "ext4".to_string(),
            source: nix::Error::from(nix::errno::Errno::EINVAL),
        };
        let s = suggested_action(&e);
        assert!(s.contains("mount -t ext4 /dev/sda1 /mnt/system"), "{s}");
    }

    #[test]
    fn suggested_action_for_no_generations_includes_path() {
        let e = NmblError::NoGenerations {
            searched: PathBuf::from("/mnt/system/nix/var/nix/profiles"),
        };
        let s = suggested_action(&e);
        assert!(s.contains("/mnt/system/nix/var/nix/profiles"), "{s}");
    }

    #[test]
    fn suggested_action_for_panicked_includes_report_path() {
        let e = NmblError::Panicked {
            report_path: PathBuf::from("/run/nmbl-panic-1.txt"),
        };
        let s = suggested_action(&e);
        assert!(s.contains("/run/nmbl-panic-1.txt"), "{s}");
    }

    #[test]
    fn suggested_action_for_activation_includes_kind() {
        let e = NmblError::Activation {
            kind: "luks-password".to_string(),
            source: Box::new(io_err("inner")),
        };
        let s = suggested_action(&e);
        assert!(s.contains("luks-password"), "{s}");
    }
}
