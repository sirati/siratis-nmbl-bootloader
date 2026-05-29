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
#[cfg(feature = "stateful")]
use nmbl_init::generations::Generation;
#[cfg(feature = "stateful")]
use nmbl_init::generations::active_generation_index;
use nmbl_init::generations::scan_generations;
use nmbl_init::modules::{load_early_modules, load_explicit_modules, load_modules};
use nmbl_init::mount::mount_pseudo_filesystems;
use nmbl_init::panic::install_panic_hook;
use nmbl_init::rescue::{self, RescueMode};
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

// Tmpfs path the byte-ring is flushed to right before every terminal
// action — defined once in `log::NMBL_LOG_PATH`. The parent dir is
// `mkdir -p`'d on every call; EEXIST is benign, anything else means
// tmpfs is broken and we surface a warning but still proceed with the
// terminal action.
use log::NMBL_LOG_PATH;

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

#[derive(Debug)]
struct Args {
    config_path: PathBuf,
    errored_report: Option<PathBuf>,
    validate_config: Option<PathBuf>,
    /// Installer-side: initialise (or validate) state.bin under the
    /// given directory and exit. Mutually exclusive with
    /// `validate_config` and `boot_succeeded_dir`.
    #[cfg(feature = "stateful")]
    init_state_dir: Option<PathBuf>,
    /// systemd-unit side: flip `last_boot_succeeded = true` in
    /// state.bin under the given directory and exit. Mutually
    /// exclusive with `validate_config` and `init_state_dir`.
    #[cfg(feature = "stateful")]
    boot_succeeded_dir: Option<PathBuf>,
}

/// Hand-rolled arg parsing: clap is too big for the size budget. We
/// recognise `--config=<v>` / `--config <v>` and the same two forms
/// for `--errored`, `--validate-config`, `--init-state`, and
/// `--boot-succeeded`. Anything else is silently ignored — PID 1 has
/// no useful "usage" target to print to.
///
/// Returns `Err(String)` when the caller asked for a stateful flag in
/// a binary built without the `stateful` feature, when two mutually
/// exclusive early-exit modes were combined, or when an early-exit
/// flag was passed with no path argument. Those three cases are
/// programmer / operator errors, not normal boot failures, and we
/// surface them to stderr before the panic hook or logger come up.
fn parse_args() -> std::result::Result<Args, String> {
    parse_args_from(std::env::args_os().skip(1))
}

/// Pure parsing core: takes an iterator of arg-like values so unit
/// tests can drive the parser without touching `std::env::args_os()`.
/// `parse_args` is the production entry point and stays a one-liner.
fn parse_args_from<I, S>(args: I) -> std::result::Result<Args, String>
where
    I: IntoIterator<Item = S>,
    S: Into<std::ffi::OsString>,
{
    let mut config_path = PathBuf::from(DEFAULT_CONFIG_PATH);
    let mut errored_report: Option<PathBuf> = None;
    let mut validate_config: Option<PathBuf> = None;
    #[cfg(feature = "stateful")]
    let mut init_state_dir: Option<PathBuf> = None;
    #[cfg(feature = "stateful")]
    let mut boot_succeeded_dir: Option<PathBuf> = None;

    let mut iter = args.into_iter().map(Into::into);
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
        } else if let Some(rest) = arg.strip_prefix("--init-state=") {
            #[cfg(feature = "stateful")]
            {
                init_state_dir = Some(PathBuf::from(rest));
            }
            #[cfg(not(feature = "stateful"))]
            {
                let _ = rest;
                return Err(
                    "--init-state requires nmbl-init to be built with the `stateful` feature"
                        .to_string(),
                );
            }
        } else if arg == "--init-state" {
            #[cfg(feature = "stateful")]
            {
                let Some(v) = iter.next() else {
                    return Err("--init-state requires a directory argument".to_string());
                };
                init_state_dir = Some(PathBuf::from(v));
            }
            #[cfg(not(feature = "stateful"))]
            {
                return Err(
                    "--init-state requires nmbl-init to be built with the `stateful` feature"
                        .to_string(),
                );
            }
        } else if let Some(rest) = arg.strip_prefix("--boot-succeeded=") {
            #[cfg(feature = "stateful")]
            {
                boot_succeeded_dir = Some(PathBuf::from(rest));
            }
            #[cfg(not(feature = "stateful"))]
            {
                let _ = rest;
                return Err(
                    "--boot-succeeded requires nmbl-init to be built with the `stateful` feature"
                        .to_string(),
                );
            }
        } else if arg == "--boot-succeeded" {
            #[cfg(feature = "stateful")]
            {
                let Some(v) = iter.next() else {
                    return Err("--boot-succeeded requires a directory argument".to_string());
                };
                boot_succeeded_dir = Some(PathBuf::from(v));
            }
            #[cfg(not(feature = "stateful"))]
            {
                return Err(
                    "--boot-succeeded requires nmbl-init to be built with the `stateful` feature"
                        .to_string(),
                );
            }
        }
    }

    // Mutual exclusion across the three early-exit modes. Each mode
    // funnels into a different exit path (validate, init-state,
    // boot-succeeded); combining them would silently pick one and
    // drop the others, masking an operator typo.
    #[cfg(feature = "stateful")]
    {
        let count = u8::from(validate_config.is_some())
            + u8::from(init_state_dir.is_some())
            + u8::from(boot_succeeded_dir.is_some());
        if count > 1 {
            return Err(
                "--validate-config, --init-state, and --boot-succeeded are mutually exclusive"
                    .to_string(),
            );
        }
    }

    Ok(Args {
        config_path,
        errored_report,
        validate_config,
        #[cfg(feature = "stateful")]
        init_state_dir,
        #[cfg(feature = "stateful")]
        boot_succeeded_dir,
    })
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
fn run_phase_1(reporter: &mut BootReporter<'_, '_>) -> Result<()> {
    nmbl_info!("phase 1: mount pseudo-filesystems");
    mount_pseudo_filesystems(reporter)
}

/// Phase 2a: load early (graphics) kernel modules so the splash backend
/// has a DRM card to attach to when `open_console` runs. Reads
/// `config.kernel_modules.early`. The reporter wraps a [`NoopConsole`];
/// status pushes do nothing visible, but the underlying log ring is
/// still populated for the post-console reporter to surface.
fn run_phase_2a(config: &Config, reporter: &mut BootReporter<'_, '_>) -> Result<()> {
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
    // When stateful storage is configured the runtime needs to rewrite
    // `state.bin` on this same device. We mount the boot fs read-write
    // ONCE here and later bind a writable view at the state mountpoint;
    // mounting the same block device twice fails with EBUSY on vfat. The
    // operator's `ro` default is only honoured when no state mount is
    // configured.
    let stateful_rw = section.state.is_some();
    let boot_options = if stateful_rw {
        if boot_fs.options.is_empty() {
            "rw,nosuid,noexec,nodev".to_string()
        } else {
            format!("{},rw,nosuid,noexec,nodev", boot_fs.options)
        }
    } else {
        boot_fs.options.clone()
    };
    nmbl_info!(
        "phase 0.5: mounting boot fs {} at {} (type {}, options {})",
        boot_fs.device,
        boot_fs.mountpoint.display(),
        boot_fs.fstype,
        boot_options,
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
        &boot_options,
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
    // the build-time `/boot` convention. This must be set before
    // `run_inner` evaluates the `force_on_boot` rescue trigger, which
    // only needs the boot mount — never the stateful state bind below.
    config.runtime_boot_mountpoint = Some(boot_fs.mountpoint.clone());

    Ok(config)
}

/// Stateful side of Phase 0.5: expose a writable view of the boot fs at
/// `state.mountpoint` so `state.bin` can be rewritten between boots. The
/// boot device is already mounted read-write at `boot_fs.mountpoint` by
/// [`run_bootstrap_phase`] when stateful is enabled; we `MS_BIND` that
/// mount here rather than mounting the block device a second time, which
/// fails with EBUSY on vfat. A bind shares the existing RW mount, so the
/// state view is writable without re-opening the device.
///
/// Split out of `run_bootstrap_phase` so `run_inner` can evaluate the
/// `force_on_boot` rescue trigger BEFORE this mount runs: the force path
/// skips generation boot entirely, so it never touches `state.bin` and
/// must not be blocked by a state-mount failure.
#[cfg(feature = "stateful")]
fn mount_state_twin(config: &mut Config, bootstrap_path: &Path) -> Result<()> {
    let bootstrap = BootstrapConfig::load(bootstrap_path)?;
    let section = &bootstrap.bootstrap;
    let boot_fs = &section.boot_fs;
    let Some(state_mount) = &section.state else {
        return Ok(());
    };
    let mp = &state_mount.mountpoint;
    nmbl_info!(
        "phase 0.5: bind-mounting {} at {} for state.bin",
        boot_fs.mountpoint.display(),
        mp.display(),
    );
    std::fs::create_dir_all(mp).map_err(|source| NmblError::Bootstrap {
        stage: "mount-state",
        source: Box::new(NmblError::Io {
            source,
            context: format!("creating state mountpoint {}", mp.display()),
        }),
    })?;
    sys_mount::mount_fs(Some(&boot_fs.mountpoint), mp, &boot_fs.fstype, "bind").map_err(
        |source| NmblError::Bootstrap {
            stage: "mount-state",
            source: Box::new(source),
        },
    )?;
    config.runtime_state_mountpoint = Some(mp.clone());
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

    // Stateful rollback gate. When the operator opted into stateful
    // storage AND state.bin is readable, `select_with_stateful` decides
    // whether to honour the TUI countdown, force-pick a known-good
    // generation, or surface an Exhausted rescue condition. In every
    // other case (no feature, no opt-in, missing/unsupported state.bin,
    // IO failure) the call collapses to the legacy `run_selector` path.
    #[cfg(feature = "stateful")]
    let decision = select_with_stateful(config, &generations, console)?;
    #[cfg(not(feature = "stateful"))]
    let decision = {
        nmbl_info!("phase 5: TUI generation selector");
        run_selector(config, &generations, console)?
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

/// Stateful entry point for the boot selector. Returns the same
/// [`Decision`] shape `run_selector` would have returned; the caller's
/// match on `Decision::Boot` / `Shell` / `Reboot` does not change.
///
/// Decision tree:
///   - No `[stateful]` table, or no `[bootstrap.state]` mount, or
///     no readable `state.bin`: fall back to `run_selector` unchanged.
///   - `state::decide` → `HonourTui`: call `run_selector`, then record
///     the operator's pick in `state.bin` before returning.
///   - `state::decide` → `ForcePick(idx)`: skip the TUI, synthesize a
///     `Decision::Boot` for `generations[idx]`, record the pick in
///     `state.bin`.
///   - `state::decide` → `Exhausted`: surface as
///     `NmblError::Rescue { stage: "stateful-exhausted", ... }` so
///     `run_inner`'s existing error arm routes through the emergency
///     screen.
#[cfg(feature = "stateful")]
fn select_with_stateful(
    config: &Config,
    generations: &[Generation],
    console: &mut dyn Console,
) -> Result<Decision> {
    // No opt-in: legacy path verbatim.
    let (Some(_stateful), Some(state_mp)) = (
        config.stateful.as_ref(),
        config.runtime_state_mountpoint.as_deref(),
    ) else {
        nmbl_info!("phase 5: TUI generation selector");
        return run_selector(config, generations, console);
    };

    let state_path = state_mp.join("nmbl").join("state.bin");
    let mut state = match nmbl_init::state::read(&state_path) {
        Ok(Some(s)) => s,
        Ok(None) => {
            // File missing or wire-format version newer than us. Either
            // is an explicit "fall back to non-stateful" signal per the
            // forward-compat contract on `State`; do not surface as
            // failure, just skip the rollback flow this boot.
            nmbl_warn!(
                "state.bin at {} absent or unsupported; skipping stateful boot this cycle",
                state_path.display(),
            );
            nmbl_info!("phase 5: TUI generation selector");
            return run_selector(config, generations, console);
        }
        Err(err) => {
            // IO error other than NotFound (which `read` already maps to
            // Ok(None)). The operator's choice was to enable stateful;
            // surfacing this as a hard rescue would be heavy-handed for
            // what may be a transient FS hiccup, so the contract is to
            // warn and skip — same fall-back as a missing file.
            nmbl_warn!(
                "state.bin at {} could not be read ({err}); skipping stateful boot this cycle",
                state_path.display(),
            );
            nmbl_info!("phase 5: TUI generation selector");
            return run_selector(config, generations, console);
        }
    };

    // Already validated at TOML parse time that `stateful = Some(...)`
    // means max_recovery_attempts is present.
    let max_attempts = _stateful.max_recovery_attempts;
    let active_index = active_generation_index(generations, &config.paths.nix_profiles_dir);

    match nmbl_init::state::decide(&mut state, generations, active_index, max_attempts) {
        nmbl_init::state::StatefulDecision::HonourTui => {
            nmbl_info!(
                "phase 5: TUI generation selector (stateful: honour operator choice, recovery_attempt={})",
                state.recovery_attempt,
            );
            let decision = run_selector(config, generations, console)?;
            if let Decision::Boot {
                generation_index,
                cmdline_override: _,
            } = &decision
            {
                record_attempt(&mut state, generations, *generation_index, &state_path);
            }
            Ok(decision)
        }
        nmbl_init::state::StatefulDecision::ForcePick(idx) => {
            let Some(target) = generations.get(idx) else {
                return Err(NmblError::ConfigInvalid {
                    reason: format!(
                        "state::decide returned ForcePick({idx}) but only {} generations",
                        generations.len()
                    ),
                    context: "stateful dispatch".to_string(),
                });
            };
            nmbl_info!(
                "phase 5: stateful rollback forced generation {} (recovery_attempt={})",
                target.number,
                state.recovery_attempt,
            );
            record_attempt(&mut state, generations, idx, &state_path);
            Ok(Decision::Boot {
                generation_index: idx,
                cmdline_override: None,
            })
        }
        nmbl_init::state::StatefulDecision::Exhausted => {
            // The emergency menu reads the source chain via
            // `format_chain`, so wrap a leaf error that explains *why*
            // the rescue arm fired. There's no `NmblError::Other`
            // variant; the existing pattern (e.g. `select_and_act`'s
            // `Decision::Shell` arm) wraps a free-form message in
            // `NmblError::Io` via `io::Error::other`. Reusing that here
            // keeps the chain walker happy and the operator-facing
            // string clear.
            Err(NmblError::Rescue {
                stage: "stateful-exhausted",
                source: Box::new(NmblError::Io {
                    source: std::io::Error::other(
                        "max recovery attempts exceeded; no known-good generation left to try",
                    ),
                    context: "stateful dispatch".to_string(),
                }),
            })
        }
    }
}

/// Persist the operator's (or stateful dispatcher's) generation pick to
/// `state.bin` before kexec. Write failures degrade to a warning — the
/// next boot will retry the decision against a stale state.bin, which
/// is strictly less bad than blocking the boot handoff. The `u32::MAX`
/// edge case on `NonMaxU32::new` is theoretical (Nix never emits that
/// many generations), but we still log and skip the state update rather
/// than panicking.
#[cfg(feature = "stateful")]
fn record_attempt(
    state: &mut nmbl_init::state::State,
    generations: &[Generation],
    generation_index: usize,
    state_path: &Path,
) {
    let Some(target) = generations.get(generation_index) else {
        nmbl_warn!(
            "stateful: generation index {generation_index} out of range, skipping state.bin update",
        );
        return;
    };
    match nonmax::NonMaxU32::new(target.number) {
        Some(n) => state.last_attempted_generation = Some(n),
        None => {
            nmbl_warn!(
                "stateful: generation number {} is u32::MAX, cannot record in state.bin",
                target.number,
            );
            return;
        }
    }
    state.last_boot_succeeded = false;
    if let Err(err) = nmbl_init::state::write_padded(state_path, state) {
        nmbl_warn!(
            "stateful: failed to write state.bin at {}: {err}; proceeding with kexec anyway",
            state_path.display(),
        );
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
                    eprintln!(
                        "[nmbl] rescue: stdio redirect unavailable, proceeding with inherited fds"
                    );
                } else {
                    halt_final("stdio redirect failed; halting")
                }
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

    // Installer and systemd-unit early-exit dispatches. Both flags
    // touch `state.bin` and then exit — never the PID-1 init flow.
    // Mirrors `--validate-config` above: no panic hook, no logger
    // init, no `run_inner`. The parser already rejects these flags
    // when the `stateful` feature is off, so the dispatch site only
    // exists under the same gate.
    #[cfg(feature = "stateful")]
    {
        if let Some(dir) = args.init_state_dir.as_deref() {
            return match nmbl_init::state::init_or_validate(dir) {
                Ok(_) => {
                    println!("nmbl-init: state.bin OK under {}", dir.display());
                    ExitCode::from(0)
                }
                Err(err) => {
                    eprintln!(
                        "nmbl-init: --init-state failed for {}: {}",
                        dir.display(),
                        format_chain(&err as &dyn std::error::Error),
                    );
                    ExitCode::from(1)
                }
            };
        }

        if let Some(dir) = args.boot_succeeded_dir.as_deref() {
            return match nmbl_init::state::mark_boot_succeeded(dir) {
                Ok(()) => ExitCode::from(0),
                Err(err) => {
                    eprintln!(
                        "nmbl-init: --boot-succeeded failed for {}: {}",
                        dir.display(),
                        format_chain(&err as &dyn std::error::Error),
                    );
                    ExitCode::from(1)
                }
            };
        }
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

    // Deterministic rescue trigger. When the operator (or the test
    // harness) set `rescue.force_on_boot` AND the rescue mode is
    // `external`, skip the entire generation-boot flow and switch_root
    // straight into the rescue squashfs. This runs after Phase 0.5 has
    // mounted the boot partition (so `runtime_boot_mountpoint` — which
    // `rescue::locate_sfs` needs — is populated) but BEFORE the stateful
    // state bind mount and before any console is opened, so no
    // interactive input is required: a single config bool fully
    // determines the path. Crucially it must run before `mount_state_twin`
    // so a state-mount failure cannot block the rescue
    // boot — the force path never touches `state.bin`. `dispatch` takes
    // the console by ownership; the disk-rescue arm never paints to it, so
    // a NoopConsole is sufficient. Production boots leave
    // `force_on_boot = false` and never enter this branch.
    if should_force_external_rescue(&config) {
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
            let mut reporter =
                BootReporter::new(&mut noop, "force_on_boot: load rescue kernel modules");
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
        return match rescue::dispatch(&config, console, cause) {
            Ok(action) => Ok(action),
            Err(err) => {
                nmbl_warn!(
                    "force_on_boot: external rescue dispatch failed: {}",
                    format_chain(&err as &dyn std::error::Error)
                );
                Err(Box::new((err, config)))
            }
        };
    }

    // Stateful state bind mount (Phase 0.5, stateful side). Deferred
    // to here — after the `force_on_boot` short-circuit — so the force
    // rescue path is never blocked by a state-mount failure. On the
    // normal boot path a failure still routes through the emergency
    // screen exactly as before.
    #[cfg(feature = "stateful")]
    if bootstrap_mode
        && let Err(err) = mount_state_twin(&mut config, bootstrap_path)
    {
        return Err(Box::new((err, config)));
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
        // Wrong-password modal Reboot path: the operator already picked
        // [Reboot]; routing through the emergency menu would just ask
        // them again. Short-circuit straight to the dispatcher's reboot
        // syscall, dropping `console` along the way so its Drop restores
        // the VT before reboot(2) fires.
        Err(NmblError::OperatorChoseReboot { .. }) => {
            let _ = console;
            Ok(TerminalAction::Reboot)
        }
        Err(err) => {
            // Hand the live boot console down to the emergency screen
            // so the operator keeps the same backend (splash or tty)
            // they saw during phase progress — no DRM/tty re-grab, no
            // flicker. drop_to_emergency itself drops the console
            // before returning, by way of normal scope exit.
            //
            // `NmblError::WrongPasswordShellExited` deliberately falls
            // through this arm so the standard emergency menu surfaces
            // — its [Retry boot from config] re-runs phase 3 and re-
            // prompts for the passphrase, which is exactly what the
            // operator wants after a shell detour.
            Ok(drop_to_emergency(console, &config, err))
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;

    #[cfg(feature = "stateful")]
    #[test]
    fn init_state_with_path_parses() {
        let args =
            parse_args_from(["--init-state", "/some/path"]).expect("--init-state should parse");
        assert_eq!(
            args.init_state_dir.as_deref(),
            Some(Path::new("/some/path"))
        );
        assert!(args.boot_succeeded_dir.is_none());
        assert!(args.validate_config.is_none());
    }

    #[cfg(feature = "stateful")]
    #[test]
    fn init_state_equals_form_parses() {
        let args =
            parse_args_from(["--init-state=/some/path"]).expect("--init-state=… should parse");
        assert_eq!(
            args.init_state_dir.as_deref(),
            Some(Path::new("/some/path"))
        );
    }

    #[cfg(feature = "stateful")]
    #[test]
    fn boot_succeeded_with_path_parses() {
        let args = parse_args_from(["--boot-succeeded", "/some/path"])
            .expect("--boot-succeeded should parse");
        assert_eq!(
            args.boot_succeeded_dir.as_deref(),
            Some(Path::new("/some/path"))
        );
        assert!(args.init_state_dir.is_none());
        assert!(args.validate_config.is_none());
    }

    #[cfg(feature = "stateful")]
    #[test]
    fn init_state_and_boot_succeeded_are_mutually_exclusive() {
        let err = parse_args_from(["--init-state", "/a", "--boot-succeeded", "/b"])
            .expect_err("both flags at once must be rejected");
        assert!(err.contains("mutually exclusive"), "{err}");
    }

    #[cfg(feature = "stateful")]
    #[test]
    fn validate_config_and_init_state_are_mutually_exclusive() {
        let err = parse_args_from(["--validate-config", "/c", "--init-state", "/a"])
            .expect_err("validate-config + init-state must be rejected");
        assert!(err.contains("mutually exclusive"), "{err}");
    }

    #[cfg(feature = "stateful")]
    #[test]
    fn init_state_without_argument_errors() {
        let err =
            parse_args_from(["--init-state"]).expect_err("--init-state without dir must error");
        assert!(err.contains("requires a directory argument"), "{err}");
    }

    #[cfg(feature = "stateful")]
    #[test]
    fn boot_succeeded_without_argument_errors() {
        let err = parse_args_from(["--boot-succeeded"])
            .expect_err("--boot-succeeded without dir must error");
        assert!(err.contains("requires a directory argument"), "{err}");
    }

    #[cfg(not(feature = "stateful"))]
    #[test]
    fn init_state_without_feature_errors() {
        // The operator built nmbl-init without `stateful` but still
        // passed `--init-state`; we must not silently ignore — that
        // would leave state.bin uninitialised and bricked installers
        // would be invisible at build time.
        let err = parse_args_from(["--init-state", "/a"])
            .expect_err("--init-state without feature must error");
        assert!(err.contains("stateful"), "{err}");
    }

    #[cfg(not(feature = "stateful"))]
    #[test]
    fn boot_succeeded_without_feature_errors() {
        let err = parse_args_from(["--boot-succeeded", "/a"])
            .expect_err("--boot-succeeded without feature must error");
        assert!(err.contains("stateful"), "{err}");
    }

    #[cfg(not(feature = "stateful"))]
    #[test]
    fn init_state_equals_without_feature_errors() {
        let err = parse_args_from(["--init-state=/a"])
            .expect_err("--init-state=… without feature must error");
        assert!(err.contains("stateful"), "{err}");
    }

    #[test]
    fn unknown_args_are_ignored() {
        // PID 1 has no "usage" target; unknown flags must not abort.
        let args = parse_args_from(["--no-such-flag", "garbage"])
            .expect("unknown flags should be silently dropped");
        assert_eq!(args.config_path, PathBuf::from(DEFAULT_CONFIG_PATH));
        assert!(args.errored_report.is_none());
        assert!(args.validate_config.is_none());
    }

    #[test]
    fn force_on_boot_external_selects_rescue() {
        // The regression: force_on_boot=true + mode=external must select
        // the deterministic external-rescue path. Both conditions are
        // required — neither alone fires the trigger.
        let mut cfg = Config::recovery_default();
        cfg.rescue.force_on_boot = true;
        cfg.rescue.mode = RescueMode::External;
        assert!(should_force_external_rescue(&cfg));
    }

    #[test]
    fn force_on_boot_requires_external_mode() {
        // force_on_boot with a non-external mode is a no-op: embedded and
        // none are not no-input deterministic rescue targets.
        for mode in [RescueMode::Embedded, RescueMode::None] {
            let mut cfg = Config::recovery_default();
            cfg.rescue.force_on_boot = true;
            cfg.rescue.mode = mode;
            assert!(
                !should_force_external_rescue(&cfg),
                "force_on_boot must not fire for mode {mode:?}"
            );
        }
    }

    #[test]
    fn external_mode_without_force_does_not_trigger() {
        // Production default: external rescue configured but not forced
        // must leave the normal generation-boot flow untouched.
        let mut cfg = Config::recovery_default();
        cfg.rescue.force_on_boot = false;
        cfg.rescue.mode = RescueMode::External;
        assert!(!should_force_external_rescue(&cfg));
    }

    #[test]
    fn force_path_loads_explicit_set_before_dispatch() {
        // Contract for the ordering fix: the force_on_boot branch must
        // run the EXPLICIT module set (which carries the auto-added
        // `loop`/`squashfs`/nicDrivers for mode==external) before
        // `rescue::dispatch`. We can't drive `run_inner` (it is PID-1
        // flow), but we can lock in that the explicit list — not the
        // early list — is the one a forced external rescue depends on,
        // and that the loader is a no-op when that list is empty (so the
        // pre-dispatch call never spuriously fails a forced boot on a
        // platform with built-in loop/squashfs).
        let mut cfg = Config::recovery_default();
        cfg.rescue.force_on_boot = true;
        cfg.rescue.mode = RescueMode::External;
        cfg.kernel_modules.explicit =
            vec!["loop".to_owned(), "squashfs".to_owned(), "virtio_net".to_owned()];
        assert!(should_force_external_rescue(&cfg));
        // The force path loads `config.kernel_modules.explicit`; confirm
        // the rescue-critical names live there and not in `early`.
        assert!(cfg.kernel_modules.explicit.iter().any(|m| m == "loop"));
        assert!(cfg.kernel_modules.explicit.iter().any(|m| m == "squashfs"));
        assert!(cfg.kernel_modules.early.is_empty());

        // Empty explicit list -> loader short-circuits Ok (no modules
        // tree parse), so a forced boot is never blocked by an empty set.
        let mut empty = Config::recovery_default();
        empty.kernel_modules.explicit.clear();
        let mut noop = NoopConsole::new();
        let mut reporter = BootReporter::new(&mut noop, "test");
        load_explicit_modules(&empty, &mut reporter).expect("empty explicit set must be a no-op");
    }

    #[test]
    fn validate_config_parses_in_default_build() {
        let args = parse_args_from(["--validate-config", "/etc/nmbl/config.toml"])
            .expect("--validate-config should parse without stateful feature");
        assert_eq!(
            args.validate_config.as_deref(),
            Some(Path::new("/etc/nmbl/config.toml"))
        );
    }
}
