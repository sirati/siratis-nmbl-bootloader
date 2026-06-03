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
//! 3. On [`EmergencyChoice::RawShell`] opens the console picker dialog
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
//! ## EmergencyChoice::RawShell — in-process flow (not execve)
//!
//! The `[Raw Shell]` entry on the emergency menu used to translate into
//! a `TerminalAction::Execve` aimed at `config.paths.shell`. As of the
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
//! longer reachable via the `[Raw Shell]` choice. The legacy rescue
//! dispatch path (`rescue::dispatch`) still produces `Execve` /
//! `switch_root`-style actions for the OTHER rescue modes (embedded
//! / external squashfs), reached from inside the picker-spawned shell
//! itself — but the menu choice now hands control to the picker,
//! not to the dispatcher.

mod banner;
mod dispatch;
mod recovery;
#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
mod tests;

pub use banner::{print_banner, print_halt_banner};
pub(crate) use dispatch::{dispatch_emergency_choice, refuse_on_seal_failure};

use crate::config::Config;
use crate::error::{NmblError, format_chain};
use crate::nmbl_warn;
use crate::sys::poller::LocalSender;
use crate::terminal::TerminalAction;
use crate::ui::app::App;
use crate::ui::console::{Console, open_console};
use crate::ui::{
    SessionInteraction, build_emergency_app, build_message, default_items,
    resolve_emergency_timeout,
};

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
/// The picker is **re-entrant**: the Raw Shell, Pretty Shell, Retry
/// boot, and Verify kexec readiness branches all return control to
/// this loop when their sub-flow exits or fails. Only the Reboot
/// branch — and the success arms of Retry/Verify — diverge into a
/// [`TerminalAction`] that `main` fires after the stack has unwound.
///
/// [`RawShell`]: EmergencyChoice::RawShell
pub async fn drop_to_emergency(
    console: Box<dyn Console>,
    config: &Config,
    err: NmblError,
    session: &SessionInteraction,
    sender: &LocalSender,
) -> TerminalAction {
    // SEAL ON ENTRY (G1): before the emergency menu (which can offer a
    // shell) renders, cap the lock PCR and close every TPM-unsealed
    // mapper. The idempotent latch makes the per-choice G3 seal a no-op;
    // sealing here closes the window between the menu rendering and the
    // operator picking a shell. On a seal failure we refuse all
    // interactive context and halt with the seal-failure banner.
    if let Err(seal_err) = crate::policy::seal_secrets(config.tpm.require_tpm, sender).await {
        return refuse_on_seal_failure(seal_err);
    }
    let mut console = console;

    // Build the emergency App once and reuse it across every iteration
    // of the picker loop so:
    //   1. The auto-reboot countdown deadline (latched on the first
    //      call to `run_emergency_screen_with_app`) survives a return
    //      from a modal / sub-flow — re-entering the error screen
    //      does NOT restart the 30s timer. If the timer already
    //      elapsed during another screen, the next visit reboots
    //      immediately.
    //   2. The selection / scroll state on the menu survives a return
    //      from a modal — the operator lands back where they were.
    //   3. Sub-flows (retry, verify) can overlay a status / modal on
    //      top of the menu via `app.modal` so the menu remains visible
    //      behind the dialog.
    let message = build_message(&err);
    let items = default_items();
    // `App<'static>` so the emergency-action sub-flows can pass `app`
    // to `BootReporter::overlay`, which requires an inner-`'static`
    // bound. `build_emergency_app(&[])` uses an empty generations
    // slice which is `'static`, so the inferred lifetime here is
    // already `'static` — the explicit annotation merely pins it.
    let app: App<'static> = build_emergency_app(&message, &items, session);

    // Resolve the auto-reboot countdown once: an operator-configured
    // `emergency_timeout_secs` overrides the built-in 30 s default.
    let emergency_timeout = resolve_emergency_timeout(config);

    recovery::drive_recovery(
        &mut *console,
        app,
        config,
        session,
        emergency_timeout,
        sender,
    )
    .await
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
    // SEAL ON ENTRY (G2): this synchronous bootstrap/panic/pre-console
    // path runs OUTSIDE the runtime, so seal via the blocking shape
    // before opening a console that leads to the (shell-offering)
    // emergency menu. Refuse all interactive context on a seal failure.
    // The inner `drop_to_emergency` (G1) re-seals idempotently.
    if let Err(seal_err) = crate::policy::seal_secrets_blocking(config.tpm.require_tpm) {
        return refuse_on_seal_failure(seal_err);
    }
    // These call sites have no prior boot session (initial bring-up
    // failure, panic-recovery re-exec, pre-console phases), so no
    // keypress could have happened yet — a fresh latch is correct.
    let session = SessionInteraction::new();
    match open_console(config, true) {
        // Cross into the async interactive phase: build the LocalRuntime,
        // spawn the reserve poller, and block_on the emergency session.
        // On a runtime-build failure fall back to Reboot (same safety
        // default as a console bring-up failure below).
        //
        // Wrap the freshly-opened console in the central interaction-latch
        // layer so a keypress on THIS emergency session (the bootstrap /
        // panic / pre-console failure path) cancels the auto-reboot
        // countdown — same as every other session. `drop_to_emergency`
        // then sees a wrapped console just like the local boot path does.
        Ok(c) => match crate::ui::block_on_tui_with_poller(|sender| async move {
            drop_to_emergency(
                Box::new(crate::ui::console::LatchingConsole::new(c, session.clone())),
                config,
                err,
                &session,
                &sender,
            )
            .await
        }) {
            Ok(action) => action,
            Err(rt_err) => {
                nmbl_warn!(
                    "emergency runtime build failed: {}; defaulting to reboot",
                    format_chain(&rt_err as &dyn std::error::Error),
                );
                eprintln!("[nmbl] operator (or timeout) chose reboot");
                TerminalAction::Reboot
            }
        },
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
