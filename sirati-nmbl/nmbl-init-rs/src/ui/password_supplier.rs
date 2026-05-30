//! [`TuiPasswordSupplier`] — wires the passphrase modal into the
//! activation runner as a [`crate::activation::PasswordSupplier`].

use zeroize::Zeroizing;

use crate::activation::PasswordSupplier;
use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::generations::Generation;
use crate::ui::POLL_SLICE;
use crate::ui::app::{App, Decision, Screen, SessionInteraction};
use crate::ui::console::Console;

/// PasswordSupplier impl that pops a passphrase modal on the live
/// boot console (splash framebuffer or raw-mode tty — including a
/// serial UART, which is just another tty character device).
///
/// Does NOT open its own console. The orchestrator (main.rs) brings
/// up exactly one `Console` for the whole boot and passes it through
/// the activation runner; the supplier reuses that handle so the
/// passphrase modal renders on the same backend as the surrounding
/// boot-status screen.
#[derive(Default)]
pub struct TuiPasswordSupplier {
    /// Shared per-boot interaction latch. The passphrase modal joins
    /// this session so a typed passphrase counts as "operator present"
    /// for the emergency screen's countdown decision.
    session: SessionInteraction,
}

impl TuiPasswordSupplier {
    #[must_use]
    pub fn new(_config: &Config, session: &SessionInteraction) -> Self {
        // `_config` is accepted for forward-compatibility with
        // future per-config passphrase policy (retry counts, masking
        // toggles, …). Today the supplier is uniform — the same
        // ratatui modal everywhere.
        Self {
            session: session.clone(),
        }
    }
}

impl PasswordSupplier for TuiPasswordSupplier {
    fn prompt<'a>(
        &'a mut self,
        console: &'a mut dyn Console,
        label: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Zeroizing<String>>> + 'a>> {
        // Fully async: the activation runner now drives this future
        // inside the single interactive runtime, so we just await the
        // passphrase modal directly — no nested runtime.
        Box::pin(passphrase_prompt_on_console(console, label, &self.session))
    }
}

/// Drive the [`Screen::Passphrase`] modal on the supplied [`Console`]
/// until the operator submits (Enter) or cancels (Esc).
///
/// Esc translates to a [`NmblError::Tui`] so the caller can drop to
/// the emergency shell. The Console is reused, NOT re-opened — the
/// orchestrator already brought up the splash framebuffer or raw-mode
/// tty before phase 1 and held it through every phase.
pub(crate) async fn passphrase_prompt_on_console(
    console: &mut dyn Console,
    label: &str,
    session: &SessionInteraction,
) -> Result<Zeroizing<String>> {
    // No generations to render — pass an empty slice. The App is
    // only used here for its Passphrase screen state.
    let empty: [Generation; 0] = [];
    let mut app = App::new_in_session(&empty, session);
    app.screen = Screen::Passphrase {
        prompt_label: label.to_string(),
        buffer: Zeroizing::new(String::new()),
        cursor: 0,
        verifying: false,
        spinner_frame: 0,
    };

    let mut dirty = true;
    loop {
        // Poll the live Caps-Lock state every tick so the warning row
        // appears / disappears as the operator toggles the key. `None`
        // (serial line, no VT) degrades to "off" — the warning simply
        // never shows. Redraw whenever the state flips so the change is
        // visible without waiting for the next keystroke.
        let caps = console.caps_lock_active().unwrap_or(false);
        if caps != app.caps_lock_warning {
            app.caps_lock_warning = caps;
            dirty = true;
        }

        if dirty {
            console.render(&app)?;
            dirty = false;
        }

        // Drive `poll_event` so host-reported `CSI 8;rows;cols t`
        // resizes redraw the modal at the new dimensions instead of
        // smearing the old layout until the next keypress.
        match console.poll_event(POLL_SLICE).await? {
            Some(crate::ui::console::ConsoleEvent::Resize { .. }) => {
                dirty = true;
            }
            Some(crate::ui::console::ConsoleEvent::Key(key)) => {
                let exited = app.on_key(key);
                // Esc on the passphrase screen sets a Shell decision.
                if matches!(app.decision, Some(Decision::Shell)) {
                    return Err(NmblError::Tui {
                        source: std::io::Error::other("operator cancelled passphrase entry"),
                    });
                }
                if exited {
                    // Enter was pressed — extract the buffer and return.
                    // Silently ignore Enter while the buffer is empty so
                    // an accidental keystroke doesn't submit "" to
                    // cryptsetup.
                    if let Screen::Passphrase { ref buffer, .. } = app.screen
                        && buffer.is_empty()
                    {
                        continue;
                    }
                    if let Screen::Passphrase { buffer, .. } = app.screen {
                        return Ok(buffer);
                    }
                    return Err(NmblError::Tui {
                        source: std::io::Error::other("passphrase screen exited without a buffer"),
                    });
                }
                dirty = true;
            }
            // No scrollback on the passphrase modal; ignore wheel notches.
            // `UserHasInteracted` is informational only — the real key
            // that triggered it follows and is typed into the buffer.
            Some(
                crate::ui::console::ConsoleEvent::Scroll { .. }
                | crate::ui::console::ConsoleEvent::UserHasInteracted,
            )
            | None => {}
        }
    }
}
