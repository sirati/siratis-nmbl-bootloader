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
use nmbl_init::panic::install_panic_hook;
use nmbl_init::rescue::RescueMode;
use nmbl_init::shell::open_console_and_drop_to_emergency;
use nmbl_init::terminal::TerminalAction;
use nmbl_init::ui::BootReporter;
use nmbl_init::ui::console::NoopConsole;
use nmbl_init::{log, nmbl_info, nmbl_warn};

#[path = "main_parts/args.rs"]
mod args;
#[path = "main_parts/boot_runtime.rs"]
mod boot_runtime;
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
use boot_runtime::{BootOutcome, run_boot_inside_runtime, run_force_rescue};
use dispatch::execute_terminal_action;
use early_exit::handle_early_exit_modes;
use phases::run_phase_1;

const BOOTSTRAP_CONFIG_PATH: &str = "/etc/nmbl/bootstrap.toml";

/// Earliest startup, before any work that can fail. Two jobs, in order:
///
/// 1. **As PID 1, mount `/dev` (devtmpfs) then `/proc` immediately and
///    blockingly.** The initramfs ships an empty `/dev` with no static
///    `/dev/console`, so until devtmpfs is mounted PID 1 has no console
///    and any early panic message is invisible; and the panic hook's
///    `execve("/proc/self/exe")` recovery needs `/proc`. Mounting these
///    up front (rather than in Phase 1, which only runs after config
///    load) makes the earliest failure observable and recoverable.
///    Best-effort: a mount that returns `EBUSY` (re-exec already has it
///    mounted) is fine, and any other error must not itself abort —
///    Phase 1 re-mounts idempotently with full reporting later.
///
/// 2. **Install the panic hook right away**, against the default report
///    dir, so a panic in `parse_args` / `Config::load` (which run before
///    the config-driven `install_panic_hook` below) still routes through
///    the `execve`-into-recovery path instead of unwinding past `main`
///    or aborting. `install_panic_hook` is idempotent/replace-semantics,
///    so the later call with the operator's real report dir just updates
///    the target directory.
fn early_init() {
    // Only PID 1 is responsible for the pseudo-filesystems; a non-PID-1
    // invocation (installer, systemd unit, rescue client) must not touch
    // the host's mounts.
    if nix::unistd::getpid().as_raw() == 1 {
        // /dev first so /dev/console exists for output, then /proc for
        // the panic hook's self-re-exec. EBUSY = already mounted (re-exec
        // path) is success; any other error is swallowed so early_init
        // itself can never be the thing that kills PID 1 — Phase 1 will
        // retry with diagnostics.
        for (target, fstype, options) in [
            ("/dev", "devtmpfs", "mode=755,nosuid"),
            ("/proc", "proc", "nosuid,noexec,nodev"),
        ] {
            let path = Path::new(target);
            let _ = std::fs::create_dir_all(path);
            match nmbl_init::sys::mount::mount_fs(None, path, fstype, options) {
                Ok(()) => {}
                Err(NmblError::Mount {
                    source: nix::errno::Errno::EBUSY,
                    ..
                }) => {}
                Err(_) => { /* swallow: Phase 1 re-mounts with reporting */ }
            }
        }
    }

    // Hook installed before parse_args / Config::load so their panics are
    // caught. Uses the panic module's default report dir; re-installed
    // with the operator's configured dir once the config is loaded.
    install_panic_hook(Path::new(nmbl_init::panic::DEFAULT_PANIC_REPORT_DIR));
}

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
    config: Config,
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
    // Phase 1 (mount /proc,/sys,/dev) is atomic mount(2) syscalls with
    // no subprocess reap and is needed before much else, so it stays a
    // synchronous pre-runtime call. The runtime is then built right
    // after it: everything from the bootstrap blkid sweep onward — whose
    // child reaps must go through the poller's non-blocking `waitpid` —
    // runs inside the one interactive runtime so no wait ever blocks the
    // single runtime thread.
    {
        let mut reporter = BootReporter::new(&mut noop, "phase 1: mount pseudo-filesystems");
        if let Err(err) = run_phase_1(&mut reporter) {
            return Err(Box::new((err, config)));
        }
    }
    let rt_result = nmbl_init::ui::block_on_tui_with_poller(move |sender| async move {
        run_boot_inside_runtime(config, bootstrap_mode, bootstrap_path, &mut noop, sender).await
    });
    match rt_result {
        // The runtime drove the boot to a terminal action (or a
        // recoverable error already paired with its config).
        Ok(BootOutcome::Done(inner)) => *inner,
        // External force-rescue: `rescue::dispatch` builds its OWN
        // `block_on_tui_with_poller` runtime for the chrooted child, so
        // it must run AFTER this runtime has torn down — nesting two
        // `block_on`s on one thread would panic. The bootstrap blkid
        // reap that precedes the gate already ran async inside the
        // runtime above; only the dispatch itself is deferred out here.
        Ok(BootOutcome::ForceRescue(config)) => {
            let mut noop = NoopConsole::new();
            run_force_rescue(*config, &mut noop)
        }
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
    // Before anything that can fail: as PID 1 mount /dev + /proc, and
    // install the panic hook. See [`early_init`]. Compiled-in for every
    // build; the mocking/debug-tui path below is non-PID-1 so it only
    // gets the (idempotent) hook install, not the mounts.
    early_init();

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
