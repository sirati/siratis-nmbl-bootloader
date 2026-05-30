//! Top-level orchestration (PLAN.md §11 entrypoint).
//!
//! Two entry modes:
//! * **Normal boot.** Install the panic hook, load config, run phases
//!   1→6, dispatch the operator's [`Decision`]. Any phase `Err` routes
//!   to [`shell::drop_to_emergency`].
//! * **Panic recovery.** Triggered by `--errored=<path>` (set by the
//!   panic hook's `execve` of `/proc/self/exe`). Reads the report,
//!   loads config best-effort, drops straight to the emergency shell
//!   with [`NmblError::Panicked`].
//!
//! Every no-return syscall — `execve(2)`, `reboot(RB_AUTOBOOT)`,
//! `reboot(RB_HALT_SYSTEM)`, `reboot(RB_KEXEC)` — funnels through
//! [`execute_terminal_action`]. Inner layers return a
//! [`TerminalAction`] value; control unwinds back to `main`, every
//! stack-allocated `Drop` runs (Console restores KD_TEXT, RawModeGuard
//! restores termios, glyph caches free, …), and only then does
//! `execute_terminal_action` fire the syscall.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use nmbl_init::config::Config;
use nmbl_init::error::{NmblError, format_chain};
use nmbl_init::modules::load_explicit_modules;
use nmbl_init::panic::install_panic_hook;
use nmbl_init::rescue::{self, RescueMode};
use nmbl_init::shell::open_console_and_drop_to_emergency;
use nmbl_init::terminal::TerminalAction;
use nmbl_init::ui::console::{Console, NoopConsole, open_console};
use nmbl_init::ui::key_echo::run_key_echo_loop;
use nmbl_init::ui::{BootReporter, SessionInteraction};
use nmbl_init::{log, nmbl_info, nmbl_warn};

#[path = "main_parts/args.rs"]
mod args;
#[path = "main_parts/dispatch.rs"]
mod dispatch;
#[path = "main_parts/early_exit.rs"]
mod early_exit;
#[path = "main_parts/phases.rs"]
mod phases;
#[cfg(feature = "stateful")]
#[path = "main_parts/stateful.rs"]
mod stateful;

use args::{Args, parse_args};
use dispatch::{execute_terminal_action, run_tui_session};
use early_exit::handle_early_exit_modes;
#[cfg(feature = "stateful")]
use phases::mount_state_twin;
use phases::{run_bootstrap_phase, run_phase_1, run_phase_2a};

const BOOTSTRAP_CONFIG_PATH: &str = "/etc/nmbl/bootstrap.toml";

// Tmpfs path the byte-ring is flushed to right before every terminal
// action — defined once in `log::NMBL_LOG_PATH`. The parent dir is
// `mkdir -p`'d on every call; EEXIST is benign, anything else means
// tmpfs is broken and we surface a warning but still proceed with the
// terminal action.

/// Kernel cmdline token that opts into the key-echo diagnostic screen.
/// Must appear as a whitespace-delimited token (e.g.
/// `... loglevel=7 nmbl.key_echo=1`); we don't accept arbitrary `=...`
/// values beyond `1` to keep the gate cheap and unambiguous.
const KEY_ECHO_CMDLINE_TOKEN: &str = "nmbl.key_echo=1";

/// `true` if `/proc/cmdline` contains [`KEY_ECHO_CMDLINE_TOKEN`] as a
/// whitespace-delimited token. False on every read error so a missing
/// or unreadable `/proc/cmdline` (e.g. mid-bootstrap before phase 1)
/// can never silently force the diagnostic screen on a production boot.
fn cmdline_has_key_echo_flag() -> bool {
    let Ok(cmdline) = std::fs::read_to_string("/proc/cmdline") else {
        return false;
    };
    cmdline
        .split_whitespace()
        .any(|tok| tok == KEY_ECHO_CMDLINE_TOKEN)
}

/// Best-effort config load for the recovery path. Falls back to
/// [`Config::recovery_default`] so the emergency shell still has a
/// valid `paths.shell` to `execve`.
fn load_config_lenient(path: &std::path::Path) -> Config {
    match Config::load(path) {
        Ok(c) => c,
        Err(err) => {
            eprintln!(
                "[nmbl] recovery-mode: cannot load {}: {err}; using built-in defaults",
                path.display()
            );
            Config::recovery_default()
        }
    }
}

/// Run the panic-recovery flow. Returns a [`TerminalAction`] the
/// dispatcher in `main` performs after the call stack has unwound.
fn recover_from_panic(args: Args, report_path: PathBuf) -> (TerminalAction, Config) {
    let report = match std::fs::read_to_string(&report_path) {
        Ok(text) => text,
        Err(err) => format!(
            "(panic report at {} unreadable: {err})",
            report_path.display()
        ),
    };
    let config = load_config_lenient(&args.config_path);
    log::init(nmbl_init::log::Verbosity::Verbose);

    nmbl_warn!("panic recovery mode: report at {}", report_path.display());
    nmbl_warn!("panic report follows:\n{report}");

    let action = open_console_and_drop_to_emergency(&config, NmblError::Panicked { report_path });
    (action, config)
}

/// Whether the deterministic force-rescue trigger should fire: the
/// operator set `rescue.force_on_boot` AND the rescue mode is
/// `external`. Factored out of `run_inner` so the guard is unit-testable
/// without driving the full PID-1 boot flow. `force_on_boot` on any
/// other mode is a no-op (only the external squashfs path is a
/// no-input, deterministic rescue).
fn should_force_external_rescue(config: &Config) -> bool {
    config.rescue.force_on_boot && config.rescue.mode == RescueMode::External
}

/// Force-external-rescue sub-flow. Loads the explicit module set
/// (carries the auto-added `loop`/`squashfs`/nicDrivers for
/// `rescue.mode == external`) then calls `rescue::dispatch`. Extracted
/// from `run_inner` to keep that fn under 100 lines.
///
/// `config` is taken by value so `Err` can carry it back to
/// `open_console_and_drop_to_emergency` without cloning when
/// `rescue::dispatch` succeeds (the `Ok` arm moves config into the
/// action).
fn run_force_rescue(
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
        let mut reporter = BootReporter::new(noop, "force_on_boot: load rescue kernel modules");
        if let Err(err) = load_explicit_modules(&config, &mut reporter) {
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
/// emergency. Extracted from `run_inner` to keep that fn under 100
/// lines.
fn run_key_echo_diagnostic(
    config: Config,
    console: Box<dyn Console>,
) -> std::result::Result<TerminalAction, Box<(NmblError, Config)>> {
    nmbl_info!("nmbl.key_echo=1 in cmdline: entering key-echo diagnostic screen");
    let err = NmblError::Io {
        source: std::io::Error::other("key-echo diagnostic mode terminated"),
        context: "key-echo".to_string(),
    };
    // Cross into the async interactive phase: one LocalRuntime drives
    // both the key-echo loop and the follow-on emergency session.
    // The key-echo diagnostic owns its own App, so a fresh session is
    // correct. A runtime-build failure routes to a plain Reboot.
    let session = SessionInteraction::new();
    // Wrap the console in the central interaction-latch layer so a key
    // pressed during the key-echo loop carries operator-presence into the
    // follow-on emergency session (and cancels its auto-reboot countdown),
    // matching every other interactive session.
    let mut console: Box<dyn Console> = Box::new(nmbl_init::ui::console::LatchingConsole::new(
        console,
        session.clone(),
    ));
    let action = nmbl_init::ui::block_on_tui(async {
        if let Err(e) = run_key_echo_loop(&mut *console).await {
            nmbl_warn!(
                "key-echo loop error: {}",
                format_chain(&e as &dyn std::error::Error)
            );
        }
        // Hand the live console down to drop_to_emergency so the
        // emergency UI paints through the same backend.
        nmbl_init::shell::drop_to_emergency(console, &config, err, &session).await
    });
    match action {
        Ok(a) => Ok(a),
        Err(rt_err) => {
            nmbl_warn!(
                "key-echo runtime build failed: {}",
                format_chain(&rt_err as &dyn std::error::Error)
            );
            Ok(TerminalAction::Reboot)
        }
    }
}

/// Helper: run the boot phases and return the resulting
/// [`TerminalAction`]. On phase failure returns
/// `Err(Box::new((err, config)))` so the caller can hand the live
/// config to `open_console_and_drop_to_emergency`. The error variant
/// is boxed because `(NmblError, Config)` is large enough to trip
/// clippy's `result_large_err` lint on every recoverable return path.
///
/// `config` is taken by value because it is mutated in bootstrap
/// mode (Phase 0.5 replaces it with the real config from `/boot`).
fn run_inner(
    mut config: Config,
    load_err: Option<NmblError>,
    bootstrap_probe: std::io::Result<bool>,
    bootstrap_path: &Path,
    _args: &Args,
) -> std::result::Result<TerminalAction, Box<(NmblError, Config)>> {
    if let Some(err) = load_err {
        return Err(Box::new((err, config)));
    }
    if let Err(probe_err) = bootstrap_probe {
        let err = NmblError::Bootstrap {
            stage: "probe",
            source: Box::new(NmblError::Io {
                source: probe_err,
                context: format!("probing {}", bootstrap_path.display()),
            }),
        };
        return Err(Box::new((err, config)));
    }
    let bootstrap_mode = bootstrap_path.try_exists().unwrap_or(false);
    let mut noop = NoopConsole::new();
    {
        let mut reporter = BootReporter::new(&mut noop, "phase 1: mount pseudo-filesystems");
        if let Err(err) = run_phase_1(&mut reporter) {
            return Err(Box::new((err, config)));
        }
    }
    if bootstrap_mode {
        match run_bootstrap_phase(bootstrap_path) {
            Ok(loaded) => {
                config = loaded;
                install_panic_hook(&config.general.panic_report_dir);
                log::init(config.general.verbosity);
            }
            Err(err) => return Err(Box::new((err, config))),
        }
    }
    if should_force_external_rescue(&config) {
        return run_force_rescue(config, &mut noop);
    }
    #[cfg(feature = "stateful")]
    if bootstrap_mode && let Err(err) = mount_state_twin(&mut config, bootstrap_path) {
        return Err(Box::new((err, config)));
    }
    {
        let mut reporter = BootReporter::new(&mut noop, "phase 2a: load early kernel modules");
        if let Err(err) = run_phase_2a(&config, &mut reporter) {
            nmbl_warn!("phase 2a (early modules) failed: {err}");
            return Err(Box::new((err, config)));
        }
    }
    let console: Box<dyn Console> = match open_console(&config, false) {
        Ok(c) => c,
        Err(err) => {
            nmbl_warn!("boot console bring-up failed: {err}");
            return Err(Box::new((err, config)));
        }
    };
    if cmdline_has_key_echo_flag() {
        return run_key_echo_diagnostic(config, console);
    }
    let session = SessionInteraction::new();
    match nmbl_init::ui::block_on_tui(run_tui_session(&config, console, &session)) {
        Ok(action) => Ok(action),
        Err(rt_err) => {
            nmbl_warn!(
                "interactive runtime build failed: {}",
                format_chain(&rt_err as &dyn std::error::Error)
            );
            Ok(TerminalAction::Reboot)
        }
    }
}

/// The orchestrator. Returns `ExitCode` so the `--validate-config`
/// path can exit normally; every other path either reaches
/// [`execute_terminal_action`] (which diverges) or returns
/// `ExitCode::SUCCESS` after a normal `Ok(())` outcome.
fn main() -> ExitCode {
    // `--debug-tui -- <scenario> [args...]` entrypoint (feature `mocking`).
    // Runs a single modal flow on the current terminal and exits.
    // Compiled out of release builds so the production initramfs cannot
    // be tricked into the mocking flow by stray cmdline.
    #[cfg(feature = "mocking")]
    {
        let argv: Vec<String> = std::env::args().collect();
        if let Some(debug_args) = nmbl_init::mocking::parse_debug_tui_args(argv) {
            return match nmbl_init::mocking::run(debug_args) {
                Ok(()) => ExitCode::from(0),
                Err(err) => {
                    eprintln!("[nmbl] --debug-tui: {err}");
                    ExitCode::from(1)
                }
            };
        }
    }

    let args = match parse_args() {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("nmbl-init: {msg}");
            return ExitCode::from(2);
        }
    };

    // Build-time validation and installer/systemd-unit early-exit
    // dispatches; each prints its outcome and exits.
    if let Some(code) = handle_early_exit_modes(&args) {
        return code;
    }

    if let Some(report_path) = args.errored_report.clone() {
        let (action, _config) = recover_from_panic(args, report_path);
        execute_terminal_action(action);
    }

    let bootstrap_path = Path::new(BOOTSTRAP_CONFIG_PATH);
    let bootstrap_probe = bootstrap_path.try_exists();
    let bootstrap_mode = matches!(bootstrap_probe, Ok(true));

    let (config, load_err): (Config, Option<NmblError>) = if bootstrap_mode {
        (Config::recovery_default(), None)
    } else {
        match Config::load(&args.config_path) {
            Ok(c) => (c, None),
            Err(err) => (Config::recovery_default(), Some(err)),
        }
    };

    install_panic_hook(&config.general.panic_report_dir);
    log::init(config.general.verbosity);
    nmbl_info!("nmbl-init starting");

    let action = match run_inner(config, load_err, bootstrap_probe, bootstrap_path, &args) {
        Ok(action) => action,
        Err(boxed) => {
            let (err, config) = *boxed;
            open_console_and_drop_to_emergency(&config, err)
        }
    };
    execute_terminal_action(action);
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
#[path = "main_parts/main_tests.rs"]
mod tests;
