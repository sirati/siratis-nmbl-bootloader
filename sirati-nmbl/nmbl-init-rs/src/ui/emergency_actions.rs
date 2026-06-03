//! Re-entrant emergency-screen actions.
//!
//! Two operator-driven recovery paths that re-use the existing boot
//! phases without an actual reboot:
//!
//! - [`retry_boot`] re-runs phases 3 (activations), 3b (mount), 4
//!   (scan generations) and 5 (selector). The operator typically
//!   picks this after fixing a transient issue at the shell (USB key
//!   re-seated, network restored). On success it returns the
//!   [`TerminalAction`] the selector produced (`Kexec` or `Reboot`).
//!
//! - [`verify_kexec_readiness`] skips phases 3 and 3b — the operator
//!   has manually mounted the system filesystem at the shell, so
//!   `run_all_activations` would either no-op or fight with the live
//!   mounts. We run only phase 4 (scan), confirm with a yes/no modal
//!   ("Found N generations. Boot one?"), and on Yes hand off to the
//!   selector.
//!
//! Both paths surface failures via [`crate::ui::show_modal_error`]
//! and return to the caller's emergency-loop without committing to
//! any terminal action. The caller in [`crate::shell::drop_to_emergency`]
//! re-shows the emergency picker afterwards.

use std::time::Duration;

use ratatui::widgets::Clear;

use crate::activation::{KeyInjection, PasswordSupplier, run_all_activations};
use crate::boot::kexec_into;
use crate::config::Config;
use crate::devices::mount_system_filesystems;
use crate::error::{NmblError, Result, format_chain};
use crate::generations::scan_generations;
use crate::nmbl_info;
use crate::terminal::TerminalAction;
use crate::ui::app::App;
use crate::ui::console::Console;
use crate::ui::{
    BootReporter, ConfirmOutcome, Decision, run_selector, show_modal_confirm_over,
    show_modal_error_over,
};

/// Blank the whole console before handing off from the emergency menu
/// to the selector.
///
/// The emergency menu paints a red `error` block top-left and a white
/// selected-action line; the selector that follows only repaints the
/// cells its own widgets touch. On the diff-rendering tty/serial
/// backend that leaves the menu's stale cells showing through the new
/// screen. The clean menu exits already reset the display — Raw/Pretty
/// Shell suspend+resume the console (`terminal.clear()`), Ctrl+L's log
/// viewer fills the frame — so this mirrors them with a full-area
/// `Clear` (the same widget the modals use to punch a hole) right
/// before the selector's first render.
fn clear_console(console: &mut dyn Console) -> Result<()> {
    console.draw_with(&mut |frame| frame.render_widget(Clear, frame.area()))
}

/// Re-run phases 3 → 5 from the emergency screen and return whatever
/// [`TerminalAction`] the selector produces.
///
/// The supplier provides passphrases for `luks-password` activations
/// the same way the normal boot path does. The caller (`drop_to_emergency`)
/// owns the [`Config`] and [`Console`] for the duration of the call.
///
/// ## Idempotency on retry
///
/// `run_all_activations` does NOT special-case "device already open"
/// errors (cryptsetup luksOpen on an open device exits with code 5).
/// In a normal boot this is correct: a partial activation indicates a
/// broken config and we want to fail loud. On the retry path the
/// operator has already triggered phase 3 once and possibly opened
/// some devices manually at the shell, so a second run will hit
/// EEXIST-style failures on the previously-opened devices.
///
/// We intentionally do NOT refactor `run_all_activations` here — that
/// would change normal-boot semantics. If a future patch teaches the
/// activation runner to tolerate "already open" exit codes the retry
/// path will benefit automatically.
///
/// For now, on `NmblError::Activation` errors the operator is shown a
/// modal and routed back to the emergency picker; they can fall back
/// to `Verify kexec readiness` if every activation has already
/// completed and they only need the selector.
pub async fn retry_boot(
    config: &Config,
    console: &mut dyn Console,
    app: &mut App<'static>,
    supplier: &mut dyn PasswordSupplier,
    sender: &crate::sys::poller::LocalSender,
) -> Result<TerminalAction> {
    nmbl_info!("emergency action: retry boot from config");

    // Phase 3: activations. The reporter overlays the emergency menu
    // App so the menu remains visible behind the progress indicator;
    // the reporter's Drop impl clears `app.modal` on scope exit.
    let injections = {
        let mut reporter =
            BootReporter::overlay(console, app, "phase 3: storage activations (retry)");
        run_all_activations(config, &mut reporter, Some(supplier), sender).await?
    };

    // Phase 3b: mount system filesystems.
    {
        let mut reporter =
            BootReporter::overlay(console, app, "phase 3b: mount system filesystems (retry)");
        mount_system_filesystems(config, &mut reporter, sender).await?;
    }

    run_selector_and_dispatch(config, console, app, &injections).await
}

/// Skip phases 3 and 3b — trust the operator's manual mount — and run
/// only phase 4 (scan generations) + the yes/no confirmation modal.
///
/// On `Yes` hands off to the selector and returns the resulting
/// [`TerminalAction`]; on `No`/Cancel returns `Ok(None)` so the
/// caller re-shows the emergency picker.
///
/// Phase 3 (activations) is intentionally skipped. The operator
/// reached this path because they mounted the system filesystem
/// themselves; activations like LUKS unlock are presumed already
/// done. Calling `run_all_activations` here would either re-prompt
/// for passphrases (annoying) or hit EEXIST on already-open devices
/// (broken). Phase 3b is also skipped because re-mounting an already-
/// mounted filesystem is at best a no-op and at worst confuses the
/// system-root layout — the operator's manual setup is trusted.
///
/// `Ok(None)` means the operator declined to commit; `Err` means
/// scanning the profile directory failed (no generations / IO error)
/// and the caller's modal-error path should fire.
pub async fn verify_kexec_readiness(
    config: &Config,
    console: &mut dyn Console,
    app: &mut App<'static>,
) -> Result<Option<TerminalAction>> {
    nmbl_info!("emergency action: verify kexec readiness");

    let generations = {
        let mut reporter =
            BootReporter::overlay(console, app, "phase 4: scan generations (verify)");
        scan_generations(config, &mut reporter)?
    };

    let count = generations.len();
    let body = format!(
        "Found {count} generation{plural} under {root}.\n\nBoot one of them?",
        plural = if count == 1 { "" } else { "s" },
        root = config.paths.system_root.display(),
    );
    let outcome = show_modal_confirm_over(
        console,
        app,
        "Verify kexec readiness",
        &body,
        "Yes",
        "Back",
        true,
    )
    .await?;
    match outcome {
        ConfirmOutcome::Yes => {
            // No passphrase injection: the operator skipped phase 3.
            let injections: Vec<KeyInjection> = Vec::new();
            // Blank the boot-failed menu (and the just-dismissed confirm
            // modal) before the selector's first render so neither bleeds
            // through on the diff-rendering tty/serial backend.
            clear_console(console)?;
            let decision = run_selector(config, &generations, console, &app.interaction).await?;
            Ok(Some(decision_to_action(
                config,
                &generations,
                &injections,
                decision,
            )?))
        }
        ConfirmOutcome::No | ConfirmOutcome::Cancelled => Ok(None),
    }
}

/// Drive the selector and translate its [`Decision`] into a
/// [`TerminalAction`]. Shared by [`retry_boot`] (and could be used by
/// `verify_kexec_readiness` if we hoist the empty-injection case).
async fn run_selector_and_dispatch(
    config: &Config,
    console: &mut dyn Console,
    app: &mut App<'static>,
    injections: &[KeyInjection],
) -> Result<TerminalAction> {
    let generations = {
        let mut reporter = BootReporter::overlay(console, app, "phase 4: scan generations (retry)");
        scan_generations(config, &mut reporter)?
    };
    // Blank the boot-failed menu before the selector's first render so
    // its red `error` block / selected-action line don't bleed through.
    clear_console(console)?;
    let decision = run_selector(config, &generations, console, &app.interaction).await?;
    decision_to_action(config, &generations, injections, decision)
}

/// Map the selector's [`Decision`] onto a [`TerminalAction`].
///
/// Mirrors the dispatch in `main::select_and_act` — we deliberately
/// duplicate the few-line match rather than expose a public helper
/// from `main.rs`, because:
///   - `main.rs` is the orchestrator binary, not the library;
///   - the retry path translates `Decision::Shell` differently (we
///     can't drop to the emergency shell from inside the emergency
///     screen — that would be an infinite loop), so it surfaces as
///     an `Err` the caller routes through `show_modal_error`.
fn decision_to_action(
    config: &Config,
    generations: &[crate::generations::Generation],
    injections: &[KeyInjection],
    decision: Decision,
) -> Result<TerminalAction> {
    match decision {
        Decision::Boot {
            generation_index,
            cmdline_override,
        } => {
            let Some(target) = generations.get(generation_index) else {
                return Err(NmblError::ConfigInvalid {
                    reason: format!(
                        "selector returned index {generation_index} but only {} generations",
                        generations.len()
                    ),
                    context: "emergency-retry decision dispatch".to_string(),
                });
            };
            // The emergency-retry path does not own the boot's driver-image
            // accumulator (those were loaded — and possibly already torn down or
            // left mounted into a shell, FIX-55 — before this recovery path was
            // reached), so it measures NO driver images: an empty handle leaves
            // PCR-11 event #4 absent. The kernel/initrd/cmdline are still
            // measured on a measure-required build (#28).
            kexec_into(
                config,
                target,
                cmdline_override.as_deref(),
                injections,
                &crate::imageload::DriverImagesHandle::empty(),
            )
        }
        Decision::Reboot => Ok(TerminalAction::Reboot),
        Decision::Shell => Err(NmblError::Tui {
            source: std::io::Error::other(
                "operator chose 'shell' inside the emergency-retry path; \
                 the emergency screen is already where you are",
            ),
        }),
    }
}

/// Surface a recoverable failure from one of the emergency actions
/// via [`show_modal_error`]. The modal blocks until the operator
/// presses any key (or 10 s elapse). Errors from the modal itself
/// are swallowed — by the time we're showing a modal error the
/// console is already in a degraded state.
pub async fn surface_action_failure(
    console: &mut dyn Console,
    app: &mut App<'static>,
    title: &str,
    err: &NmblError,
) {
    let chain = format_chain(err as &dyn std::error::Error);
    let _ = show_modal_error_over(console, app, title, &chain, Duration::from_secs(10)).await;
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::pin::Pin;

    use ratatui::Terminal;
    use ratatui::backend::{Backend, TestBackend};

    use crate::generations::Generation;
    use crate::ui::app::SessionInteraction;
    use crate::ui::console::{ConsoleEvent, ConsoleKind};
    use crate::ui::{build_emergency_app, default_items};

    /// Minimal [`Console`] over a ratatui [`TestBackend`] so a test can
    /// render a screen and then inspect the resulting cell buffer.
    /// `render` and `draw_with` both go through `terminal.draw`, exactly
    /// like the real tty/mock backends, so [`clear_console`] exercises
    /// the same diff-render path that produced the bleed bug.
    struct BufferConsole {
        terminal: Terminal<TestBackend>,
    }

    impl BufferConsole {
        fn new(w: u16, h: u16) -> Self {
            Self {
                terminal: Terminal::new(TestBackend::new(w, h)).expect("test terminal"),
            }
        }

        /// True if every cell is the default blank: a single space with
        /// no fg/bg colour and no modifiers. A residual emergency cell
        /// (the red "boot failed" header, the bordered "error" block,
        /// the highlighted action line) fails this.
        fn is_blank(&self) -> bool {
            let buf = self.terminal.backend().buffer();
            buf.content().iter().all(|cell| {
                cell.symbol() == " "
                    && cell.fg == ratatui::style::Color::Reset
                    && cell.bg == ratatui::style::Color::Reset
                    && cell.modifier.is_empty()
            })
        }

        fn dump(&self) -> String {
            let buf = self.terminal.backend().buffer();
            buf.content().iter().map(|c| c.symbol()).collect()
        }
    }

    impl Console for BufferConsole {
        fn render(&mut self, app: &App<'_>) -> Result<()> {
            // TestBackend's draw is infallible; surface a clean unwrap.
            self.terminal
                .draw(|f| crate::ui::render_current_screen(f, app))
                .expect("TestBackend render");
            Ok(())
        }
        fn poll_event<'a>(
            &'a mut self,
            _timeout: Duration,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<Option<ConsoleEvent>>> + 'a>> {
            Box::pin(async move { Ok(None) })
        }
        fn poll_event_blocking(&mut self, _timeout: Duration) -> Result<Option<ConsoleEvent>> {
            Ok(None)
        }
        fn size(&self) -> (u16, u16) {
            match self.terminal.backend().size() {
                Ok(s) => (s.width, s.height),
                Err(_) => (0, 0),
            }
        }
        fn kind(&self) -> ConsoleKind {
            ConsoleKind::Tty
        }
        fn draw_with(&mut self, body: &mut dyn FnMut(&mut ratatui::Frame<'_>)) -> Result<()> {
            self.terminal
                .draw(|f| body(f))
                .expect("TestBackend draw_with");
            Ok(())
        }
        fn suspend(&mut self) -> Result<()> {
            Ok(())
        }
        fn resume(&mut self) -> Result<()> {
            Ok(())
        }
    }

    /// The two emergency actions (retry boot, verify kexec readiness)
    /// must blank the boot-failed menu before they render the selector,
    /// or the menu's red header / bordered "error" block / highlighted
    /// action line bleed through on the diff-rendering backend. Pin the
    /// `clear_console` helper they both call: after it runs, no
    /// emergency-screen cell survives.
    #[test]
    fn clear_console_blanks_residual_emergency_cells() {
        let session = SessionInteraction::new();
        let items = default_items();
        let app = build_emergency_app("boot phase failed: disk offline", &items, &session);

        let mut console = BufferConsole::new(80, 24);
        // Paint the emergency screen, exactly what the operator sees
        // behind the menu before picking retry / verify.
        console.render(&app).expect("render emergency screen");
        assert!(
            !console.is_blank(),
            "emergency screen must paint visible chrome before the clear"
        );
        assert!(
            console.dump().contains("boot failed"),
            "sanity: the red header is on screen pre-clear"
        );

        // The transition the two fixed actions perform before run_selector.
        clear_console(&mut console).expect("clear_console");

        assert!(
            console.is_blank(),
            "no emergency-screen cell may survive the clear:\n{}",
            console.dump()
        );
    }

    fn fake_gen(number: u32) -> Generation {
        Generation {
            number,
            profile_link: PathBuf::from(format!("/p/system-{number}-link")),
            toplevel: PathBuf::from(format!("/p/toplevel-{number}")),
            kernel: PathBuf::from("/p/kernel"),
            initrd: PathBuf::from("/p/initrd"),
            init_path: PathBuf::from(format!("/p/system-{number}-link/init")),
            kernel_params: Vec::new(),
            label: String::new(),
        }
    }

    #[test]
    fn decision_to_action_reboot_yields_reboot() {
        let cfg = Config::recovery_default();
        let gens = vec![fake_gen(1)];
        let action = decision_to_action(&cfg, &gens, &[], Decision::Reboot)
            .expect("Reboot decision must produce TerminalAction::Reboot");
        assert!(matches!(action, TerminalAction::Reboot));
    }

    #[test]
    fn decision_to_action_shell_yields_tui_error() {
        // The emergency-retry path cannot honour Decision::Shell — the
        // operator is already on the emergency screen, so dropping to
        // the shell from here would either be a no-op or an infinite
        // loop. Pin the explicit `NmblError::Tui` translation.
        let cfg = Config::recovery_default();
        let gens = vec![fake_gen(1)];
        let err = decision_to_action(&cfg, &gens, &[], Decision::Shell)
            .expect_err("Shell decision must produce an error inside retry path");
        assert!(matches!(err, NmblError::Tui { .. }));
    }

    #[test]
    fn decision_to_action_out_of_range_index_yields_config_invalid() {
        // Defence-in-depth: a buggy selector returning an index past
        // the generations slice must produce a structured error, not a
        // panic-on-indexing. The error context names the dispatcher so
        // a future regression is easy to bisect from the boot log.
        let cfg = Config::recovery_default();
        let gens = vec![fake_gen(1)];
        let err = decision_to_action(
            &cfg,
            &gens,
            &[],
            Decision::Boot {
                generation_index: 42,
                cmdline_override: None,
            },
        )
        .expect_err("out-of-range index must error, not panic");
        match err {
            NmblError::ConfigInvalid { context, reason } => {
                assert!(
                    context.contains("emergency-retry"),
                    "context must name the dispatcher: {context}"
                );
                assert!(
                    reason.contains("42"),
                    "reason must mention bad index: {reason}"
                );
            }
            other => panic!("expected ConfigInvalid, got {other:?}"),
        }
    }
}
