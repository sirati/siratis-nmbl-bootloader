use nmbl_init::boot::kexec_into;
use nmbl_init::config::Config;
use nmbl_init::error::{NmblError, Result};
use nmbl_init::generations::scan_generations;
use nmbl_init::nmbl_info;
use nmbl_init::shell::drop_to_emergency;
use nmbl_init::terminal::TerminalAction;
use nmbl_init::ui::console::{Console, LatchingConsole};
#[cfg(not(feature = "stateful"))]
use nmbl_init::ui::run_selector;
use nmbl_init::ui::{
    BootReporter, Decision, SessionInteraction, SkipSelector, TuiPasswordSupplier,
};

use super::phases::run_phases_post_console;
#[cfg(feature = "stateful")]
use super::stateful::{select_default_with_stateful, select_with_stateful};

/// Headless default-boot decision: the `Decision::Boot` a fully
/// unattended boot would settle on without rendering the selector.
/// Drives the skip-selector fast path (LUKS unlocked with the "Select
/// Generation" checkbox off) and is reusable as a later dry-run's
/// headless entry point.
///
/// With the `stateful` feature on it delegates to
/// `select_default_with_stateful` so rollback `ForcePick` / `Exhausted`
/// still override the "just boot the default" intent; otherwise it
/// returns the plain active-profile index the legacy selector's timeout
/// would have chosen. Behaviour is byte-identical to the inline skip
/// branches it replaces.
#[cfg(feature = "stateful")]
fn default_boot_decision(
    config: &Config,
    generations: &[nmbl_init::generations::Generation],
) -> Result<Decision> {
    select_default_with_stateful(config, generations)
}

#[cfg(not(feature = "stateful"))]
fn default_boot_decision(
    config: &Config,
    generations: &[nmbl_init::generations::Generation],
) -> Result<Decision> {
    Ok(Decision::Boot {
        generation_index: nmbl_init::generations::active_generation_index(
            generations,
            &config.paths.nix_profiles_dir,
        ),
        cmdline_override: None,
    })
}

/// Run phases 4→6 (generation discovery, UI, decision dispatch). Kept
/// separate so the call sites for `drop_to_emergency` stay obvious.
///
/// Phase 4 uses a [`BootReporter`] around `console` so the operator
/// keeps seeing the boot-status screen while we walk the profiles
/// directory. The reporter is dropped before phase 5 so the bare
/// console can be handed to `run_selector`, which swaps the App over
/// to the boot-menu screen on top of the same backend.
pub(super) async fn select_and_act<S: nmbl_init::sys::ops::SysOps>(
    ops: &mut S,
    config: &Config,
    console: &mut dyn Console,
    key_injections: &[nmbl_init::activation::KeyInjection],
    session: &SessionInteraction,
    skip_selector: &SkipSelector,
    driver_images: &nmbl_init::imageload::DriverImagesHandle,
) -> Result<TerminalAction> {
    nmbl_info!("phase 4: scan generations");
    let generations = {
        let mut reporter = BootReporter::new(console, "phase 4: scan generations");
        scan_generations(config, &mut reporter)?
        // reporter drops here, releasing the &mut console borrow.
    };

    // Skip-selector fast path. Set only when the operator unlocked LUKS
    // with the "Select NixOS Generation" checkbox left UNCHECKED: boot the
    // same default generation the selector's timeout would pick, without
    // rendering the picker or running its countdown. `false` (non-LUKS
    // boots, or a CHECKED submit) falls through to the normal selector.
    #[cfg(feature = "stateful")]
    let decision = if skip_selector.get() {
        nmbl_info!("phase 5: selector skipped (checkbox off) — booting default generation");
        default_boot_decision(config, &generations)?
    } else {
        // Stateful rollback gate. When the operator opted into stateful
        // storage AND state.bin is readable, `select_with_stateful`
        // decides whether to honour the TUI countdown, force-pick a
        // known-good generation, or surface an Exhausted rescue
        // condition. Otherwise it collapses to the legacy selector.
        select_with_stateful(config, &generations, console, session).await?
    };
    #[cfg(not(feature = "stateful"))]
    let decision = if skip_selector.get() {
        nmbl_info!("phase 5: selector skipped (checkbox off) — booting default generation");
        default_boot_decision(config, &generations)?
    } else {
        nmbl_info!("phase 5: TUI generation selector");
        run_selector(config, &generations, console, session).await?
    };

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
                    context: "decision dispatch".to_string(),
                });
            };
            kexec_into(
                ops,
                config,
                target,
                cmdline_override.as_deref(),
                key_injections,
                driver_images,
            )
        }
        Decision::Shell => Err(NmblError::Io {
            source: std::io::Error::other("operator chose emergency shell"),
            context: "TUI selector".to_string(),
        }),
        Decision::Reboot => {
            nmbl_info!("operator chose reboot");
            Ok(TerminalAction::Reboot)
        }
    }
}

/// Keystone: a single self-contained async fn that IS one local
/// interactive TUI session. It owns the console for its lifetime, runs
/// the post-console phases (2b modules, 3 activations + passphrase
/// prompt, 3b mount), the generation selector (phase 4/5), and on any
/// failure drives the re-entrant emergency session through the same
/// backend. It holds no globals and takes all state by parameter, so a
/// later phase can `spawn_local` the same shape per connection. It is
/// `block_on`'d once for the local console here.
pub(super) async fn run_tui_session<S: nmbl_init::sys::ops::SysOps>(
    ops: &mut S,
    config: &mut Config,
    console: Box<dyn Console>,
    session: &SessionInteraction,
    sender: &nmbl_init::sys::poller::LocalSender,
    driver_images: &mut nmbl_init::imageload::DriverImagesHandle,
) -> TerminalAction {
    // Wrap the live boot console in the central interaction-latch layer
    // for the whole session. Every consumer below — the early-boot
    // reporter (phases 2b/3/3b), the generation selector, and the
    // emergency menu — polls input through this one wrapper, so the first
    // keypress anywhere (including the early boot-log window) sets the
    // shared latch and emits `UserHasInteracted`. Boxed as
    // `Box<dyn Console>` so it stands in transparently everywhere the
    // bare console used to, including the by-value hand-off into
    // `drop_to_emergency`; its Drop drops the real backend, preserving
    // the VT-restore-before-reboot ordering.
    let mut console: Box<dyn Console> = Box::new(LatchingConsole::new(console, session.clone()));
    // Shared "skip the generation selector" latch for this session. The
    // passphrase modal sets it from its checkbox; the post-phase selector
    // dispatch reads it. Default `false` (= show the selector) means a
    // boot with no passphrase prompt is completely unaffected.
    let skip_selector = SkipSelector::new();
    // Genuine passphrase supplier: pops the live ratatui modal on the
    // boot console. Built here (not inside `run_phases_post_console`) so
    // the dry-run path can substitute a scripted supplier that never
    // touches the console — see `validate_initrm::scenarios`.
    let mut supplier = TuiPasswordSupplier::new(config, session, &skip_selector);
    // Phases 2b/3/3b run here too: their syscalls (modules, cryptsetup,
    // mount) are plain synchronous calls inside this async fn, and the
    // passphrase prompt / wrong-password modal `.await` the same
    // console — no nested runtime anywhere. The phases run through the
    // `ops` seam (so a dry-run can no-op their side effects) and still carry
    // the security params (priority gate hooks + staged-boot apply).
    let outcome = match run_phases_post_console(
        ops,
        config,
        &mut *console,
        &mut supplier,
        session,
        &skip_selector,
        sender,
        driver_images,
    )
    .await
    {
        // The post-console phases may have appended staged-rerun driver images
        // to `driver_images` (#33); reborrow it as shared for the measure.
        Ok(injections) => {
            select_and_act(
                ops,
                config,
                &mut *console,
                &injections,
                session,
                &skip_selector,
                driver_images,
            )
            .await
        }
        Err(err) => Err(err),
    };
    dispatch_session_outcome(outcome, console, config, session, sender).await
}

/// Map the [`run_tui_session`] phase outcome to a [`TerminalAction`],
/// owning the session `console` so each arm drops it (restoring the VT)
/// before the no-return syscall fires in `execute_terminal_action`.
/// Extracted from `run_tui_session` to keep that fn within the size
/// budget; behaviour is identical to the inline `match` it replaces.
async fn dispatch_session_outcome(
    outcome: Result<TerminalAction>,
    mut console: Box<dyn Console>,
    config: &mut Config,
    session: &SessionInteraction,
    sender: &nmbl_init::sys::poller::LocalSender,
) -> TerminalAction {
    match outcome {
        Ok(action) => {
            // `console` falls out of scope on return, running
            // SplashConsole/TtyConsole Drop (KD_TEXT restore, termios
            // reset) before the no-return syscall fires in main.
            action
        }
        // Wrong-password modal Reboot path: the operator already picked
        // [Reboot]; routing through the emergency menu would just ask
        // them again. Short-circuit straight to Reboot, dropping
        // `console` so its Drop restores the VT before reboot(2) fires.
        Err(NmblError::OperatorChoseReboot { .. }) => {
            drop(console);
            TerminalAction::Reboot
        }
        // Policy refuse (R-1/R-13): an untrusted image / failed gate
        // surfaced `PolicyRefused`. This is the ONE shared refuse-render
        // entry — NEVER the shell-offering emergency menu (FIX-35). The
        // security teardown (cap → close → sentinel → relock) and the
        // non-interactive countdown both happen inside `run_refuse_screen`,
        // which returns the type-gated `RebootIntoRescue` terminus. The
        // console drops on return, restoring the VT before the reboot
        // syscall fires in `execute_terminal_action`.
        Err(NmblError::PolicyRefused { cause }) => {
            let action =
                nmbl_init::policy::run_refuse_screen(config, &mut *console, *cause, sender).await;
            drop(console);
            action
        }
        Err(err) => {
            // Hand the live boot console down to the emergency screen so
            // the operator keeps the same backend (splash or tty) they
            // saw during phase progress — no DRM/tty re-grab, no flicker.
            //
            // `NmblError::WrongPasswordShellExited` deliberately falls
            // through this arm so the standard emergency menu surfaces —
            // its [Retry boot from config] re-runs phase 3 and re-prompts
            // for the passphrase, which is what the operator wants after
            // a shell detour.
            drop_to_emergency(console, config, err, session, sender).await
        }
    }
}
