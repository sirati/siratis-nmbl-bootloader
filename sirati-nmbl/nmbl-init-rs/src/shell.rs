//! Emergency-shell entrypoint (PLAN.md §6.3 / §9).
//!
//! When any top-level phase returns `Err`, `main` routes the error
//! through [`drop_to_emergency`], which:
//!
//! 1. Runs the [`crate::ui::run_emergency_screen`] TUI over the splash
//!    backend (or a tty/serial fallback) to ask the operator whether
//!    they want to reboot or get a shell.
//! 2. On [`EmergencyChoice::Reboot`] returns
//!    [`TerminalAction::Reboot`].
//! 3. On [`EmergencyChoice::Shell`] hands off to
//!    [`crate::rescue::dispatch`], which produces a
//!    [`TerminalAction::Execve`] (or `HaltWithBanner` when no rescue
//!    path is reachable). The dispatcher decides whether to execve
//!    the embedded busybox, loop-mount the external rescue squashfs,
//!    fetch one over HTTP, or halt — see `src/rescue/mod.rs`.
//!
//! All terminal-action syscalls — `execve`, `reboot(RB_AUTOBOOT)`,
//! `reboot(RB_HALT_SYSTEM)`, `reboot(RB_KEXEC)` — happen in one
//! place: `main::execute_terminal_action`. By the time control
//! reaches that dispatcher the call stack has fully unwound, so
//! every [`crate::ui::console::Console`] backend's `Drop` impl has
//! already run (KD_TEXT restored, termios reset, fds closed) and the
//! shell that inherits PID 1 sees a clean VT.

use crate::config::Config;
use crate::error::{NmblError, format_chain};
use crate::nmbl_warn;
use crate::rescue;
use crate::terminal::{EmergencyBanner, TerminalAction};
use crate::ui::console::{Console, open_console};
use crate::ui::{EmergencyChoice, run_emergency_screen};

/// Print the operator-facing emergency banner and hand off to the
/// rescue dispatcher. Returns the [`TerminalAction`] the dispatcher
/// in `main` will perform once the call stack has fully unwound.
///
/// `console` is the live boot console the orchestrator still holds.
/// We render the emergency-screen TUI through it, then hand it on to
/// [`rescue::dispatch`] for any further UI (e.g. network-rescue
/// screens). The box is dropped by normal scope exit before the
/// `TerminalAction` is fired in `main`, which is what restores
/// KD_TEXT and termios — without that ordering the freshly-execve'd
/// shell would run invisibly under a frozen splash frame.
pub fn drop_to_emergency(
    console: Box<dyn Console>,
    config: &Config,
    err: NmblError,
) -> TerminalAction {
    // Run the emergency-screen TUI over the live console. The TUI
    // returns the operator's pick (or `Reboot` on the 30s timeout).
    let mut console = console;
    let choice = run_emergency_screen(&mut *console, &err);
    handle_choice(choice, console, config, err)
}

/// Open a fresh tty console (panic-recovery mode skips splash) and
/// then run [`drop_to_emergency`]. Used by call sites that have no
/// live console yet — the initial bring-up failure, the
/// panic-recovery re-exec, the pre-console phases.
///
/// On console bring-up failure we log it, print a reboot reason, and
/// return [`TerminalAction::Reboot`] so the dispatcher reboots
/// instead of leaving the operator at an inert PID 1.
pub fn open_console_and_drop_to_emergency(config: &Config, err: NmblError) -> TerminalAction {
    match open_console(config, true) {
        Ok(c) => drop_to_emergency(c, config, err),
        Err(open_err) => {
            nmbl_warn!(
                "emergency console bring-up failed: {}; defaulting to reboot",
                format_chain(&open_err as &dyn std::error::Error),
            );
            eprintln!("[nmbl] operator (or timeout) chose reboot");
            TerminalAction::Reboot
        }
    }
}

/// Act on the operator's emergency-screen choice. `console` is the
/// live boot console (owned) the caller routed down; on the shell
/// branch it is threaded into `rescue::dispatch` so the
/// network-rescue screens paint through the same backend (no second
/// `/dev/console` grab, no flicker between splash and tty).
fn handle_choice(
    choice: EmergencyChoice,
    console: Box<dyn Console>,
    config: &Config,
    err: NmblError,
) -> TerminalAction {
    match choice {
        EmergencyChoice::Reboot => {
            // `console` is owned by this arm and drops on the closing
            // brace via scope exit; the dispatcher in `main` performs
            // `reboot(RB_AUTOBOOT)` only after this whole call has
            // returned.
            let _ = console;
            eprintln!("[nmbl] operator (or timeout) chose reboot");
            TerminalAction::Reboot
        }
        EmergencyChoice::Shell => exec_shell(console, config, err),
    }
}

/// Execute the chosen-shell path: hand off to the rescue dispatcher
/// and let it produce a [`TerminalAction`]. On dispatch failure we
/// print one last diagnostic and collapse to a halt-with-banner.
fn exec_shell(console: Box<dyn Console>, config: &Config, err: NmblError) -> TerminalAction {
    // rescue::dispatch builds a TerminalAction the caller in `main`
    // will fire after every stack-allocated resource is dropped. The
    // box is moved into the dispatcher; either it drops the console
    // inside its arms or returns it dropped through the network-UI
    // helper. Any Err means no rescue strategy could complete — we
    // log the failure chain and surface a halt-with-banner so the
    // operator sees a structured diagnostic.
    match rescue::dispatch(config, console, err) {
        Ok(action) => action,
        Err(dispatch_err) => {
            eprintln!(
                "[nmbl] EMERGENCY RESCUE DISPATCH FAILED: {}",
                format_chain(&dispatch_err as &dyn std::error::Error)
            );
            TerminalAction::HaltWithBanner {
                cause: dispatch_err,
            }
        }
    }
}

/// Print the full operator-facing banner: header, suggested action,
/// the error chain. Plain ASCII — the early-userspace console may
/// not have UTF-8 box-drawing glyphs. Called by the dispatcher in
/// `main` immediately before the execve syscall fires, so the
/// operator sees the chain printed onto the freshly-restored VT.
pub fn print_banner(banner: &EmergencyBanner) {
    let separator = "=".repeat(72);
    eprintln!("{separator}");
    eprintln!("NMBL: dropped to emergency shell");
    eprintln!("{separator}");
    eprintln!();
    eprintln!("Suggested action:");
    eprintln!("  {}", suggested_action(&banner.err));
    eprintln!();
    eprintln!("Error chain:");
    let chain = format_chain(&banner.err as &dyn std::error::Error);
    for line in chain.lines() {
        eprintln!("  {line}");
    }
    eprintln!();
    eprintln!(
        "Shell: {}  (will execve next)",
        banner.shell_path.display()
    );
    eprintln!("Type `exit` to reboot, or fix the issue and re-exec /init.");
    eprintln!("{separator}");
}

/// Print the halt-with-banner banner: same shape as
/// [`print_banner`] but tailored to the no-rescue-toolkit scenario.
/// Called by the dispatcher in `main` immediately before the
/// `reboot(RB_HALT_SYSTEM)` syscall fires.
pub fn print_halt_banner(cause: &NmblError) {
    let separator = "=".repeat(72);
    eprintln!("{separator}");
    eprintln!("NMBL: no rescue toolkit available — halting");
    eprintln!("{separator}");
    eprintln!();
    eprintln!("Configured rescue mode is `none`. The initramfs ships no");
    eprintln!("interactive shell, and the operator did not enable the");
    eprintln!("external squashfs rescue. The system will halt.");
    eprintln!();
    eprintln!("Error chain:");
    let chain = format_chain(cause as &dyn std::error::Error);
    for line in chain.lines() {
        eprintln!("  {line}");
    }
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

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Frame;

    use crate::error::Result;
    use crate::rescue::RescueMode;
    use crate::terminal::TerminalAction;
    use crate::ui::app::App;
    use crate::ui::console::ConsoleKind;

    fn io_err(ctx: &str) -> NmblError {
        NmblError::Io {
            source: std::io::Error::other("test"),
            context: ctx.to_string(),
        }
    }

    /// Scripted in-process [`Console`] for unit-testing
    /// `drop_to_emergency`. Drives a queued sequence of key events on
    /// `poll_key()` and stays in lockstep with the emergency-screen
    /// loop's render/poll cadence.
    ///
    /// Mirrors the `TestConsole` in `ui::emergency::tests`; lives
    /// here because the cross-module visibility rules make the
    /// emergency-module one unreachable from `shell::tests`.
    struct ScriptedConsole {
        events: Vec<Option<KeyEvent>>,
        cursor: usize,
    }

    impl ScriptedConsole {
        fn new(events: Vec<Option<KeyEvent>>) -> Self {
            Self { events, cursor: 0 }
        }
    }

    impl Console for ScriptedConsole {
        fn render(&mut self, _app: &App<'_>) -> Result<()> {
            Ok(())
        }
        fn poll_key(&mut self, _timeout: Duration) -> Result<Option<KeyEvent>> {
            let v = self.events.get(self.cursor).copied().flatten();
            self.cursor = self.cursor.saturating_add(1);
            Ok(v)
        }
        fn size(&self) -> (u16, u16) {
            (80, 24)
        }
        fn kind(&self) -> ConsoleKind {
            ConsoleKind::Tty
        }
        fn draw_with(&mut self, _body: &mut dyn FnMut(&mut Frame<'_>)) -> Result<()> {
            Ok(())
        }
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn drop_to_emergency_returns_execve_on_shell_choice() {
        // Down (selects Shell) + Enter. drop_to_emergency must hand
        // off to rescue::dispatch which, with mode=Embedded
        // (recovery-default), builds a TerminalAction::Execve aimed
        // at config.paths.shell.
        let mut config = Config::recovery_default();
        config.rescue.mode = RescueMode::Embedded;
        // Pin the shell path to a known value so we can assert the
        // execve target without depending on the recovery-default
        // string.
        config.paths.shell = PathBuf::from("/bin/test-emergency-shell");

        let console: Box<dyn Console> = Box::new(ScriptedConsole::new(vec![
            Some(press(KeyCode::Down)),
            Some(press(KeyCode::Enter)),
        ]));

        let action = drop_to_emergency(console, &config, io_err("synthetic boot failure"));

        match action {
            TerminalAction::Execve { path, banner, .. } => {
                let path_bytes = path.as_bytes();
                assert_eq!(
                    path_bytes, b"/bin/test-emergency-shell",
                    "execve target must match the configured shell"
                );
                let banner = banner.expect("emergency execve must carry a banner");
                assert_eq!(banner.shell_path, PathBuf::from("/bin/test-emergency-shell"));
            }
            other => panic!("expected Execve, got {other:?}"),
        }
    }

    #[test]
    fn drop_to_emergency_returns_reboot_on_r_hotkey() {
        // The emergency screen surfaces 'r' as a one-shot reboot
        // hotkey (matches the operator muscle-memory call-out in
        // ui::app::handle_emergency_key). drop_to_emergency must
        // surface that as TerminalAction::Reboot.
        let config = Config::recovery_default();

        let console: Box<dyn Console> =
            Box::new(ScriptedConsole::new(vec![Some(press(KeyCode::Char('r')))]));

        let action = drop_to_emergency(console, &config, io_err("synthetic"));

        match action {
            TerminalAction::Reboot => {}
            other => panic!("expected Reboot, got {other:?}"),
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
