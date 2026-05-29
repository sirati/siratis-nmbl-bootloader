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
#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
mod tests;

pub use banner::{print_banner, print_halt_banner};

use crate::config::Config;
use crate::error::{NmblError, format_chain};
use crate::nmbl_warn;
use crate::terminal::TerminalAction;
use crate::ui::app::App;
use crate::ui::console::{Console, open_console};
use crate::ui::emergency_actions::{retry_boot, surface_action_failure, verify_kexec_readiness};
use crate::ui::{
    EmergencyChoice, SessionInteraction, TuiPasswordSupplier, build_emergency_app, build_message,
    default_items, resolve_emergency_timeout, run_emergency_screen_with_app,
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
) -> TerminalAction {
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
    let mut app: App<'static> = build_emergency_app(&message, &items, session);

    // Count of distinct failures surfaced this session. Used so the
    // persistent emergency-screen "error" box can show the LATEST
    // failure (not just the original boot error it latched on entry)
    // along with how many have been seen — see `update_latest_error`.
    let mut error_count: u32 = 0;

    // Resolve the auto-reboot countdown once: an operator-configured
    // `emergency_timeout_secs` overrides the built-in 30 s default.
    let emergency_timeout = resolve_emergency_timeout(config);

    // Re-entrant picker. The Raw Shell, Pretty Shell, Retry boot, and
    // Verify kexec readiness branches all return control to this loop
    // on exit (sub-shell ended, retry failed, operator picked Back).
    // The Reboot branch — and the success arms of Retry/Verify —
    // diverge into a `TerminalAction` and break out via `return`.
    //
    // The Raw Shell branch now runs the in-process picker + multiplexed
    // PTY relay (`crate::ui::console_picker::run_picker_session`);
    // it never produces a `TerminalAction::Execve`. NMBL stays at
    // PID 1 across the shell session.
    loop {
        // Modal state from the prior iteration (if any) must be
        // cleared before re-entering the picker; otherwise a stale
        // overlay would obscure the menu.
        app.modal = None;
        let choice =
            run_emergency_screen_with_app(&mut *console, &mut app, emergency_timeout).await;

        match choice {
            EmergencyChoice::Reboot => {
                eprintln!("[nmbl] operator (or timeout) chose reboot");
                return TerminalAction::Reboot;
            }
            EmergencyChoice::RawShell => {
                run_raw_shell_choice(&mut *console, &mut app, &mut error_count, config).await;
                // Picker session done (shell exited, detached, or cancelled); re-show menu.
                continue;
            }
            #[cfg(feature = "pretty-shell")]
            EmergencyChoice::PrettyShell => {
                run_pretty_shell_choice(&mut *console, &mut app, &mut error_count, config).await;
                continue;
            }
            EmergencyChoice::RetryBoot => {
                let mut supplier = TuiPasswordSupplier::new(config, session);
                if let Some(action) = run_retry_boot_arm(
                    config,
                    &mut *console,
                    &mut app,
                    &mut error_count,
                    &mut supplier,
                )
                .await
                {
                    return action;
                }
                continue;
            }
            EmergencyChoice::VerifyKexecReadiness => {
                if let Some(action) =
                    run_verify_kexec_arm(config, &mut *console, &mut app, &mut error_count).await
                {
                    return action;
                }
                continue;
            }
        }
    }
}

/// Handle the [`EmergencyChoice::RawShell`] picker arm: run the in-process
/// console picker session and show a toast or error modal over the emergency
/// menu so the operator knows what happened before re-entering the loop.
async fn run_raw_shell_choice(
    console: &mut dyn Console,
    app: &mut App<'static>,
    error_count: &mut u32,
    config: &Config,
) {
    match crate::ui::console_picker::run_picker_session(console, config).await {
        Ok(crate::ui::console_picker::PickerSessionOutcome::ShellDetached { targets }) => {
            // Fire-and-forget regime: tell the operator their shell(s) have
            // been started elsewhere so they don't wonder why the menu
            // re-appeared unchanged. Use the overlay variant so the
            // emergency menu remains visible behind the press-any-key toast.
            let body = format_detached_targets(&targets);
            let _ = crate::ui::show_modal_error_over(
                console,
                app,
                "Shell spawned",
                &body,
                std::time::Duration::from_secs(5),
            )
            .await;
        }
        Ok(_) => {}
        Err(e) => {
            let chain = format_chain(&e as &dyn std::error::Error);
            nmbl_warn!("emergency-shell picker session failed: {chain}");
            let _ = crate::ui::show_modal_error_over(
                console,
                app,
                "Emergency shell failed",
                &chain,
                std::time::Duration::from_secs(10),
            )
            .await;
            update_latest_error(app, error_count, "Emergency shell failed", &chain);
        }
    }
}

/// Handle the [`EmergencyChoice::PrettyShell`] arm: launch the alacritty-backed
/// pty terminal and show an error modal if it fails to start.
#[cfg(feature = "pretty-shell")]
async fn run_pretty_shell_choice(
    console: &mut dyn Console,
    app: &mut App<'static>,
    error_count: &mut u32,
    config: &Config,
) {
    if let Err(e) = crate::ui::pretty_shell::run_pretty_shell(console, config).await {
        let chain = format_chain(&e as &dyn std::error::Error);
        nmbl_warn!("pretty-shell session failed: {chain}");
        let _ = crate::ui::show_modal_error_over(
            console,
            app,
            "Pretty Shell failed to start",
            &chain,
            std::time::Duration::from_secs(10),
        )
        .await;
        update_latest_error(app, error_count, "Pretty Shell failed to start", &chain);
    }
}

/// Handle the [`EmergencyChoice::RetryBoot`] picker arm. Returns `Some(action)` if
/// the retry succeeded (caller should return it), or `None` to continue the loop.
async fn run_retry_boot_arm(
    config: &Config,
    console: &mut dyn Console,
    app: &mut App<'static>,
    error_count: &mut u32,
    supplier: &mut TuiPasswordSupplier,
) -> Option<TerminalAction> {
    match retry_boot(config, console, app, supplier).await {
        Ok(action) => Some(action),
        Err(e) => {
            let title = abort_aware_title(&e, "Retry boot failed");
            nmbl_warn!(
                "emergency retry-boot failed: {}",
                format_chain(&e as &dyn std::error::Error)
            );
            surface_action_failure(console, app, title, &e).await;
            update_latest_error(
                app,
                error_count,
                title,
                &format_chain(&e as &dyn std::error::Error),
            );
            None
        }
    }
}

/// Handle the [`EmergencyChoice::VerifyKexecReadiness`] picker arm. Returns
/// `Some(action)` if kexec is ready (caller should return it), or `None` to continue.
async fn run_verify_kexec_arm(
    config: &Config,
    console: &mut dyn Console,
    app: &mut App<'static>,
    error_count: &mut u32,
) -> Option<TerminalAction> {
    match verify_kexec_readiness(config, console, app).await {
        Ok(Some(action)) => Some(action),
        Ok(None) => None,
        Err(e) => {
            let title = abort_aware_title(&e, "Kexec readiness check failed");
            nmbl_warn!(
                "emergency verify-kexec-readiness failed: {}",
                format_chain(&e as &dyn std::error::Error)
            );
            surface_action_failure(console, app, title, &e).await;
            update_latest_error(
                app,
                error_count,
                title,
                &format_chain(&e as &dyn std::error::Error),
            );
            None
        }
    }
}

/// Update the persistent emergency-screen "error" box so it always
/// reflects the *most recent* failure rather than latching the
/// original boot error for the rest of the session. The transient
/// modal that just flashed the failure auto-dismisses; without this
/// the operator would be left staring at the first error again, with
/// no trace of what actually went wrong (e.g. a failed Raw Shell).
///
/// `count` is bumped per call and rendered so repeated failures are
/// visible at a glance; the freshest chain is shown in full underneath.
fn update_latest_error(app: &mut App<'static>, count: &mut u32, title: &str, chain: &str) {
    *count = count.saturating_add(1);
    let message =
        format!("Latest error (#{count}): {title}\n\n{chain}\n\nChoose what to do next.",);
    app.set_emergency_message(message);
}

/// Render the "Shell spawned on …" body for the fire-and-forget
/// success modal. Single target → `Shell spawned on /dev/X`; multiple
/// → comma-separated. Keeps the message single-line so the modal stays
/// readable on serial.
fn format_detached_targets(targets: &[std::path::PathBuf]) -> String {
    let joined = targets
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!("Shell spawned on {joined}")
}

/// Pick the modal title shown when an emergency action returns an
/// error. The operator-abort path uses a distinct "Aborted by operator"
/// banner so the modal reads naturally ("you pressed Esc") instead of
/// the generic action-failed banner that would otherwise apply.
///
/// `OperatorAborted` can surface from any emergency action that
/// internally calls `wait_for` (Retry boot, Kexec readiness check on a
/// future code path) — funneling the rename here means the call sites
/// don't have to duplicate the `matches!` check.
fn abort_aware_title(err: &NmblError, default: &'static str) -> &'static str {
    match err {
        NmblError::OperatorAborted { .. } => "Aborted by operator",
        _ => default,
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
    // These call sites have no prior boot session (initial bring-up
    // failure, panic-recovery re-exec, pre-console phases), so no
    // keypress could have happened yet — a fresh latch is correct.
    let session = SessionInteraction::new();
    match open_console(config, true) {
        // Cross into the async interactive phase: build the LocalRuntime,
        // spawn the reserve poller, and block_on the emergency session.
        // On a runtime-build failure fall back to Reboot (same safety
        // default as a console bring-up failure below).
        Ok(c) => match crate::ui::block_on_tui(drop_to_emergency(c, config, err, &session)) {
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
