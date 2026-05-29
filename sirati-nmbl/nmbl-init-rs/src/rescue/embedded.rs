//! Embedded-shell and halt-with-banner rescue actions.

use std::ffi::CString;

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::terminal::TerminalAction;

/// Build a [`TerminalAction::Execve`] for the operator-configured
/// shell (`cfg.paths.shell`) with an empty environment. Mirrors the
/// pre-refactor `exec_embedded` body byte-for-byte in terms of which
/// argv/env it constructs — the only difference is that the syscall
/// itself is deferred to the dispatcher in `main`.
///
/// The `cause` is moved into the [`crate::terminal::EmergencyBanner`]
/// so the dispatcher can render the operator-facing banner
/// immediately before the execve.
///
/// Returns `Err(NmblError::Rescue { stage, ... })` on the rare path
/// where the configured shell path or argv contains an interior NUL
/// — execve cannot proceed and the caller halts with a banner.
pub fn exec_embedded(config: &Config, cause: NmblError) -> Result<TerminalAction> {
    let shell_path = config.paths.shell.as_path();
    let argv0_bytes: Vec<u8> = shell_path
        .file_name()
        .map(|n| n.as_encoded_bytes().to_vec())
        .unwrap_or_else(|| shell_path.as_os_str().as_encoded_bytes().to_vec());

    // Interior NUL in a config-supplied path is astronomically unlikely
    // but still has to be handled. Surface as Rescue{stage:"shell-path-nul"}
    // so the banner makes the failure mode obvious.
    let path_c =
        CString::new(shell_path.as_os_str().as_encoded_bytes()).map_err(|_| NmblError::Rescue {
            stage: "shell-path-nul",
            source: Box::new(NmblError::ConfigInvalid {
                reason: "shell path contains interior NUL".to_string(),
                context: format!("preparing execve of {}", shell_path.display()),
            }),
        })?;
    let argv0_c = CString::new(argv0_bytes).map_err(|_| NmblError::Rescue {
        stage: "shell-argv0-nul",
        source: Box::new(NmblError::ConfigInvalid {
            reason: "shell argv0 contains interior NUL".to_string(),
            context: format!("preparing execve of {}", shell_path.display()),
        }),
    })?;

    Ok(TerminalAction::Execve {
        path: path_c,
        argv: vec![argv0_c],
        env: Vec::new(),
        banner: Some(crate::terminal::EmergencyBanner::new(config, cause)),
        rescue_handoff: true,
    })
}

/// Build a [`TerminalAction::HaltWithBanner`] for the no-rescue path.
/// Used for [`RescueMode::None`] installs where no toolkit ships and
/// the kindest UX is to stop — rather than leave the operator at an
/// inert PID 1.
///
/// The banner text is rendered by the dispatcher; this constructor
/// only packages the cause so every halt-with-banner producer goes
/// through the same code path.
pub fn halt_with_banner(cause: NmblError) -> TerminalAction {
    TerminalAction::HaltWithBanner { cause }
}
