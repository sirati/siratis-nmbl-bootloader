//! Emergency-shell entrypoint (PLAN.md §6.3 / §9).
//!
//! When any top-level phase returns `Err`, `main` routes the error
//! through [`drop_to_emergency`], which:
//!
//! 1. Runs the [`crate::ui::run_emergency_screen`] TUI over the splash
//!    backend (or a tty/serial fallback) to ask the operator whether
//!    they want to reboot, open an in-process shell on the operator's
//!    chosen console(s), open the Pretty Shell (feature
//!    `image-splash`), retry the normal boot flow, or verify kexec
//!    readiness without re-running the activation phases.
//! 2. On [`EmergencyChoice::Reboot`] returns
//!    [`TerminalAction::Reboot`].
//! 3. On [`EmergencyChoice::Shell`] opens the console picker dialog
//!    ([`crate::ui::console_picker`]), forks ONE busybox onto a PTY,
//!    and runs the multiplex relay loop in PID 1
//!    ([`crate::ui::console_relay`]). When the shell exits — or the
//!    operator cancels the picker — control returns to the emergency
//!    menu. This branch never produces a `TerminalAction`; NMBL stays
//!    at PID 1.
//! 4. On [`EmergencyChoice::PrettyShell`] (feature `image-splash`)
//!    runs the alacritty-backed pty terminal inside the TUI box.
//!    When the operator exits that shell — or it fails to start — we
//!    re-enter this picker so they can try another action. This
//!    branch never produces a `TerminalAction`; control stays here.
//! 5. On [`EmergencyChoice::RetryBoot`] re-runs phases 3, 3b, 4 and
//!    surfaces the selector; on success returns the resulting
//!    [`TerminalAction`], on failure shows a modal and re-shows the
//!    menu.
//! 6. On [`EmergencyChoice::VerifyKexecReadiness`] skips phases 3 and
//!    3b (operator presumed to have mounted manually), scans
//!    generations, confirms with a yes/no modal, and either returns a
//!    [`TerminalAction`] or re-shows the menu.
//!
//! All terminal-action syscalls — `execve`, `reboot(RB_AUTOBOOT)`,
//! `reboot(RB_HALT_SYSTEM)`, `reboot(RB_KEXEC)` — happen in one
//! place: `main::execute_terminal_action`. By the time control
//! reaches that dispatcher the call stack has fully unwound, so
//! every [`crate::ui::console::Console`] backend's `Drop` impl has
//! already run (KD_TEXT restored, termios reset, fds closed) and the
//! shell that inherits PID 1 sees a clean VT.
//!
//! ## EmergencyChoice::Shell — in-process flow (not execve)
//!
//! The `[Shell]` entry on the emergency menu used to translate into a
//! `TerminalAction::Execve` aimed at `config.paths.shell`. As of the
//! console-picker work it is now an **in-process** flow:
//!
//! 1. Open the picker dialog ([`crate::ui::console_picker`]) on the
//!    live console.
//! 2. On commit, fork ONE busybox onto a PTY and run the multiplex
//!    relay loop in PID 1 ([`crate::ui::console_relay`]).
//! 3. When the shell exits, re-show the emergency menu (the same way
//!    `Pretty Shell` already does).
//!
//! NMBL stays at PID 1 throughout; `TerminalAction::Execve` is no
//! longer reachable via the `[Shell]` choice. The legacy rescue
//! dispatch path (`rescue::dispatch`) still produces `Execve` /
//! `switch_root`-style actions for the OTHER rescue modes (embedded
//! / external squashfs), reached from inside the picker-spawned shell
//! itself — but the menu choice now hands control to the picker,
//! not to the dispatcher.

use crate::config::Config;
use crate::error::{NmblError, format_chain};
use crate::nmbl_warn;
use crate::terminal::{EmergencyBanner, TerminalAction};
use crate::ui::console::{Console, open_console};
use crate::ui::emergency_actions::{retry_boot, surface_action_failure, verify_kexec_readiness};
use crate::ui::{EmergencyChoice, TuiPasswordSupplier, run_emergency_screen};

/// Print the operator-facing emergency banner and drive the
/// re-entrant emergency picker. Returns the [`TerminalAction`] the
/// dispatcher in `main` will perform once the call stack has fully
/// unwound.
///
/// `console` is the live boot console the orchestrator still holds.
/// We render the emergency-screen TUI through it; the in-process
/// shell, pretty-shell, retry-boot, and verify-kexec-readiness
/// branches all keep the same `console` borrowed across the loop, so
/// there is no second `/dev/console` grab and no flicker between
/// splash and tty backends.
///
/// The picker is **re-entrant**: the Shell, Pretty Shell, Retry boot,
/// and Verify kexec readiness branches all return control to this
/// loop when their sub-flow exits or fails. Only the Reboot branch —
/// and the success arms of Retry/Verify — diverge into a
/// [`TerminalAction`] that `main` fires after the stack has unwound.
///
/// [`Shell`]: EmergencyChoice::Shell
pub fn drop_to_emergency(
    console: Box<dyn Console>,
    config: &Config,
    err: NmblError,
) -> TerminalAction {
    let mut console = console;

    // Re-entrant picker. The Shell, Pretty Shell, Retry boot, and
    // Verify kexec readiness branches all return control to this loop
    // on exit (sub-shell ended, retry failed, operator picked Back).
    // The Reboot branch — and the success arms of Retry/Verify —
    // diverge into a `TerminalAction` and break out via `return`.
    //
    // The Shell branch now runs the in-process picker + multiplexed
    // PTY relay (`crate::ui::console_picker::run_picker_session`);
    // it never produces a `TerminalAction::Execve`. NMBL stays at
    // PID 1 across the shell session.
    loop {
        let choice = run_emergency_screen(&mut *console, &err);

        match choice {
            EmergencyChoice::Reboot => {
                eprintln!("[nmbl] operator (or timeout) chose reboot");
                return TerminalAction::Reboot;
            }
            EmergencyChoice::Shell => {
                if let Err(e) =
                    crate::ui::console_picker::run_picker_session(&mut *console, config)
                {
                    let chain = format_chain(&e as &dyn std::error::Error);
                    nmbl_warn!("emergency-shell picker session failed: {chain}");
                    let _ = crate::ui::show_modal_error(
                        &mut *console,
                        "Emergency shell failed",
                        &chain,
                        std::time::Duration::from_secs(10),
                    );
                }
                // Picker session done (shell exited or cancelled);
                // re-show the emergency menu.
                continue;
            }
            #[cfg(feature = "image-splash")]
            EmergencyChoice::PrettyShell => {
                if let Err(e) = crate::ui::pretty_shell::run_pretty_shell(&mut *console, config) {
                    let chain = format_chain(&e as &dyn std::error::Error);
                    nmbl_warn!("pretty-shell session failed: {chain}");
                    let _ = crate::ui::show_modal_error(
                        &mut *console,
                        "Pretty Shell failed to start",
                        &chain,
                        std::time::Duration::from_secs(10),
                    );
                }
                continue;
            }
            EmergencyChoice::RetryBoot => {
                let mut supplier = TuiPasswordSupplier::new(config);
                match retry_boot(config, &mut *console, &mut supplier) {
                    Ok(action) => return action,
                    Err(e) => {
                        nmbl_warn!(
                            "emergency retry-boot failed: {}",
                            format_chain(&e as &dyn std::error::Error)
                        );
                        surface_action_failure(&mut *console, "Retry boot failed", &e);
                        continue;
                    }
                }
            }
            EmergencyChoice::VerifyKexecReadiness => {
                match verify_kexec_readiness(config, &mut *console) {
                    Ok(Some(action)) => return action,
                    Ok(None) => continue,
                    Err(e) => {
                        nmbl_warn!(
                            "emergency verify-kexec-readiness failed: {}",
                            format_chain(&e as &dyn std::error::Error)
                        );
                        surface_action_failure(
                            &mut *console,
                            "Kexec readiness check failed",
                            &e,
                        );
                        continue;
                    }
                }
            }
        }
    }
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
        fn suspend(&mut self) -> Result<()> {
            Ok(())
        }
        fn resume(&mut self) -> Result<()> {
            Ok(())
        }
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn drop_to_emergency_shell_choice_cancels_picker_then_reboots() {
        // The `[Shell]` choice now opens the in-process picker dialog
        // (in-process flow, NOT TerminalAction::Execve). The script
        // navigates Down to Shell + Enter, then Esc to cancel the
        // picker, then 'r' on the re-displayed emergency menu to
        // commit a reboot. Verifying the produced TerminalAction is
        // Reboot — not Execve — pins the architectural change.
        let mut config = Config::recovery_default();
        config.rescue.mode = RescueMode::Embedded;
        config.paths.shell = PathBuf::from("/bin/test-emergency-shell");

        let console: Box<dyn Console> = Box::new(ScriptedConsole::new(vec![
            // Emergency menu: Down (Shell) + Enter → enter picker.
            Some(press(KeyCode::Down)),
            Some(press(KeyCode::Enter)),
            // Picker dialog: Esc to cancel back to the emergency menu.
            Some(press(KeyCode::Esc)),
            // Emergency menu (second iteration): 'r' commits Reboot.
            Some(press(KeyCode::Char('r'))),
        ]));

        let action = drop_to_emergency(console, &config, io_err("synthetic boot failure"));
        assert!(
            matches!(action, TerminalAction::Reboot),
            "Shell choice must NOT produce a TerminalAction::Execve any more; \
             got {action:?}"
        );
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
