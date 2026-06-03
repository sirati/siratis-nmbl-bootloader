//! Boot orchestration that runs inside the interactive
//! [`LocalRuntime`]. Extracted from `main.rs` to keep that file under the
//! file-size limit: this module owns [`run_boot_inside_runtime`], the
//! [`BootOutcome`] enum it returns, and the two sub-flows it can branch
//! into ([`run_force_rescue`] and [`run_key_echo_diagnostic`]).

use std::path::Path;

use nmbl_init::config::Config;
use nmbl_init::error::{NmblError, format_chain};
use nmbl_init::imageload::{DriverImagesHandle, detach_all_driver_images, load_driver_images};
use nmbl_init::modules::load_explicit_modules;
use nmbl_init::panic::install_panic_hook;
use nmbl_init::rescue::{self};
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

/// Force-external-rescue sub-flow. Loads the explicit module set (which
/// carries the network-rescue NIC/`af_packet` set when `rescue.network`
/// is on — NOT `loop`/`squashfs`, which `rescue::dispatch` →
/// `prepare_disk_rescue` now loads on demand), then calls
/// `rescue::dispatch`. Extracted from `run_inner` to keep that fn under
/// 100 lines.
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
    // SEAL ON ENTRY (G5): force-on-boot rescue drops the operator into an
    // interactive rescue system, so cap the lock PCR + close every
    // TPM-unsealed mapper FIRST (blocking — this runs after the runtime
    // exits). `rescue::dispatch` re-seals idempotently. On a seal failure
    // route through the refuse terminus (M1): best-effort relock + sentinel
    // + reboot into rescue, instead of entering an interactive rescue.
    if let Err(seal_err) = nmbl_init::policy::seal_secrets_blocking(config.tpm.require_tpm) {
        nmbl_warn!(
            "force_on_boot: seal-on-rescue failed; relocking and rebooting into rescue: {}",
            format_chain(seal_err.cause() as &dyn std::error::Error)
        );
        return Ok(nmbl_init::policy::refuse_unsigned_blocking(
            &config,
            seal_err.into_cause(),
        ));
    }
    // The network-rescue NIC drivers + `af_packet` are added to
    // `config.kernel_modules.explicit` for `rescue.network &&
    // rescue.mode == external` (see lib/config.nix `rescueNicModules`/
    // `rescuePacketModule`), normally loaded in phase 2b. The force path
    // short-circuits before phase 2b, so load the explicit set now so
    // NMBL's own in-initramfs DHCP path has its NIC driver. `loop` +
    // `squashfs` are NOT in this list anymore: `rescue::dispatch` →
    // `prepare_disk_rescue` loads them on demand right before the
    // loop-mount, so this path no longer has to. Load via the pre-console
    // NoopConsole reporter exactly as phase 2a does.
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
    if bootstrap_mode {
        match run_bootstrap_phase(bootstrap_path, &sender).await {
            Ok(loaded) => {
                config = loaded;
                install_panic_hook(&config.general.panic_report_dir);
                log::init(config.general.verbosity);
            }
            Err(err) => return BootOutcome::Done(Box::new(Err(Box::new((err, config))))),
        }
    }
    // Force-rescue decision: the legacy `rescue.force_on_boot && external`
    // trigger UNIONED with the rescue sentinel (FIX-49/MED-1). Routing through
    // `should_force_rescue` is what actually READS the sentinel — without it an
    // empty `/boot/nmbl/rescue` (e.g. dropped by a prior refuse→reboot) would
    // never force rescue and the box could re-enter the failing boot. A
    // sentinel-forced rescue takes the SAME `rescue::dispatch` path, whose G4
    // seal keeps the TPM locked.
    if nmbl_init::policy::should_force_rescue(should_force_external_rescue(&config), &config) {
        return BootOutcome::ForceRescue(Box::new(config));
    }
    #[cfg(feature = "stateful")]
    if bootstrap_mode && let Err(err) = mount_state_twin(&mut config, bootstrap_path) {
        return BootOutcome::Done(Box::new(Err(Box::new((err, config)))));
    }
    {
        let mut reporter = BootReporter::new(noop, "phase 2a: load early kernel modules");
        if let Err(err) = run_phase_2a(&config, &mut reporter) {
            nmbl_warn!("phase 2a (early modules) failed: {err}");
            return BootOutcome::Done(Box::new(Err(Box::new((err, config)))));
        }
    }
    // Driver-image hook (#24 / FEATURE-#1): load every declared, signed
    // out-of-tree driver image AFTER the early explicit-module load and
    // BEFORE the generation kexec, so extra drivers (and their firmware) are
    // available for the rest of boot. A no-op unless `driver_images.enable`
    // (FIX-05 guarantees enable ⇒ secure-boot). A verify/load failure surfaces
    // `NmblError::DriverImage`, which we route through the async
    // `refuse_unsigned` terminus — cap → close-mappers → sentinel → relock,
    // then `RebootIntoRescue` (R-1; NOT a halt). No console is open yet, so the
    // non-interactive refuse countdown does not render here; the reboot fires
    // in `execute_terminal_action` after the runtime unwinds.
    let mut driver_images = match load_driver_images(&config) {
        Ok(handle) => handle,
        Err(err) => {
            nmbl_warn!(
                "driver-image load failed; relocking and rebooting into rescue: {}",
                format_chain(&err as &dyn std::error::Error)
            );
            let action = nmbl_init::policy::refuse_unsigned(&config, err, &sender).await;
            return BootOutcome::Done(Box::new(Ok(action)));
        }
    };
    // #28 (Wave-4): `driver_images` is the ORDERED accumulator of every loaded
    // driver image (this base set, plus any staged-rerun additions the session
    // appends). It is threaded `&mut` into `run_tui_session` so the staged path
    // can extend it, and its verified `measure_refs()` feed TPM measure event #4
    // through the kexec handoff. It is torn down below on the normal terminus.
    let console: Box<dyn Console> = match open_console(&config, false) {
        Ok(c) => c,
        Err(err) => {
            nmbl_warn!("boot console bring-up failed: {err}");
            return BootOutcome::Done(Box::new(Err(Box::new((err, config)))));
        }
    };
    if cmdline_has_key_echo_flag() {
        // The key-echo diagnostic drops into the emergency shell, so the
        // driver images are LEFT MOUNTED for inspection (FIX-55): they carry
        // no secrets, so leaving them mounted into a shell is safe.
        return BootOutcome::Done(Box::new(
            run_key_echo_diagnostic(config, console, &sender).await,
        ));
    }
    let session = SessionInteraction::new();
    let action = run_tui_session(&mut config, console, &session, &sender, &mut driver_images).await;
    teardown_driver_images_if_normal(&action, &driver_images);
    BootOutcome::Done(Box::new(Ok(action)))
}

/// Whether the driver images should be torn down for `action` (FIX-55).
///
/// `true` for every terminus that reboots or hands the machine off
/// (`Kexec` normal boot, `Reboot`, `RebootIntoRescue`, `HaltWithBanner`) so
/// the loop devices + mounts do not leak across the cutover. `false` ONLY for
/// the capped emergency shell ([`TerminalAction::Execve`]): the images are
/// deliberately LEFT MOUNTED there so the operator can inspect them, which is
/// safe because driver images carry no secret material.
pub(crate) fn should_teardown_driver_images(action: &TerminalAction) -> bool {
    !matches!(action, TerminalAction::Execve { .. })
}

/// Tear down the loaded driver images on the NORMAL pre-kexec path, but LEAVE
/// them mounted when the boot diverted into the capped emergency shell
/// ([`TerminalAction::Execve`]) — FIX-55.
fn teardown_driver_images_if_normal(action: &TerminalAction, handle: &DriverImagesHandle) {
    if handle.is_empty() {
        return;
    }
    if !should_teardown_driver_images(action) {
        nmbl_info!(
            "driver images: leaving {} image(s) mounted for the emergency shell (no secrets — FIX-55)",
            handle.len()
        );
        return;
    }
    if let Err(err) = detach_all_driver_images(handle) {
        nmbl_warn!("driver-image teardown reported an error (continuing): {err}");
    }
}
