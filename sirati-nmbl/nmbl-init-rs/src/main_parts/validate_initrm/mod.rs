//! `--validate-initrm` driver: dry-run the REAL boot control flow ×4
//! scenarios against an extracted-initrd closure under the side-effect-free
//! `DryRunSys`, collect "missing file" findings, and (optionally)
//! structurally validate the efi-stub UKI.
//!
//! This is the bin-side driver because it calls the bin-internal
//! orchestration (`run_phases_post_console`, `select_and_act`) reachable
//! from a sibling `main_parts` child; it also reaches the lib-public
//! `drop_to_emergency`, `DryRunSys`, and `sys::uki::validate_uki`.
//!
//! The four scenarios (`NormalBoot`, `ErrorToErrorScreen`, `PrettyShell`,
//! `RawShell`) each run once over a FRESH `DryRunSys` on the SAME closure;
//! their findings are merged + de-duplicated into one [`InitrmReport`]
//! (see `report`). The mode lists every finding and exits non-zero when the
//! report is not clean — mirroring the `validate_hardware` collect-all
//! style. See `scenarios` for how each path stays headless and
//! side-effect-free.

mod report;
mod scenarios;
mod scripted_console;
mod scripted_supplier;

use std::path::Path;

use nmbl_init::config::Config;
use nmbl_init::sys::uki::validate_uki;

use report::InitrmReport;

/// Drive the four dry-run scenarios over `closure_root`, optionally merge a
/// UKI structural check, and return the aggregated report.
///
/// `closure_root` is the extracted-initrd directory (build/sandbox mode) or
/// `/` (validate against the live initramfs). `uki`, when `Some`, is the
/// efi-stub UKI image to structurally validate.
///
/// Performs ZERO real side effects: every scenario runs under `DryRunSys`
/// (mounts/forks/kexec are no-op'd) and every returned `TerminalAction` is
/// swallowed — the mode is sandbox-safe (like `--validate-config`), touching
/// only `closure_root`, the config, and the passed UKI path.
pub(super) fn validate_initrm(
    config: &Config,
    uki: Option<&Path>,
    closure_root: &Path,
) -> InitrmReport {
    let mut report = InitrmReport::new();
    report.mark_ran();

    report.add_scenario(
        "NormalBoot",
        &scenarios::run_normal_boot(config, closure_root),
    );
    report.add_scenario(
        "ErrorToErrorScreen",
        &scenarios::run_error_screen(config, closure_root),
    );
    #[cfg(feature = "pretty-shell")]
    report.add_scenario(
        "PrettyShell",
        &scenarios::run_pretty_shell_scenario(config, closure_root),
    );
    report.add_scenario(
        "RawShell",
        &scenarios::run_raw_shell_scenario(config, closure_root),
    );

    if let Some(uki_path) = uki {
        // expected_cmdline is `None`: deriving the default-generation
        // cmdline here would mean scanning generations and re-running
        // `boot::build_cmdline` (private), duplicating boot logic for a
        // value the nix gate supplies authoritatively in a later phase. We
        // do structural UKI validation only; the cmdline-match expectation
        // comes from the nix derivation that wires this mode in.
        match validate_uki(uki_path, None) {
            Ok(findings) => report.add_uki(findings),
            Err(err) => report.add_uki(vec![nmbl_init::sys::uki::UkiFinding {
                kind: nmbl_init::sys::uki::UkiFindingKind::ParseError,
                detail: format!("cannot read UKI {}: {err}", uki_path.display()),
            }]),
        }
    }

    report
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
mod tests;
