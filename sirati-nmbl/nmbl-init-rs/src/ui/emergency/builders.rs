//! Builder helpers for the emergency screen.
//!
//! These pure functions construct the message string, item list, and
//! `App` that `drive_emergency_loop` operates on.

use crate::error::{NmblError, format_chain};
use crate::ui::app::{App, EmergencyChoice, EmergencyItem, Screen, SessionInteraction};

/// Build the message string shown to the operator.
///
/// Layout, top to bottom:
///   1. A one-line plain-language "Likely cause" hint when the error
///      pattern is recognisable (e.g. a vanished boot device), so the
///      operator gets a diagnosis before the raw chain.
///   2. The full formatted error chain (operation, device path, and the
///      underlying errno text are already carried by the `NmblError`
///      variants and `format_chain`, so nothing is lost).
///   3. A "choose what to do next" prompt.
pub(crate) fn build_message(err: &NmblError) -> String {
    let mut s = String::new();
    if let Some(cause) = likely_cause(err) {
        s.push_str("Likely cause: ");
        s.push_str(cause);
        s.push_str("\n\n");
    }
    s.push_str("Boot failed. The chain of errors is:\n\n");
    s.push_str(&format_chain(err as &dyn std::error::Error));
    s.push_str("\n\nChoose what to do next.");
    s
}

/// Best-effort plain-language diagnosis of a boot failure, recursing
/// into the typed wrapper variants (`Activation` / `Bootstrap` /
/// `Rescue`) so a cause nested inside one — the real shape when a
/// LUKS-produced dm node never appears — is still recognised.
///
/// Returns `None` when nothing specific can be said — the raw chain
/// alone is then shown. Kept deliberately conservative: only patterns
/// with a clear operator-actionable meaning get a hint, so the line
/// stays trustworthy rather than guessing.
pub(crate) fn likely_cause(err: &NmblError) -> Option<&'static str> {
    // Unwrap the typed wrappers explicitly (they hold `Box<NmblError>`,
    // so we keep full type information rather than downcasting a
    // `dyn Error`). Recurse innermost-first so the most specific cause
    // wins over a generic wrapper.
    let inner = match err {
        NmblError::Activation { source, .. }
        | NmblError::Bootstrap { source, .. }
        | NmblError::Rescue { source, .. } => Some(source.as_ref()),
        _ => None,
    };
    if let Some(inner) = inner
        && let Some(hint) = likely_cause(inner)
    {
        return Some(hint);
    }
    cause_for(err)
}

/// One-line cause for a single `NmblError` variant, or `None` if the
/// variant carries no clearer-than-the-chain meaning.
fn cause_for(err: &NmblError) -> Option<&'static str> {
    match err {
        // A device the kernel had, then lost: the canonical "operator
        // yanked the boot USB / a drive dropped off the bus" failure.
        NmblError::DeviceTimeout { .. } => Some(
            "the expected block device never appeared — the boot device may have been \
             unplugged, or a disk/USB key is not seated. Re-seat it and retry boot.",
        ),
        // mount(2) failed: usually the filesystem isn't there or the
        // device vanished between activation and mount.
        NmblError::Mount { .. } => Some(
            "the filesystem could not be mounted — the device may have disappeared or holds \
             no/incompatible filesystem.",
        ),
        NmblError::SystemRootNotMounted { .. } | NmblError::ProfilesDirMissing { .. } => Some(
            "the NixOS system root is not mounted (or is the wrong filesystem). Mount it at \
             the expected path from a shell, then retry boot.",
        ),
        NmblError::NoGenerations { .. } => {
            Some("no bootable NixOS generations were found on the mounted system root.")
        }
        // The operator cut a wait short themselves — say so plainly.
        NmblError::OperatorAborted { .. } => {
            Some("you aborted a wait. Retry boot once the device is ready, or open a shell.")
        }
        _ => None,
    }
}

/// Items shown on the emergency screen. Order matters: index 0 is the
/// default if the operator just presses Enter, and it's what the
/// timeout rolls over to.
///
/// `Pretty Shell` is inserted between `Reboot` and `Raw Shell` only
/// when the `pretty-shell` Cargo feature is compiled in — it depends
/// on the `alacritty_terminal` parser which is only an optional dep of
/// that feature. `pretty-shell` is default-on (and also pulled in by
/// `image-splash`), so the normal build shows it; a
/// `--no-default-features` build hides it. When the feature is on
/// Pretty Shell is the preferred recovery shell; the raw busybox-on-tty
/// path sits below it as a fallback. The `Retry boot from config` and
/// `Verify kexec readiness` actions are unconditional: they only need
/// the existing phase 3/4/5 plumbing already in the binary.
pub(crate) fn default_items() -> Vec<EmergencyItem> {
    // `mut` is conditionally used (the `insert` below is feature-gated);
    // suppress the unused_mut warning on no-feature builds without
    // duplicating the vec literal.
    #[cfg_attr(not(feature = "pretty-shell"), allow(unused_mut))]
    let mut items = vec![EmergencyItem {
        label: "Reboot",
        choice: EmergencyChoice::Reboot,
    }];
    #[cfg(feature = "pretty-shell")]
    items.push(EmergencyItem {
        label: "Pretty Shell",
        choice: EmergencyChoice::PrettyShell,
    });
    items.push(EmergencyItem {
        label: "Raw Shell",
        choice: EmergencyChoice::RawShell,
    });
    items.push(EmergencyItem {
        label: "Retry boot from config",
        choice: EmergencyChoice::RetryBoot,
    });
    items.push(EmergencyItem {
        label: "Verify kexec readiness",
        choice: EmergencyChoice::VerifyKexecReadiness,
    });
    items
}

/// Build an `App` parked on the Emergency screen with the given
/// message and items.
pub(crate) fn build_emergency_app<'a>(
    message: &str,
    items_template: &[EmergencyItem],
    session: &SessionInteraction,
) -> App<'a> {
    // Items are tiny, no point fighting the borrow checker — clone
    // the template into the App's own Screen state.
    let items: Vec<EmergencyItem> = items_template
        .iter()
        .map(|it| EmergencyItem {
            label: it.label,
            choice: it.choice,
        })
        .collect();
    let mut app = App::new_in_session(&[], session);
    app.screen = Screen::Emergency {
        message: message.to_owned(),
        items,
        selected: 0,
        chosen: None,
    };
    app
}
