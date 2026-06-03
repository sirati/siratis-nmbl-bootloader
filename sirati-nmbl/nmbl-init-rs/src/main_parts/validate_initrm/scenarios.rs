//! The four `--validate-initrm` scenario runners.
//!
//! Each runs the GENUINE boot control flow once against a fresh
//! [`DryRunSys`] over the same closure, inside the same
//! [`block_on_tui_with_poller`] runtime the real boot uses (so the async
//! ops have a real poller `sender` — the dry-run never forks, so the
//! waitpid consumer never fires, but the seam is identical). It then
//! returns every [`MissingFile`] the dry-run recorded.
//!
//! ## Side-effect freedom and the shell scenarios
//!
//! No scenario performs a real side effect: the dry-run ops no-op every
//! mount/fork/kexec, and the drivers SWALLOW any returned
//! [`TerminalAction`] — we never call `execute_terminal_action`, `reboot`,
//! `execve`, or `kexec::execute`.
//!
//! `NormalBoot` and `ErrorToErrorScreen` drive the REAL bin orchestration
//! (`run_phases_post_console` / `select_and_act`) and the REAL emergency
//! menu (`drop_to_emergency`) over the `DryRunSys` / a `NoopConsole`. The
//! emergency menu's no-input countdown rolls over to `Reboot` (a pure
//! choice that touches no ops), so no shell is ever forked.
//!
//! The PrettyShell / RawShell scenarios CANNOT be driven through the real
//! `drop_to_emergency` menu: `shell::dispatch_emergency_choice` hardcodes a
//! `RealSys` for the shell-spawn dispatch (it is not generic over
//! `SysOps`), so selecting the shell button there would perform a genuine
//! `fork`/`execve`. Driving the picker (`run_picker_session`) is likewise
//! unsafe — its fire-and-forget arm forks a real shell and its
//! display-overlap decision reads sysfs. So we take the task's documented
//! sound fallback: invoke the lib-public shell entries directly against the
//! `DryRunSys`, which routes the spawn through `DryRunSys::spawn_shell` →
//! `DryRunShellPreflight` (the preflight findings are recorded and the
//! typed signal is treated as success). PrettyShell uses the real
//! `run_pretty_shell` driver (which reaches `spawn_shell` as its first op,
//! exercising the on-demand grid sizing); RawShell drives the same
//! `ExecOps::spawn_shell` convergence point both shell paths share.

use nmbl_init::config::Config;
use nmbl_init::error::NmblError;
use nmbl_init::imageload::DriverImagesHandle;
use nmbl_init::shell::drop_to_emergency;
use nmbl_init::sys::ops::ExecOps;
use nmbl_init::sys::ops::dryrun::{ClosureView, DryRunScenario, DryRunSys, MissingFile};
use nmbl_init::ui::block_on_tui_with_poller;
use nmbl_init::ui::console::NoopConsole;
use nmbl_init::ui::{SessionInteraction, SkipSelector};

use super::super::dispatch::select_and_act;
use super::super::phases::run_phases_post_console;
use super::scripted_console::ScriptedConsole;
use super::scripted_supplier::ScriptedPasswordSupplier;

use crossterm::event::KeyCode;

/// Grid the shell-spawn preflight is sized for in the dry run. Any sane
/// non-zero pair works — the dry-run never opens a PTY — so we use the
/// `NoopConsole` default the real backends fall back to.
const DRYRUN_SHELL_COLS: u16 = 80;
const DRYRUN_SHELL_ROWS: u16 = 24;

/// Run the `NormalBoot` scenario: skip the selector (headless default-boot
/// decision) and walk modules → activations → mount → generation scan →
/// kexec, all no-op'd. Returns every recorded finding.
///
/// `select_and_act` returns a [`TerminalAction`] (typically `Kexec`); it is
/// SWALLOWED. An `Err` (e.g. `scan_generations` finding no profiles dir in
/// a synthetic closure) is informational, not a crash — the valuable phase
/// 2b/3/3b file-dep coverage already ran by then.
pub(super) fn run_normal_boot(config: &Config, closure_root: &std::path::Path) -> Vec<MissingFile> {
    let closure = ClosureView::new(closure_root.to_path_buf());
    let mut dryrun = DryRunSys::new(closure, DryRunScenario::NormalBoot);
    // Owned config: `run_phases_post_console` takes `&mut Config` (the
    // staged-boot merge path); the dry-run never enables staged-boot, so the
    // config is only ever read, but the signature requires `&mut`.
    let mut config: Config = config.clone();
    // A successful (dry-run exit-0) `luks-tpm` activation registers its mapper,
    // which would otherwise append to the REAL `/run/nmbl` registry file. Hold
    // the dry-run seal scope for the whole scenario so the registry's on-disk
    // persistence is suppressed (the mapper close it pairs with is no-op'd too).
    let seal_scope = nmbl_init::policy::DryRunSealScope::enter();
    let result = block_on_tui_with_poller(|sender| async move {
        let session = SessionInteraction::new();
        let skip_selector = SkipSelector::new();
        // Headless: take the default-boot decision path, no keypress.
        skip_selector.set(true);
        let mut console = NoopConsole::new();
        // No driver images in the dry run — the gate hooks are inert when
        // signing is disabled (the validate-initrm config posture).
        let mut driver_images = DriverImagesHandle::empty();
        // Headless supplier: returns a placeholder passphrase instantly,
        // never driving the NoopConsole's timeout-ignoring poll loop.
        let mut supplier = ScriptedPasswordSupplier;
        if let Ok(injections) = run_phases_post_console(
            &mut dryrun,
            &mut config,
            &mut console,
            &mut supplier,
            &session,
            &skip_selector,
            &sender,
            &mut driver_images,
        )
        .await
        {
            // SWALLOW the TerminalAction — never execute it.
            let _ = select_and_act(
                &mut dryrun,
                &config,
                &mut console,
                &injections,
                &session,
                &skip_selector,
                &driver_images,
            )
            .await;
        }
        dryrun.into_findings().items().to_vec()
    });
    // Restore the real seal/registry path for any later caller on this thread.
    drop(seal_scope);
    findings_or_runtime_note(result, "NormalBoot")
}

/// Run the `ErrorToErrorScreen` scenario: the dry-run fails an activation
/// so `run_phases_post_console` returns `Err`, then drive the REAL
/// `drop_to_emergency` menu to exercise the emergency-screen bring-up + menu
/// render. A `ScriptedConsole` feeds a single `Enter`, which selects the
/// default menu item (index 0 = `Reboot`, a pure op-free choice) so the menu
/// exits at once instead of blocking on its 30 s auto-reboot countdown. The
/// returned [`TerminalAction`] is SWALLOWED — never executed.
pub(super) fn run_error_screen(
    config: &Config,
    closure_root: &std::path::Path,
) -> Vec<MissingFile> {
    let closure = ClosureView::new(closure_root.to_path_buf());
    let mut dryrun = DryRunSys::new(closure, DryRunScenario::ErrorToErrorScreen);
    // Owned config: see `run_normal_boot` — the post-console phase takes
    // `&mut Config`; the dry-run only reads it.
    let mut config: Config = config.clone();
    let result = block_on_tui_with_poller(|sender| async move {
        let session = SessionInteraction::new();
        let skip_selector = SkipSelector::new();
        let mut driver_images = DriverImagesHandle::empty();
        let mut noop = NoopConsole::new();
        let mut supplier = ScriptedPasswordSupplier;
        let outcome = run_phases_post_console(
            &mut dryrun,
            &mut config,
            &mut noop,
            &mut supplier,
            &session,
            &skip_selector,
            &sender,
            &mut driver_images,
        )
        .await;
        // The scenario scripts an activation failure, so we expect Err;
        // either way, route to the emergency screen to exercise it.
        let err = outcome.err().unwrap_or_else(|| NmblError::Io {
            source: std::io::Error::other("validate-initrm error-screen scenario"),
            context: "validate-initrm".to_string(),
        });
        // Enter selects the default Reboot item, exiting the menu loop
        // immediately. Reboot touches no ops, so nothing forks. SWALLOW.
        let console = ScriptedConsole::from_keys([KeyCode::Enter]);
        // Property-6: `drop_to_emergency` runs the GENUINE
        // `policy::seal_secrets`, which would otherwise cap the REAL lock PCR
        // (irreversible poison) and run `cryptsetup close` on a TPM host. The
        // dry-run seal scope routes the cap through the `DryRunSys` `TpmOps`
        // no-op and suppresses the close for the duration of this run, so the
        // emergency-screen path is exercised side-effect-free. The scope drops
        // before the closure returns, restoring the real seal for any later
        // caller on this thread.
        let seal_scope = nmbl_init::policy::DryRunSealScope::enter();
        let _ = drop_to_emergency(Box::new(console), &config, err, &session, &sender).await;
        drop(seal_scope);
        dryrun.into_findings().items().to_vec()
    });
    findings_or_runtime_note(result, "ErrorToErrorScreen")
}

/// Run the `PrettyShell` scenario via the REAL `run_pretty_shell` driver
/// against the `DryRunSys`. The driver's first op is `spawn_shell`, which
/// the dry-run answers with the preflight check + `DryRunShellPreflight`
/// signal; that propagates straight back out of the driver, so no terminal
/// emulator loop ever runs. Returns the recorded shell-spawn preflight
/// findings. See the module docs for why the real menu can't be used here.
#[cfg(feature = "pretty-shell")]
pub(super) fn run_pretty_shell_scenario(
    config: &Config,
    closure_root: &std::path::Path,
) -> Vec<MissingFile> {
    let closure = ClosureView::new(closure_root.to_path_buf());
    let mut dryrun = DryRunSys::new(closure, DryRunScenario::PrettyShell);
    let result = block_on_tui_with_poller(|_sender| async move {
        let mut console = NoopConsole::new();
        // The shell-spawn seam demands a `Sealed` witness by type (C-1). The
        // dry-run's `DryRunSys::spawn_shell` never reaches the real fork, so
        // the witness gates no syscall here; mint the dry-run-only witness
        // rather than running the real seal (which would attempt a cryptsetup
        // close side effect that `--validate-initrm` must never perform).
        let sealed = nmbl_init::policy::Sealed::dry_run_witness();
        // `DryRunShellPreflight` is the expected, success-equivalent error
        // (the preflight ran, no fork happened); any other error is left to
        // the report as the surfaced finding-or-note. SWALLOW the result.
        let _ = nmbl_init::ui::pretty_shell::run_pretty_shell(
            sealed,
            &mut dryrun,
            &mut console,
            config,
        )
        .await;
        dryrun.into_findings().items().to_vec()
    });
    findings_or_runtime_note(result, "PrettyShell")
}

/// Run the `RawShell` scenario by driving the `ExecOps::spawn_shell`
/// convergence point both raw- and pretty-shell paths share, against the
/// `DryRunSys`. The dry-run records the shell-spawn preflight findings and
/// returns `DryRunShellPreflight`; treated as success. See the module docs
/// for why the picker session can't be driven here.
pub(super) fn run_raw_shell_scenario(
    config: &Config,
    closure_root: &std::path::Path,
) -> Vec<MissingFile> {
    let closure = ClosureView::new(closure_root.to_path_buf());
    let mut dryrun = DryRunSys::new(closure, DryRunScenario::RawShell);
    // The shell-spawn seam demands a `Sealed` witness by type (C-1). The
    // dry-run's `DryRunSys::spawn_shell` never reaches the real fork, so the
    // witness gates no syscall here; mint the dry-run-only witness rather than
    // running the real seal (no cryptsetup-close side effect). `spawn_shell` is
    // synchronous; no runtime needed.
    let sealed = nmbl_init::policy::Sealed::dry_run_witness();
    let _ = dryrun.spawn_shell(
        sealed,
        &config.paths.shell,
        DRYRUN_SHELL_COLS,
        DRYRUN_SHELL_ROWS,
    );
    dryrun.into_findings().items().to_vec()
}

/// Unwrap a scenario's runtime result. A runtime-build failure is itself a
/// finding (the sandbox could not stand up the `LocalRuntime`); surface it
/// as a synthetic `MissingFile` so it shows in the report rather than being
/// silently dropped.
fn findings_or_runtime_note(
    result: nmbl_init::error::Result<Vec<MissingFile>>,
    scenario: &'static str,
) -> Vec<MissingFile> {
    match result {
        Ok(findings) => findings,
        Err(err) => vec![MissingFile::new(
            "runtime",
            std::path::Path::new("(runtime)"),
            format!("{scenario}: dry-run runtime build failed: {err}"),
        )],
    }
}
