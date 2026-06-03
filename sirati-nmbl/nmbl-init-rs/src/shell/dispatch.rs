//! Emergency-menu choice dispatch.
//!
//! [`dispatch_emergency_choice`] routes one [`EmergencyChoice`] to its
//! sub-flow and returns `Some(action)` for terminal choices or `None`
//! when control should return to the re-entrant picker. Shared by the
//! local picker ([`super::drop_to_emergency`]) and the remote-TUI session
//! loop (`crate::ui::remote`) so both drive identical dispatch. Extracted
//! from `shell/mod.rs` to keep that file under the file-size limit.

use crate::config::Config;
use crate::error::{NmblError, format_chain};
use crate::nmbl_warn;
use crate::sys::poller::LocalSender;
use crate::terminal::TerminalAction;
use crate::ui::app::App;
use crate::ui::console::Console;
use crate::ui::emergency_actions::{retry_boot, surface_action_failure, verify_kexec_readiness};
use crate::ui::{EmergencyChoice, SessionInteraction, SkipSelector, TuiPasswordSupplier};

/// Run one emergency-menu choice. Returns `Some(action)` when the choice
/// is terminal (the caller should return it) or `None` when the sub-flow
/// returned control to the menu (the caller should re-show it).
///
/// Shared by the local re-entrant picker ([`drop_to_emergency`]) and the
/// remote-TUI per-session loop (`crate::ui::remote`) so both paths drive
/// exactly the same dispatch — only the [`Console`] differs (the live
/// boot console vs. the remote operator's pty).
pub(crate) async fn dispatch_emergency_choice(
    choice: EmergencyChoice,
    console: &mut dyn Console,
    app: &mut App<'static>,
    error_count: &mut u32,
    config: &Config,
    session: &SessionInteraction,
    sender: &LocalSender,
) -> Option<TerminalAction> {
    match choice {
        EmergencyChoice::Reboot => {
            eprintln!("[nmbl] operator (or timeout) chose reboot");
            Some(TerminalAction::Reboot)
        }
        EmergencyChoice::RawShell => {
            // SEAL BEFORE SPAWN (G3): cap the lock PCR + close every
            // TPM-unsealed mapper, then hand the spawn helper the
            // unforgeable witness. On a seal failure there is no `Sealed`,
            // so we cannot — and must not — open a shell: divert to refuse.
            match crate::policy::seal_secrets(config.tpm.require_tpm, sender).await {
                Ok(sealed) => {
                    run_raw_shell_choice(sealed, console, app, error_count, config).await;
                    // Picker session done (shell exited, detached, or cancelled); re-show menu.
                    None
                }
                Err(e) => Some(refuse_on_seal_failure(e, config, sender).await),
            }
        }
        #[cfg(feature = "pretty-shell")]
        EmergencyChoice::PrettyShell => {
            match crate::policy::seal_secrets(config.tpm.require_tpm, sender).await {
                Ok(sealed) => {
                    run_pretty_shell_choice(sealed, console, app, error_count, config).await;
                    None
                }
                Err(e) => Some(refuse_on_seal_failure(e, config, sender).await),
            }
        }
        EmergencyChoice::RetryBoot => {
            // Rescue retry always shows the selector (the operator
            // explicitly chose "retry boot from config"), so the
            // skip-selector latch is a fresh, never-read default here —
            // the checkbox renders and toggles but `run_selector_and_dispatch`
            // does not consult it.
            let skip_selector = SkipSelector::new();
            let mut supplier = TuiPasswordSupplier::new(config, session, &skip_selector);
            run_retry_boot_arm(config, console, app, error_count, &mut supplier, sender).await
        }
        EmergencyChoice::VerifyKexecReadiness => {
            run_verify_kexec_arm(config, console, app, error_count).await
        }
    }
}

/// Divert to a NON-INTERACTIVE refuse when the seal fails (FIX-27): a
/// present-but-uncappable TPM, a `requireTpm` box with no TPM, or a mapper
/// that will not close. NEVER offer a shell.
///
/// Routes through [`crate::policy::refuse_unsigned`] (M1): even on a
/// `SealFailed`, `relock_and_refuse` runs (BEST-EFFORT cap, then
/// close-mappers + relock + sentinel) and yields the type-gated
/// [`TerminalAction::RebootIntoRescue`] — the correct safe outcome (the
/// imminent reboot is the real lock boundary). The `Sealed` witness is
/// minted by the best-effort seal inside `relock_and_refuse`, so the
/// refuse countdown is reachable only after that teardown.
pub(crate) async fn refuse_on_seal_failure(
    err: crate::policy::SealFailed,
    config: &Config,
    sender: &LocalSender,
) -> TerminalAction {
    nmbl_warn!(
        "seal-on-rescue failed; refusing to open a shell, relocking and rebooting into rescue: {}",
        format_chain(err.cause() as &dyn std::error::Error)
    );
    crate::policy::refuse_unsigned(config, err.into_cause(), sender).await
}

/// Handle the [`EmergencyChoice::RawShell`] picker arm: run the in-process
/// console picker session and show a toast or error modal over the emergency
/// menu so the operator knows what happened before re-entering the loop.
///
/// Takes the `Sealed` witness by value: a shell cannot be reached on this
/// arm without proof that [`crate::policy::seal_secrets`] ran (G3).
async fn run_raw_shell_choice(
    sealed: crate::policy::Sealed,
    console: &mut dyn Console,
    app: &mut App<'static>,
    error_count: &mut u32,
    config: &Config,
) {
    match crate::ui::console_picker::run_picker_session(sealed, console, config).await {
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
    sealed: crate::policy::Sealed,
    console: &mut dyn Console,
    app: &mut App<'static>,
    error_count: &mut u32,
    config: &Config,
) {
    if let Err(e) = crate::ui::pretty_shell::run_pretty_shell(sealed, console, config).await {
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
    sender: &LocalSender,
) -> Option<TerminalAction> {
    match retry_boot(config, console, app, supplier, sender).await {
        Ok(action) => Some(action),
        // A refused retry (a generation/measure gate refused inside
        // `kexec_into`) must RELOCK + reboot into rescue, not loop back to
        // the shell-offering menu via a modal (FIX-35 residual). Route it
        // through `run_refuse_screen` — cap → close-mappers → sentinel →
        // relock + the non-interactive countdown — so the refuse relocks
        // and the type-gated `RebootIntoRescue` terminus is returned. The
        // TPM is already capped by G1 on this path, so this is not a
        // fail-open today, but a refused retry must still refuse properly.
        Err(NmblError::PolicyRefused { cause }) => {
            nmbl_warn!(
                "emergency retry-boot refused; relocking and rebooting into rescue: {}",
                format_chain(cause.as_ref() as &dyn std::error::Error)
            );
            Some(crate::policy::run_refuse_screen(config, console, *cause, sender).await)
        }
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
