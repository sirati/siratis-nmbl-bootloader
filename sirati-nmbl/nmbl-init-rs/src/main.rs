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

use std::path::PathBuf;
use std::process::ExitCode;

use nix::sys::reboot::{RebootMode, reboot};

use nmbl_init::activation::run_all_activations;
use nmbl_init::boot::kexec_into;
use nmbl_init::config::Config;
use nmbl_init::devices::mount_system_filesystems;
use nmbl_init::error::{NmblError, Result};
use nmbl_init::generations::scan_generations;
use nmbl_init::modules::load_explicit_modules;
use nmbl_init::mount::mount_pseudo_filesystems;
use nmbl_init::panic::install_panic_hook;
use nmbl_init::shell::drop_to_emergency;
use nmbl_init::ui::console::{Console, open_console};
use nmbl_init::ui::{BootReporter, Decision, TuiPasswordSupplier, run_selector};
use nmbl_init::{log, nmbl_info, nmbl_warn};

const DEFAULT_CONFIG_PATH: &str = "/etc/nmbl/config.toml";

struct Args {
    config_path: PathBuf,
    errored_report: Option<PathBuf>,
    validate_config: Option<PathBuf>,
}

/// Hand-rolled arg parsing: clap is too big for the size budget. We
/// recognise `--config=<v>` / `--config <v>` and the same two forms
/// for `--errored` and `--validate-config`. Anything else is silently
/// ignored — PID 1 has no useful "usage" target to print to.
fn parse_args() -> Args {
    let mut config_path = PathBuf::from(DEFAULT_CONFIG_PATH);
    let mut errored_report: Option<PathBuf> = None;
    let mut validate_config: Option<PathBuf> = None;

    let mut iter = std::env::args_os().skip(1);
    while let Some(arg_os) = iter.next() {
        let arg = arg_os.to_string_lossy();
        if let Some(rest) = arg.strip_prefix("--config=") {
            config_path = PathBuf::from(rest);
        } else if arg == "--config"
            && let Some(v) = iter.next()
        {
            config_path = PathBuf::from(v);
        } else if let Some(rest) = arg.strip_prefix("--errored=") {
            errored_report = Some(PathBuf::from(rest));
        } else if arg == "--errored"
            && let Some(v) = iter.next()
        {
            errored_report = Some(PathBuf::from(v));
        } else if let Some(rest) = arg.strip_prefix("--validate-config=") {
            validate_config = Some(PathBuf::from(rest));
        } else if arg == "--validate-config"
            && let Some(v) = iter.next()
        {
            validate_config = Some(PathBuf::from(v));
        }
    }

    Args {
        config_path,
        errored_report,
        validate_config,
    }
}

/// Read the panic report file, degrading gracefully if it's gone — we
/// still want to enter the recovery flow even when the report itself
/// was lost.
fn read_panic_report(path: &std::path::Path) -> String {
    match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) => format!("(panic report at {} unreadable: {err})", path.display()),
    }
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

/// Run the panic-recovery flow. Returns `Infallible` to document the
/// non-return — [`drop_to_emergency`] either `execve`s the shell or
/// halts the system.
fn recover_from_panic(args: Args, report_path: PathBuf) -> std::convert::Infallible {
    let report = read_panic_report(&report_path);
    let config = load_config_lenient(&args.config_path);
    log::init(nmbl_init::log::Verbosity::Verbose);

    nmbl_warn!("panic recovery mode: report at {}", report_path.display());
    nmbl_warn!("panic report follows:\n{report}");

    drop_to_emergency(None, &config, NmblError::Panicked { report_path })
}

/// Execute the normal boot phases in order. Each phase that errors
/// short-circuits to the caller, which routes through the emergency
/// shell.
///
/// `console` is the live boot console (splash framebuffer or raw-mode
/// tty) brought up by the caller before phase 1. We construct a
/// short-lived [`BootReporter`] around it so every phase pushes its
/// current "what am I doing" string through the reporter and the
/// operator sees progress instead of a frozen logo. The reporter is
/// dropped on return so the caller can reuse the underlying console
/// for the generation selector.
fn run_phases(config: &Config, console: &mut dyn Console) -> Result<()> {
    let mut reporter = BootReporter::new(console, "phase 1: mount pseudo-filesystems");
    // Paint the first frame so the operator sees a populated screen
    // before any work happens — otherwise a fast phase 1 would race the
    // first kmsg push and the log panel would be empty for one frame.
    let _ = reporter.refresh_log();

    nmbl_info!("phase 1: mount pseudo-filesystems");
    mount_pseudo_filesystems(&mut reporter)?;

    nmbl_info!("phase 2: load explicit kernel modules");
    load_explicit_modules(config, &mut reporter)?;

    // The splash backend opens /dev/tty1 and calls VT_ACTIVATE itself
    // (see `splash::input::SplashInput::open`), so an explicit
    // `activate_vt1()` here is redundant in the splash code path. For
    // the tty backend `/dev/console` already points at the kernel-chosen
    // VT, so reads land correctly without forcing a VT switch.

    nmbl_info!("phase 3: storage activations");
    let mut supplier = TuiPasswordSupplier::new(config);
    run_all_activations(config, &mut reporter, Some(&mut supplier))?;

    nmbl_info!("phase 3b: mount system filesystems");
    mount_system_filesystems(config, &mut reporter)?;

    Ok(())
}

/// Run phases 4→6 (generation discovery, UI, decision dispatch). Kept
/// separate so the call sites for `drop_to_emergency` stay obvious.
///
/// Phase 4 uses a [`BootReporter`] around `console` so the operator
/// keeps seeing the boot-status screen while we walk the profiles
/// directory. The reporter is dropped before phase 5 so the bare
/// console can be handed to `run_selector`, which swaps the App over
/// to the boot-menu screen on top of the same backend.
fn select_and_act(config: &Config, console: &mut dyn Console) -> Result<()> {
    nmbl_info!("phase 4: scan generations");
    let generations = {
        let mut reporter = BootReporter::new(console, "phase 4: scan generations");
        scan_generations(config, &mut reporter)?
        // reporter drops here, releasing the &mut console borrow.
    };

    nmbl_info!("phase 5: TUI generation selector");
    let decision = run_selector(config, &generations, console)?;

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
            // kexec_into returns Result<Infallible> — on success it
            // does not return. Match against the Infallible so a
            // future signature change becomes a compile error here
            // rather than a silently-ignored return value.
            match kexec_into(config, target, cmdline_override.as_deref())? {}
        }
        Decision::Shell => Err(NmblError::Io {
            source: std::io::Error::other("operator chose emergency shell"),
            context: "TUI selector".to_string(),
        }),
        Decision::Reboot => {
            nmbl_info!("operator chose reboot");
            let _err = reboot(RebootMode::RB_AUTOBOOT);
            // reboot only returns on failure.
            Err(NmblError::Io {
                source: std::io::Error::other("reboot(RB_AUTOBOOT) returned"),
                context: "decision dispatch".to_string(),
            })
        }
    }
}

#[allow(
    unreachable_code,
    reason = "drop_to_emergency / recover_from_panic return Infallible; the empty match consuming an uninhabited type is the canonical idiom"
)]
fn main() -> ExitCode {
    let args = parse_args();

    // Build-time validation hook: load and validate the given config
    // file, print the outcome, and exit. Used by the Nix expression
    // that emits the runtime TOML so a malformed config fails
    // `nix build` instead of `nmbl-init` at PID 1 in front of the
    // operator.
    if let Some(path) = args.validate_config.as_deref() {
        return match Config::load(path) {
            Ok(_) => {
                println!("nmbl-init: config OK: {}", path.display());
                ExitCode::from(0)
            }
            Err(err) => {
                eprintln!("nmbl-init: config invalid at {}: {err}", path.display());
                ExitCode::from(1)
            }
        };
    }

    if let Some(report_path) = args.errored_report.clone() {
        // Note: the panic hook must NOT be re-installed here — a
        // second panic during recovery should crash, not loop.
        match recover_from_panic(args, report_path) {}
    }

    // Config load is the chicken-and-egg moment: if it fails we have
    // no `shell` path, no verbosity, no nothing. Fall back to the
    // recovery default and route the load error through the shell.
    let (config, load_err): (Config, Option<NmblError>) = match Config::load(&args.config_path) {
        Ok(c) => (c, None),
        Err(err) => (Config::recovery_default(), Some(err)),
    };

    // Install the panic hook now that we know where to write reports.
    // A panic during the brief window before this call would still
    // unwind through the default Rust hook, abort PID 1, and let the
    // kernel panic — the documented worst case.
    install_panic_hook(&config.general.panic_report_dir);

    log::init(config.general.verbosity);
    nmbl_info!("nmbl-init starting");

    if let Some(err) = load_err {
        // No console is open yet at this point — drop_to_emergency will
        // open a tty console itself (panic_recovery=true so it skips the
        // splash bring-up).
        match drop_to_emergency(None, &config, err) {}
    }

    // Bring the boot console up BEFORE phase 1 so the operator sees a
    // populated BootStatus screen during the first mount, the module
    // loads, storage activations, and the generation scan. The same
    // backend is reused all the way through the boot-menu selector and
    // the emergency screen on phase failure — `drop_to_emergency` takes
    // the live console handle when one exists.
    //
    // If we cannot bring up ANY console at all, log it and route the
    // bring-up error through the emergency shell; `drop_to_emergency`
    // will open a fresh tty console itself (forced past splash via
    // panic_recovery=true), so the operator still gets a screen.
    let mut console: Box<dyn Console> = match open_console(&config, false) {
        Ok(c) => c,
        Err(err) => {
            nmbl_warn!("boot console bring-up failed: {err}");
            // No console available; drop_to_emergency opens a tty
            // console itself (panic_recovery=true so it skips splash).
            match drop_to_emergency(None, &config, err) {}
        }
    };

    let outcome = run_phases(&config, &mut *console)
        .and_then(|()| select_and_act(&config, &mut *console));

    match outcome {
        Ok(()) => ExitCode::from(0),
        Err(err) => {
            // Hand the live boot console down to the emergency screen so
            // the operator keeps the same backend (splash or tty) they
            // saw during phase progress — no DRM/tty re-grab, no flicker.
            match drop_to_emergency(Some(&mut *console), &config, err) {}
        }
    }
}
