//! LUKS-specific helpers: verifying-spinner runner and wrong-password modal.

use std::time::Duration;

use crate::config::{Activation, Config};
use crate::error::Result;
use crate::generations::Generation;
use crate::sys::activation::ProcessOutcome;
use crate::sys::ops::ExecOps;
use crate::ui::app::{App, Screen};
use crate::ui::console::Console;

use super::helpers::wrap_runner_error;

/// Non-blocking input slice polled on each verifying-spinner tick so a
/// Ctrl+L / Esc lands within ~one frame without slowing the reap. Short
/// enough that the `TICK_INTERVAL`-driven spinner cadence is unaffected.
const VERIFY_INPUT_SLICE: Duration = Duration::from_millis(0);

/// Resolved outcome of [`handle_wrong_password`]. Distinct from
/// [`crate::ui::WrongPasswordOutcome`] (the modal-level reply) because
/// the helper also drives the in-process shell session before
/// returning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WrongPasswordHandled {
    /// Re-prompt for the passphrase and re-run cryptsetup.
    TryAgain,
    /// Operator picked [Reboot] on the wrong-password modal.
    Reboot,
    /// Operator opened a recovery shell (Pretty Shell or Raw Shell)
    /// and the shell has now exited. Caller turns this into
    /// [`NmblError::WrongPasswordShellExited`] so the standard
    /// emergency menu can surface and offer [Retry boot from config].
    ShellExited,
}

/// Run a `luks-password` activation under [`run_with_tick`], using the
/// boot console to paint a verifying-spinner on the passphrase modal
/// every ~150 ms. Returns the same [`ProcessOutcome`] [`run`] would.
///
/// The App owned here is throwaway: it carries a `Screen::Passphrase`
/// in verifying mode so the existing `render_passphrase` view paints
/// the same modal the operator saw during input, with a spinner row
/// overlaid. We do NOT share state with the supplier's App (which was
/// consumed inside `collect_stdin`) — a fresh App is cheaper than
/// threading a mutable reference through the supplier trait.
pub(super) async fn run_luks_with_spinner<S: ExecOps>(
    activation: &Activation,
    stdin_slice: Option<&[u8]>,
    console: &mut dyn Console,
    ops: &mut S,
) -> Result<ProcessOutcome> {
    let label = activation
        .prompt_label
        .as_deref()
        .unwrap_or("Verifying passphrase")
        .to_string();

    // Throwaway App parked on the passphrase modal in verifying mode.
    // `generations` is empty — the modal renders the same way against
    // any (or no) generation slice. `'static` works because we hand
    // out `&[]` at the constructor.
    let empty: [Generation; 0] = [];
    let mut app = App::new(&empty);
    app.screen = Screen::Passphrase {
        prompt_label: label,
        // Buffer length carries through to the dotted mask. We can't
        // know the operator's actual byte count cheaply here without
        // crossing the supplier API; the stdin slice is one byte per
        // input character (the supplier doesn't add a newline), so
        // its length is a faithful approximation.
        buffer: zeroize::Zeroizing::new("*".repeat(stdin_slice.map_or(0, <[u8]>::len))),
        cursor: 0,
        verifying: true,
        spinner_frame: 0,
        // Display-only verifying frame; the checkbox is irrelevant here
        // (this path never reads it back). Default unchecked.
        select_generation: false,
    };

    // Paint the first verifying frame BEFORE the child starts — so the
    // operator sees the spinner pop up the instant they press Enter,
    // not after the first 150 ms tick.
    let _ = console.render(&app);

    let tick = |c: &mut dyn Console, a: &mut App<'_>| {
        // Service input WHILE cryptsetup verifies so Ctrl+L still opens
        // the boot-log viewer (and Esc closes it) instead of the operator
        // staring at a spinner they can't escape. We route through the
        // shared `on_key` dispatch — the same one the selector uses — so
        // Ctrl+L toggles `Screen::Log` over the verifying modal. The
        // passphrase-spinner only advances while the log is NOT open so
        // the operator can read it without the frame flickering back.
        if let Ok(Some(key)) = c.poll_key(VERIFY_INPUT_SLICE) {
            a.on_key(key);
        }
        if !matches!(a.screen, Screen::Log { .. }) {
            a.tick_passphrase_spinner();
        }
        let _ = c.render(a);
    };

    // The tick closure needs &mut on both console and app at the same
    // time. We can't capture both in one FnMut because run_with_tick
    // would then need to thread them; instead, the closure captures
    // `&mut *console` and `&mut app` via mutable borrows held in this
    // function frame. Use a RefCell-free split: keep the closure
    // borrowing locals declared before the call.
    //
    // (Rust 1.83 closure borrow rules now make this clean — the
    // closure captures `&mut app` and `&mut *console` directly.)
    let mut cb = || tick(console, &mut app);

    // Route the spinner-reaping exec through `ops` so the dry-run
    // presence-checks `cryptsetup` instead of forking it. `RealSys`
    // forwards to `sys::activation::run_with_tick`, preserving the
    // genuine spinner; `DryRunSys` records a finding and never forks.
    let outcome = ops
        .run_with_tick(
            &activation.binary,
            &activation.argv,
            stdin_slice,
            &mut cb as &mut dyn FnMut(),
        )
        .await
        .map_err(|source| wrap_runner_error(activation, source))?;

    // If the operator opened the log viewer (Ctrl+L) during verification
    // and never closed it, the App is parked on `Screen::Log` rather than
    // the passphrase modal. Pop it back so `set_passphrase_verifying`
    // (which is a no-op / debug-asserts off the passphrase screen) lands
    // on the right screen and the next transition starts clean.
    if matches!(app.screen, Screen::Log { .. })
        && let Some(prev) = app.return_screen.take()
    {
        app.screen = *prev;
    }

    // Done verifying — clear the overlay and repaint once so the next
    // screen transition (success → boot-status; wrong-pw → modal) starts
    // from a clean slate. Guard the setter: a degraded path could leave
    // the App off the passphrase screen, and the setter debug-asserts.
    if matches!(app.screen, Screen::Passphrase { .. }) {
        app.set_passphrase_verifying(false);
    }
    let _ = console.render(&app);

    Ok(outcome)
}

/// Render the wrong-password modal, dispatch on the operator's choice,
/// and — for the shell branches — drive the chosen in-process shell
/// session. Returns when the operator's choice has been fully
/// resolved (modal closed; shell, if any, has exited).
pub(super) async fn handle_wrong_password(
    config: &Config,
    console: &mut dyn Console,
    _activation: &Activation,
    attempt: u32,
) -> Result<WrongPasswordHandled> {
    use crate::ui::{WrongPasswordOutcome, show_wrong_password_modal};

    match show_wrong_password_modal(console, attempt).await? {
        WrongPasswordOutcome::TryAgain => Ok(WrongPasswordHandled::TryAgain),
        WrongPasswordOutcome::Reboot => Ok(WrongPasswordHandled::Reboot),
        #[cfg(feature = "pretty-shell")]
        WrongPasswordOutcome::PrettyShell => {
            // This path has no poller `sender` in scope; the shell spawn is
            // sync and never touches the sender, so a sender-less `RealSys`
            // satisfies the `ExecOps::spawn_shell` route safely.
            let mut ops = crate::sys::ops::RealSys::sync_only();
            if let Err(e) =
                crate::ui::pretty_shell::run_pretty_shell(&mut ops, console, config).await
            {
                let chain = crate::error::format_chain(&e as &dyn std::error::Error);
                crate::nmbl_warn!("wrong-password pretty-shell failed: {chain}");
                let _ = crate::ui::show_modal_error(
                    console,
                    "Pretty Shell failed to start",
                    &chain,
                    std::time::Duration::from_secs(10),
                )
                .await;
            }
            Ok(WrongPasswordHandled::ShellExited)
        }
        WrongPasswordOutcome::RawShell => {
            // Console-picker + multiplexed busybox PTY (overlap) or
            // fire-and-forget (no overlap). Errors are surfaced via a
            // modal-error so the wrong-password flow doesn't crash the
            // boot — we still want the operator to be able to retry.
            let mut ops = crate::sys::ops::RealSys::sync_only();
            match crate::ui::console_picker::run_picker_session(&mut ops, console, config).await {
                Ok(crate::ui::console_picker::PickerSessionOutcome::ShellDetached { targets }) => {
                    let joined = targets
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let _ = crate::ui::show_modal_error(
                        console,
                        "Shell spawned",
                        &format!("Shell spawned on {joined}"),
                        std::time::Duration::from_secs(5),
                    )
                    .await;
                }
                Ok(_) => {}
                Err(e) => {
                    let chain = crate::error::format_chain(&e as &dyn std::error::Error);
                    crate::nmbl_warn!("wrong-password shell-picker session failed: {chain}");
                    let _ = crate::ui::show_modal_error(
                        console,
                        "Emergency shell failed",
                        &chain,
                        std::time::Duration::from_secs(10),
                    )
                    .await;
                }
            }
            Ok(WrongPasswordHandled::ShellExited)
        }
    }
}
