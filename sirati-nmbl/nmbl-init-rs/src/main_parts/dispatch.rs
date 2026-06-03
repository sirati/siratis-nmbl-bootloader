use std::ffi::CString;
use std::path::Path;

use nix::sys::reboot::{RebootMode, reboot};
use nix::unistd::execve;

use nmbl_init::boot::kexec_into;
use nmbl_init::config::Config;
use nmbl_init::error::{NmblError, Result, format_chain};
use nmbl_init::generations::scan_generations;
use nmbl_init::shell::{drop_to_emergency, print_banner, print_halt_banner};
use nmbl_init::terminal::{TerminalAction, redirect_stdio_for_execve};
use nmbl_init::ui::console::{Console, LatchingConsole};
#[cfg(not(feature = "stateful"))]
use nmbl_init::ui::run_selector;
use nmbl_init::ui::{
    BootReporter, Decision, SessionInteraction, SkipSelector, TuiPasswordSupplier,
};
use nmbl_init::{log, nmbl_info, nmbl_warn};

use super::phases::run_phases_post_console;
#[cfg(feature = "stateful")]
use super::stateful::{select_default_with_stateful, select_with_stateful};

use log::NMBL_LOG_PATH;

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

/// Dispatch the final [`TerminalAction`] produced by the inner
/// layers. Single point of `execve(2)` / `reboot(2)` / `reboot
/// (RB_KEXEC)` in the entire crate — by the time control reaches
/// here every `Drop` has run via normal stack unwinding, so the
/// freshly-execve'd shell or freshly-kexec'd kernel sees a clean VT.
///
/// All four variants diverge on success; on failure they fall through
/// to [`halt_final`] which performs a final `reboot(RB_HALT_SYSTEM)`
/// (or `libc::_exit` if the kernel refuses).
#[allow(
    clippy::needless_pass_by_value,
    reason = "TerminalAction is consumed exactly once at the top of main; \
              taking by value makes the move explicit"
)]
pub(super) fn execute_terminal_action(action: TerminalAction) -> ! {
    // Persist the byte-ring transcript before the no-return syscall.
    // The byte ring lives in RAM only; once we kexec / reboot / execve
    // it is gone. Disk-flushing here means the operator's emergency
    // shell (Execve path), and the kexec-staging step in `kexec_into`
    // (Kexec path), both have a fresh on-disk snapshot to work with.
    // Failures must not block the terminal action — a missing log is
    // strictly less bad than failing to reboot a wedged system.
    let log_path = Path::new(NMBL_LOG_PATH);
    if let Some(parent) = log_path.parent() {
        // EEXIST is the expected case after the first call; any other
        // error gets surfaced by the flush_to attempt below.
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(err) = log::flush_to(log_path) {
        nmbl_warn!("failed to flush log to {}: {err}", log_path.display());
    }

    match action {
        TerminalAction::Reboot => {
            eprintln!("[nmbl] operator (or timeout) chose reboot");
            let _ = reboot(RebootMode::RB_AUTOBOOT);
            halt_final("reboot(RB_AUTOBOOT) returned; halting")
        }
        TerminalAction::HaltWithBanner { cause } => {
            print_halt_banner(&cause);
            halt_final("halt-with-banner")
        }
        TerminalAction::Execve {
            path,
            argv,
            env,
            banner,
            rescue_handoff,
        } => {
            // SEAL BACKSTOP (G10): the LAST line of defense before the
            // execve syscall hands PID 1 to a shell. An `Execve` action is
            // only ever produced AFTER a G-site seal (G4 `rescue::dispatch`
            // is the authoritative, `requireTpm`-aware seal), so this hits
            // the idempotent latch and returns instantly. If — by some
            // future refactor — an unsealed `Execve` reaches here, this
            // caps fail-closed (`require_tpm=false`: degrade-open on no-TPM,
            // but a present-but-uncappable TPM still halts). `dispatch_execve`
            // REQUIRES the witness by type, so it cannot run without a seal.
            match nmbl_init::policy::seal_secrets_blocking(false) {
                Ok(sealed) => dispatch_execve(sealed, path, argv, env, banner, rescue_handoff),
                Err(seal_err) => {
                    print_halt_banner(&seal_err.into_cause());
                    halt_final("seal-on-execve failed; halting")
                }
            }
        }
        TerminalAction::RebootIntoRescue { cause, sealed } => {
            // The untrusted-image / policy refuse terminus (R-1/R-13). By
            // the time we reach here `relock_and_refuse` has already capped
            // the lock PCR, closed every TPM-unsealed mapper, relocked LUKS,
            // and written the rescue sentinel, and the non-interactive
            // refuse countdown has run to its Enter/timeout. The `Sealed`
            // witness rode along inside the value as the type-level proof
            // that the seal happened before this terminus was built; drop it
            // here — its job (gating construction) is done.
            let _: nmbl_init::policy::Sealed = sealed;
            print_halt_banner(&cause);
            eprintln!("[nmbl] policy refuse: rebooting into rescue (sentinel set, TPM locked)");
            let _ = reboot(RebootMode::RB_AUTOBOOT);
            halt_final("reboot(RB_AUTOBOOT) returned after refuse; halting")
        }
        TerminalAction::Kexec => {
            nmbl_info!("kexec: handing off to new kernel");
            // sys::kexec::execute returns Result<Infallible>; either
            // branch surfaces an error we cannot recover from at this
            // point (the image was already loaded and mounts were
            // detached), so fall through to halt_final.
            match nmbl_init::sys::kexec::execute() {
                Ok(infallible) => match infallible {},
                Err(err) => {
                    eprintln!(
                        "[nmbl] kexec execute returned: {}",
                        format_chain(&err as &dyn std::error::Error)
                    );
                    halt_final("kexec returned; halting")
                }
            }
        }
    }
}

/// Execve arm of [`execute_terminal_action`]. Extracted to keep the
/// parent match arms short (the Execve arm alone carried 35 source
/// lines of redirect + safety comment + execve call).
fn dispatch_execve(
    sealed: nmbl_init::policy::Sealed,
    path: std::ffi::CString,
    argv: Vec<std::ffi::CString>,
    env: Vec<std::ffi::CString>,
    banner: Option<nmbl_init::terminal::EmergencyBanner>,
    rescue_handoff: bool,
) -> ! {
    // The `Sealed` witness proves the lock PCR was capped and every
    // TPM-unsealed mapper closed before this single PID1 execve waist
    // (re-audit C-1). Required by type so the execve cannot run unsealed.
    let _sealed = sealed;
    if let Some(b) = banner {
        print_banner(&b);
    }
    // Re-open /dev/console and dup2 it onto 0/1/2 so the
    // freshly-execve'd shell renders on the operator's primary
    // console (framebuffer for head, ttyS0 for serial). Every
    // boot-console `Drop` has already fired by now via normal
    // stack unwinding, so the fds we just opened are the ones
    // the shell will inherit.
    //
    // On the rescue handoff this is best-effort: the rescue
    // root's /dev may not be fully populated (the full-system
    // `/init` mounts devtmpfs itself as its first step), so a
    // failed redirect must NOT halt — the entrypoint manages
    // its own console (`exec bash < /dev/console`) and halting
    // here would strand the operator. We log and execve anyway
    // with the inherited fds. For a non-rescue execve a redirect
    // failure stays fatal: an execve into invisibility is worse
    // than halting with a banner.
    if let Err(err) = redirect_stdio_for_execve() {
        eprintln!(
            "[nmbl] cannot redirect stdio before execve: {}",
            format_chain(&err as &dyn std::error::Error)
        );
        if rescue_handoff {
            eprintln!("[nmbl] rescue: stdio redirect unavailable, proceeding with inherited fds");
        } else {
            halt_final("stdio redirect failed; halting")
        }
    }
    let argv_refs: Vec<&CString> = argv.iter().collect();
    let env_refs: Vec<&CString> = env.iter().collect();
    // execve safety: single PID1 handoff point — every console/DRM Drop has run via stack unwinding, so the framebuffer/tty is back in the state the target program expects.
    let _ = execve(&path, &argv_refs, &env_refs);
    halt_final("execve returned; halting")
}

/// Print a one-line final-fallback message and halt. Diverges via
/// `reboot(RB_HALT_SYSTEM)` on success or `libc::_exit(1)` if the
/// kernel refuses (lacking CAP_SYS_BOOT in a sandbox, not PID 1, …).
pub(super) fn halt_final(reason: &str) -> ! {
    eprintln!("[nmbl] {reason}");
    let _ = reboot(RebootMode::RB_HALT_SYSTEM);
    // SAFETY: libc::_exit is async-signal-safe and unconditionally
    // terminates the process; no crate wraps it (rustix issue #844).
    unsafe { libc::_exit(1) };
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
