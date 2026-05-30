//! Boot orchestration that runs inside the interactive
//! [`LocalRuntime`]. Extracted from `main.rs` to keep that file under the
//! file-size limit: this module owns [`run_boot_inside_runtime`], the
//! [`BootOutcome`] enum it returns, and the two sub-flows it can branch
//! into ([`run_force_rescue`] and [`run_key_echo_diagnostic`]).

use std::path::Path;

use nmbl_init::config::Config;
use nmbl_init::error::{NmblError, format_chain};
use nmbl_init::modules::load_explicit_modules;
use nmbl_init::panic::install_panic_hook;
use nmbl_init::rescue::{self};
use nmbl_init::sys::ops::RealSys;
use nmbl_init::terminal::TerminalAction;
use nmbl_init::ui::console::{Console, NoopConsole, open_console};
use nmbl_init::ui::key_echo::run_key_echo_loop;
use nmbl_init::ui::{BootReporter, SessionInteraction};
use nmbl_init::{log, nmbl_info, nmbl_warn};

use super::dispatch::run_tui_session;
#[cfg(feature = "stateful")]
use super::phases::mount_state_twin;
use super::phases::{run_bootstrap_phase, run_phase_2a};
use super::{cmdline_has_key_echo_flag, should_force_external_rescue};

/// Force-external-rescue sub-flow. Loads the explicit module set
/// (carries the auto-added `loop`/`squashfs`/nicDrivers for
/// `rescue.mode == external`) then calls `rescue::dispatch`. Extracted
/// from `run_inner` to keep that fn under 100 lines.
///
/// `config` is taken by value so `Err` can carry it back to
/// `open_console_and_drop_to_emergency` without cloning when
/// `rescue::dispatch` succeeds (the `Ok` arm moves config into the
/// action).
pub(crate) fn run_force_rescue(
    config: Config,
    noop: &mut NoopConsole,
) -> std::result::Result<TerminalAction, Box<(NmblError, Config)>> {
    nmbl_info!("force_on_boot: entering external rescue");
    // The rescue-required kernel modules (`loop`, `squashfs`, the
    // rescue `nicDrivers` and `af_packet`) are auto-added to
    // `config.kernel_modules.explicit` for `rescue.mode == external`
    // (see lib/config.nix `rescueDiskModules`/`rescueNicModules`),
    // which is normally loaded in phase 2b. The force path
    // short-circuits before phase 2b, so without loading them here
    // `allocate_loop_device` would fail with ENOENT on
    // `/dev/loop-control` (the `loop` module was never inserted, so
    // devtmpfs never created the node) and `/init`'s DHCP would have
    // no NIC driver after switch_root. Load the explicit set now —
    // before `rescue::dispatch` — using the pre-console NoopConsole
    // reporter exactly as phase 2a does.
    {
        // Pre-runtime: no poller yet, so module loading (sync `ModuleOps`)
        // goes through a sender-less `RealSys`.
        let mut ops = RealSys::sync_only();
        let mut reporter = BootReporter::new(noop, "force_on_boot: load rescue kernel modules");
        if let Err(err) = load_explicit_modules(&mut ops, &config, &mut reporter) {
            nmbl_warn!("force_on_boot: loading rescue modules failed: {err}");
            return Err(Box::new((err, config)));
        }
    }
    nmbl_info!("force_on_boot: loaded rescue modules");
    let cause = NmblError::Rescue {
        stage: "force-on-boot",
        source: Box::new(NmblError::Io {
            source: std::io::Error::other(
                "rescue.force_on_boot requested an unconditional external rescue boot",
            ),
            context: "force-on-boot rescue trigger".to_string(),
        }),
    };
    let console: Box<dyn Console> = Box::new(NoopConsole::new());
    match rescue::dispatch(&config, console, cause) {
        Ok(action) => Ok(action),
        Err(err) => {
            nmbl_warn!(
                "force_on_boot: external rescue dispatch failed: {}",
                format_chain(&err as &dyn std::error::Error)
            );
            Err(Box::new((err, config)))
        }
    }
}

/// Key-echo diagnostic sub-flow. Runs the key-echo loop then drops to
/// emergency. Extracted from `run_boot_inside_runtime` to keep that fn
/// under 100 lines.
///
/// Async and driven by the caller's already-running interactive
/// [`LocalRuntime`] (the `sender` flows in from there) — it no longer
/// builds its own runtime now that the runtime starts right after the
/// pseudo-fs mount. The key-echo diagnostic owns its own App, so a fresh
/// `session` is correct.
async fn run_key_echo_diagnostic(
    config: Config,
    console: Box<dyn Console>,
    sender: &nmbl_init::sys::poller::LocalSender,
) -> std::result::Result<TerminalAction, Box<(NmblError, Config)>> {
    nmbl_info!("nmbl.key_echo=1 in cmdline: entering key-echo diagnostic screen");
    let err = NmblError::Io {
        source: std::io::Error::other("key-echo diagnostic mode terminated"),
        context: "key-echo".to_string(),
    };
    let session = SessionInteraction::new();
    // Wrap the console in the central interaction-latch layer so a key
    // pressed during the key-echo loop carries operator-presence into the
    // follow-on emergency session (and cancels its auto-reboot countdown),
    // matching every other interactive session.
    let mut console: Box<dyn Console> = Box::new(nmbl_init::ui::console::LatchingConsole::new(
        console,
        session.clone(),
    ));
    if let Err(e) = run_key_echo_loop(&mut *console).await {
        nmbl_warn!(
            "key-echo loop error: {}",
            format_chain(&e as &dyn std::error::Error)
        );
    }
    // Hand the live console down to drop_to_emergency so the emergency UI
    // paints through the same backend.
    Ok(nmbl_init::shell::drop_to_emergency(console, &config, err, &session, sender).await)
}

/// Result of [`run_boot_inside_runtime`]. Either the runtime drove the
/// boot to a final outcome, or it hit the external force-rescue gate and
/// hands the (bootstrap-loaded) config back so `run_inner` can run
/// `rescue::dispatch` AFTER the runtime exits — `dispatch` builds its own
/// runtime and cannot nest inside this one.
pub(crate) enum BootOutcome {
    // `TerminalAction` (the Ok arm) is the large payload here; box the
    // whole `Done` result so this variant stays pointer-sized next to the
    // tiny `ForceRescue` (clippy `large_enum_variant`).
    Done(Box<std::result::Result<TerminalAction, Box<(NmblError, Config)>>>),
    ForceRescue(Box<Config>),
}

/// The post-phase-1 boot flow that runs inside the interactive
/// [`LocalRuntime`]: bootstrap (async blkid reap), the force-rescue and
/// stateful gates, phase 2a (atomic `init_module` calls — synchronous
/// within the async region), console bring-up, and the key-echo or main
/// TUI session. The `sender` threads the poller down so every subprocess
/// reap stays non-blocking.
///
/// `config` is taken by value because bootstrap mode replaces it with
/// the real config read from `/boot`. The `noop` reporter sink is the
/// pre-console sentinel reused across the synchronous phases. The
/// force-rescue arm returns [`BootOutcome::ForceRescue`] instead of
/// dispatching inline (see [`BootOutcome`]).
pub(crate) async fn run_boot_inside_runtime(
    mut config: Config,
    bootstrap_mode: bool,
    bootstrap_path: &Path,
    noop: &mut NoopConsole,
    sender: nmbl_init::sys::poller::LocalSender,
) -> BootOutcome {
    // The genuine in-runtime system-ops impl: its async forwards reap
    // subprocesses through the poller `sender`. Threaded `&mut` down the
    // spine so every side-effecting call dispatches through it.
    let mut ops = RealSys::new(&sender);
    if bootstrap_mode {
        match run_bootstrap_phase(&mut ops, bootstrap_path).await {
            Ok(loaded) => {
                config = loaded;
                install_panic_hook(&config.general.panic_report_dir);
                log::init(config.general.verbosity);
            }
            Err(err) => return BootOutcome::Done(Box::new(Err(Box::new((err, config))))),
        }
    }
    if should_force_external_rescue(&config) {
        return BootOutcome::ForceRescue(Box::new(config));
    }
    #[cfg(feature = "stateful")]
    if bootstrap_mode && let Err(err) = mount_state_twin(&mut ops, &mut config, bootstrap_path) {
        return BootOutcome::Done(Box::new(Err(Box::new((err, config)))));
    }
    {
        let mut reporter = BootReporter::new(noop, "phase 2a: load early kernel modules");
        if let Err(err) = run_phase_2a(&mut ops, &config, &mut reporter) {
            nmbl_warn!("phase 2a (early modules) failed: {err}");
            return BootOutcome::Done(Box::new(Err(Box::new((err, config)))));
        }
    }
    let console: Box<dyn Console> = match open_console(&config, false) {
        Ok(c) => c,
        Err(err) => {
            nmbl_warn!("boot console bring-up failed: {err}");
            return BootOutcome::Done(Box::new(Err(Box::new((err, config)))));
        }
    };
    if cmdline_has_key_echo_flag() {
        return BootOutcome::Done(Box::new(
            run_key_echo_diagnostic(config, console, &sender).await,
        ));
    }
    let session = SessionInteraction::new();
    BootOutcome::Done(Box::new(Ok(run_tui_session(
        &mut ops, &config, console, &session, &sender,
    )
    .await)))
}
