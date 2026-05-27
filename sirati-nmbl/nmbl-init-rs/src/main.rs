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

use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use nix::sys::reboot::{RebootMode, reboot};
use nix::unistd::execve;

use nmbl_init::activation::{KeyInjection, run_all_activations};
use nmbl_init::boot::kexec_into;
use nmbl_init::config::{BootstrapConfig, Config, resolve_full_config_path};
use nmbl_init::devices::mount_system_filesystems;
use nmbl_init::error::{NmblError, Result, format_chain};
use nmbl_init::generations::scan_generations;
use nmbl_init::modules::{load_early_modules, load_explicit_modules, load_modules};
use nmbl_init::mount::mount_pseudo_filesystems;
use nmbl_init::panic::install_panic_hook;
use nmbl_init::shell::{
    drop_to_emergency, open_console_and_drop_to_emergency, print_banner, print_halt_banner,
};
use nmbl_init::sys::{blkid, mount as sys_mount};
use nmbl_init::terminal::{TerminalAction, redirect_stdio_for_execve};
use nmbl_init::ui::console::{Console, NoopConsole, open_console};
use nmbl_init::ui::key_echo::run_key_echo_loop;
use nmbl_init::ui::{BootReporter, Decision, TuiPasswordSupplier, run_selector};
use nmbl_init::{log, nmbl_info, nmbl_warn};

const DEFAULT_CONFIG_PATH: &str = "/etc/nmbl/config.toml";
const BOOTSTRAP_CONFIG_PATH: &str = "/etc/nmbl/bootstrap.toml";

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

/// Run the panic-recovery flow. Returns a [`TerminalAction`] the
/// dispatcher in `main` performs after the call stack has unwound.
fn recover_from_panic(args: Args, report_path: PathBuf) -> (TerminalAction, Config) {
    let report = read_panic_report(&report_path);
    let config = load_config_lenient(&args.config_path);
    log::init(nmbl_init::log::Verbosity::Verbose);

    nmbl_warn!("panic recovery mode: report at {}", report_path.display());
    nmbl_warn!("panic report follows:\n{report}");

    let action = open_console_and_drop_to_emergency(
        &config,
        NmblError::Panicked { report_path },
    );
    (action, config)
}

/// Phase 1: mount /proc, /sys, /dev. Lives at the top of `main` so the
/// optional bootstrap phase (0.5) can see those pseudo-filesystems
/// before it touches blkid or mounts the boot partition. Uses a
/// [`NoopConsole`] sentinel because the real console is not open yet.
fn run_phase_1(reporter: &mut BootReporter<'_>) -> Result<()> {
    nmbl_info!("phase 1: mount pseudo-filesystems");
    mount_pseudo_filesystems(reporter)
}

/// Phase 2a: load early (graphics) kernel modules so the splash backend
/// has a DRM card to attach to when `open_console` runs. Reads
/// `config.kernel_modules.early`. The reporter wraps a [`NoopConsole`];
/// status pushes do nothing visible, but the underlying log ring is
/// still populated for the post-console reporter to surface.
fn run_phase_2a(config: &Config, reporter: &mut BootReporter<'_>) -> Result<()> {
    let _ = reporter.set_phase("phase 2a: load early kernel modules");
    nmbl_info!("phase 2a: load early kernel modules");
    load_early_modules(config, reporter)
}

/// Execute the post-console phases (2b, 3, 3b). The caller has already
/// opened the live console; we wrap it in a [`BootReporter`] so every
/// phase pushes its current "what am I doing" string through the
/// reporter and the operator sees progress on the splash framebuffer
/// or raw-mode tty. The reporter is dropped on return so the caller
/// can reuse the underlying console for the generation selector.
///
/// Returns the LUKS-passphrase injections that the kexec phase must
/// thread into the chained initrd (one per `luks-password` activation
/// whose TOML sets `pass_to_stage1`; empty when none opted in).
fn run_phases_post_console(
    config: &Config,
    console: &mut dyn Console,
) -> Result<Vec<KeyInjection>> {
    let mut reporter = BootReporter::new(console, "phase 2b: loading kernel modules");
    // Paint the first frame so the operator sees a populated screen
    // before any work happens — otherwise a fast phase 2b would race
    // the first kmsg push and the log panel would be empty for one
    // frame. The pre-console phases already populated the log ring,
    // so the snapshot we pull here already shows phase 1 + 2a output.
    let _ = reporter.refresh_log();

    nmbl_info!("phase 2b: load explicit kernel modules");
    load_explicit_modules(config, &mut reporter)?;

    // The splash backend opens /dev/tty1 and calls VT_ACTIVATE itself
    // (see `splash::input::SplashInput::open`); the tty backend uses
    // `/dev/console`, which already points at the kernel-chosen VT.
    // Neither path needs an extra VT switch here.

    nmbl_info!("phase 3: storage activations");
    let mut supplier = TuiPasswordSupplier::new(config);
    let injections = run_all_activations(config, &mut reporter, Some(&mut supplier))?;

    nmbl_info!("phase 3b: mount system filesystems");
    mount_system_filesystems(config, &mut reporter)?;

    Ok(injections)
}

/// Phase 0.5: two-tier bootstrap. Loads the embedded
/// `/etc/nmbl/bootstrap.toml`, brings up the minimum kernel modules it
/// names, sweeps blkid to populate `/dev/disk/by-*`, mounts the boot
/// filesystem, and reads the full `Config` from there.
///
/// On any failure the returned `NmblError::Bootstrap` carries a `stage`
/// string the emergency-shell banner surfaces. Once `boot_fs` is
/// mounted we intentionally leave it mounted on the error path so the
/// operator's shell still sees it under `bootstrap.boot_fs.mountpoint`.
fn run_bootstrap_phase(bootstrap_path: &Path) -> Result<Config> {
    nmbl_info!(
        "phase 0.5: loading bootstrap config {}",
        bootstrap_path.display()
    );
    let bootstrap = BootstrapConfig::load(bootstrap_path)?;
    let section = &bootstrap.bootstrap;

    nmbl_info!(
        "phase 0.5: loading {} bootstrap kernel modules from {}",
        section.kernel_modules.explicit.len(),
        section.kernel_modules.modules_dir.display(),
    );
    load_modules(
        &section.kernel_modules.modules_dir,
        &section.kernel_modules.explicit,
        &[],
    )
    .map_err(|source| NmblError::Bootstrap {
        stage: "load-modules",
        source: Box::new(source),
    })?;

    nmbl_info!("phase 0.5: populating /dev/disk/by-* symlinks");
    blkid::populate_disk_by_symlinks().map_err(|source| NmblError::Bootstrap {
        stage: "blkid-sweep",
        source: Box::new(source),
    })?;

    let boot_fs = &section.boot_fs;
    nmbl_info!(
        "phase 0.5: mounting boot fs {} at {} (type {})",
        boot_fs.device,
        boot_fs.mountpoint.display(),
        boot_fs.fstype,
    );
    std::fs::create_dir_all(&boot_fs.mountpoint).map_err(|source| NmblError::Bootstrap {
        stage: "mount-boot",
        source: Box::new(NmblError::Io {
            source,
            context: format!("creating boot mountpoint {}", boot_fs.mountpoint.display()),
        }),
    })?;
    sys_mount::mount_fs(
        Some(Path::new(&boot_fs.device)),
        &boot_fs.mountpoint,
        &boot_fs.fstype,
        &boot_fs.options,
    )
    .map_err(|source| NmblError::Bootstrap {
        stage: "mount-boot",
        source: Box::new(source),
    })?;

    // boot_fs is mounted; from here on, any failure must NOT unmount
    // it — the operator's emergency shell needs to see it.
    let full_path = resolve_full_config_path(&boot_fs.mountpoint, &section.config_path);
    nmbl_info!(
        "phase 0.5: loading full config from {}",
        full_path.display()
    );
    let mut config = Config::load(&full_path).map_err(|source| NmblError::Bootstrap {
        stage: "read-config",
        source: Box::new(source),
    })?;

    // Hand the runtime boot mountpoint to the rescue dispatcher so
    // `rescue::locate_sfs` can resolve `sfs_path` against it instead of
    // the build-time `/boot` convention.
    config.runtime_boot_mountpoint = Some(boot_fs.mountpoint.clone());

    Ok(config)
}

/// Run phases 4→6 (generation discovery, UI, decision dispatch). Kept
/// separate so the call sites for `drop_to_emergency` stay obvious.
///
/// Phase 4 uses a [`BootReporter`] around `console` so the operator
/// keeps seeing the boot-status screen while we walk the profiles
/// directory. The reporter is dropped before phase 5 so the bare
/// console can be handed to `run_selector`, which swaps the App over
/// to the boot-menu screen on top of the same backend.
fn select_and_act(
    config: &Config,
    console: &mut dyn Console,
    key_injections: &[KeyInjection],
) -> Result<TerminalAction> {
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
            kexec_into(config, target, cmdline_override.as_deref(), key_injections)
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
fn execute_terminal_action(action: TerminalAction) -> ! {
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
        } => {
            if let Some(b) = banner {
                print_banner(&b);
            }
            // Re-open /dev/console and dup2 it onto 0/1/2 so the
            // freshly-execve'd shell renders on the operator's primary
            // console (framebuffer for head, ttyS0 for serial). Every
            // boot-console `Drop` has already fired by now via normal
            // stack unwinding, so the fds we just opened are the ones
            // the shell will inherit. On failure we cannot recover —
            // an execve into invisibility is worse than halting with a
            // banner — so we surface the redirect error through
            // halt_final instead of charging ahead.
            if let Err(err) = redirect_stdio_for_execve() {
                eprintln!(
                    "[nmbl] cannot redirect stdio before execve: {}",
                    format_chain(&err as &dyn std::error::Error)
                );
                halt_final("stdio redirect failed; halting")
            }
            let argv_refs: Vec<&CString> = argv.iter().collect();
            let env_refs: Vec<&CString> = env.iter().collect();
            let _ = execve(&path, &argv_refs, &env_refs);
            halt_final("execve returned; halting")
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

/// Print a one-line final-fallback message and halt. Diverges via
/// `reboot(RB_HALT_SYSTEM)` on success or `libc::_exit(1)` if the
/// kernel refuses (lacking CAP_SYS_BOOT in a sandbox, not PID 1, …).
fn halt_final(reason: &str) -> ! {
    eprintln!("[nmbl] {reason}");
    let _ = reboot(RebootMode::RB_HALT_SYSTEM);
    // SAFETY: libc::_exit is async-signal-safe and unconditionally
    // terminates the process; no crate wraps it (rustix issue #844).
    unsafe { libc::_exit(1) };
}

/// The orchestrator. Returns `ExitCode` so the `--validate-config`
/// path can exit normally; every other path either reaches
/// [`execute_terminal_action`] (which diverges) or returns
/// `ExitCode::SUCCESS` after a normal `Ok(())` outcome.
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
        let (action, _config) = recover_from_panic(args, report_path);
        // _config drops here; action moves into the dispatcher
        // below by value.
        execute_terminal_action(action);
    }

    // Two-tier vs single-tier branch. The bootstrap.toml file is shipped
    // inside the initramfs by the new Nix path; its absence means the
    // image was built with the legacy single-tier flow and the real
    // config lives at `args.config_path` (default `/etc/nmbl/config.toml`).
    //
    // `try_exists()` (not `exists()`) so an unreadable bootstrap.toml
    // (broken symlink, permission denied, …) routes into the bootstrap
    // failure path instead of silently being mistaken for legacy mode
    // — which would then resurface as a misleading missing
    // `/etc/nmbl/config.toml` error.
    let bootstrap_path = Path::new(BOOTSTRAP_CONFIG_PATH);
    let bootstrap_probe = bootstrap_path.try_exists();
    let bootstrap_mode = matches!(bootstrap_probe, Ok(true));

    // Config load is the chicken-and-egg moment: if it fails we have
    // no `shell` path, no verbosity, no nothing. In bootstrap mode the
    // real config only becomes reachable after Phase 0.5 mounts the
    // boot filesystem, so seed from `recovery_default` and replace
    // later. In single-tier mode load from `args.config_path` with a
    // recovery-default fallback as before.
    let (config, load_err): (Config, Option<NmblError>) = if bootstrap_mode {
        (Config::recovery_default(), None)
    } else {
        match Config::load(&args.config_path) {
            Ok(c) => (c, None),
            Err(err) => (Config::recovery_default(), Some(err)),
        }
    };

    // Install the panic hook now that we know where to write reports.
    // A panic during the brief window before this call would still
    // unwind through the default Rust hook, abort PID 1, and let the
    // kernel panic — the documented worst case. In bootstrap mode the
    // configured `panic_report_dir` from the operator's `config.toml`
    // is not yet reachable, so we install with the recovery default
    // and re-install once `run_bootstrap_phase` returns the real config
    // (`install_panic_hook` is idempotent: each call replaces the
    // previously stored directory).
    install_panic_hook(&config.general.panic_report_dir);

    log::init(config.general.verbosity);
    nmbl_info!("nmbl-init starting");

    // Compute the TerminalAction from the inner layers and let the
    // single `execute_terminal_action` site at the bottom of `main`
    // fire the syscall. Every intermediate stack frame is dropped
    // before that call, which is what restores VT mode and termios
    // for the freshly-execve'd shell.
    let action = match run_inner(config, load_err, bootstrap_probe, bootstrap_path, &args) {
        Ok(action) => action,
        Err(boxed) => {
            // Unrecoverable: render the emergency screen, drop to
            // shell. drop_to_emergency itself returns a
            // TerminalAction so this collapses to a single path
            // back to the dispatcher.
            let (err, config) = *boxed;
            open_console_and_drop_to_emergency(&config, err)
        }
    };

    // Drop everything left on this stack frame, then fire the
    // syscall. `execute_terminal_action` diverges so the trailing
    // ExitCode is unreachable in practice; it only exists to satisfy
    // the signature of `main`.
    execute_terminal_action(action);
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

    // Phase 1 lives at the top of `main` so Phase 0.5 (when active) and
    // Phase 2a both see /proc, /sys, /dev already mounted. The reporter
    // wraps a NoopConsole until phase 2b opens the real one; nmbl_info!
    // lines still reach the kernel ring and replay onto the live
    // console once it comes up.
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
                // Re-install the panic hook against the operator's
                // configured directory and re-init the logger at the
                // operator's verbosity. The early init used the recovery
                // defaults because the real config was not yet
                // reachable, but a panic or log line during the
                // remaining phases must honour what the operator set in
                // /boot's config.toml.
                install_panic_hook(&config.general.panic_report_dir);
                log::init(config.general.verbosity);
            }
            // boot_fs may have been mounted before the failure — the
            // emergency shell wants to see it, so we do NOT unmount on
            // this path.
            Err(err) => return Err(Box::new((err, config))),
        }
    }

    // Phase 2a runs BEFORE the real console is opened: the splash
    // backend needs a DRM card to attach to, and phase 2a brings up the
    // graphics-driver modules (virtio_gpu / simpledrm / i915 / …) that
    // materialise `/dev/dri/card*`.
    {
        let mut reporter = BootReporter::new(&mut noop, "phase 2a: load early kernel modules");
        if let Err(err) = run_phase_2a(&config, &mut reporter) {
            nmbl_warn!("phase 2a (early modules) failed: {err}");
            return Err(Box::new((err, config)));
        }
    }

    // Bring the boot console up AFTER phase 2a so the splash backend
    // can attach to the DRM card the early modules just brought up.
    // The same backend is reused all the way through the boot-menu
    // selector and the emergency screen on phase failure.
    let mut console: Box<dyn Console> = match open_console(&config, false) {
        Ok(c) => c,
        Err(err) => {
            nmbl_warn!("boot console bring-up failed: {err}");
            return Err(Box::new((err, config)));
        }
    };

    // Diagnostic harness: `nmbl.key_echo=1` on the kernel cmdline
    // routes us into the key-echo screen instead of the normal boot
    // flow. The screen never returns a Decision; on Ctrl+C we fall
    // through to `drop_to_emergency` so the operator still gets a
    // shell. This branch is gated and unreachable in production
    // boots (cmdline tokens are operator-set, not config-set).
    if cmdline_has_key_echo_flag() {
        nmbl_info!("nmbl.key_echo=1 in cmdline: entering key-echo diagnostic screen");
        if let Err(e) = run_key_echo_loop(&mut *console) {
            nmbl_warn!(
                "key-echo loop error: {}",
                format_chain(&e as &dyn std::error::Error)
            );
        }
        let err = NmblError::Io {
            source: std::io::Error::other("key-echo diagnostic mode terminated"),
            context: "key-echo".to_string(),
        };
        // Hand the live console down to drop_to_emergency so the
        // emergency UI paints through the same backend.
        return Ok(drop_to_emergency(console, &config, err));
    }

    match run_phases_post_console(&config, &mut *console)
        .and_then(|injections| select_and_act(&config, &mut *console, &injections))
    {
        Ok(action) => {
            // `console` falls out of scope on this return, running
            // SplashConsole/TtyConsole Drop (KD_TEXT restore, termios
            // reset) before the no-return syscall fires in main.
            let _ = console;
            Ok(action)
        }
        Err(err) => {
            // Hand the live boot console down to the emergency screen
            // so the operator keeps the same backend (splash or tty)
            // they saw during phase progress — no DRM/tty re-grab, no
            // flicker. drop_to_emergency itself drops the console
            // before returning, by way of normal scope exit.
            Ok(drop_to_emergency(console, &config, err))
        }
    }
}
