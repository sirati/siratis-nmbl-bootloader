//! Builder helpers for the emergency screen.
//!
//! These pure functions construct the message string, item list, and
//! `App` that `drive_emergency_loop` operates on.

use crate::error::{NmblError, format_chain};
use crate::ui::app::{App, EmergencyChoice, EmergencyItem, Screen, SessionInteraction};

/// Build the message string shown to the operator. Includes the
/// suggested-action hint plus the formatted error chain.
pub(crate) fn build_message(err: &NmblError) -> String {
    let mut s = String::new();
    s.push_str("Boot failed. The chain of errors is:\n\n");
    s.push_str(&format_chain(err as &dyn std::error::Error));
    s.push_str("\n\nChoose what to do next.");
    s
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
