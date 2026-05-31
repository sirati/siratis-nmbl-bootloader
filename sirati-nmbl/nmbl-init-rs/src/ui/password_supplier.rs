//! [`TuiPasswordSupplier`] — wires the passphrase modal into the
//! activation runner as a [`crate::activation::PasswordSupplier`].

use zeroize::Zeroizing;

use crate::activation::PasswordSupplier;
use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::generations::Generation;
use crate::ui::POLL_SLICE;
use crate::ui::app::{App, Decision, Screen, SessionInteraction, SkipSelector};
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
    /// Shared "skip the generation selector" latch, set at passphrase
    /// submit from the modal's "Select NixOS Generation" checkbox.
    /// Unchecked (default) ⇒ `true` (skip → boot default gen); checked
    /// ⇒ `false` (show the selector, today's behaviour). Read by the
    /// post-phase selector dispatch in `main_parts`.
    skip_selector: SkipSelector,
}

impl TuiPasswordSupplier {
    #[must_use]
    pub fn new(
        _config: &Config,
        session: &SessionInteraction,
        skip_selector: &SkipSelector,
    ) -> Self {
        // `_config` is accepted for forward-compatibility with
        // future per-config passphrase policy (retry counts, masking
        // toggles, …). Today the supplier is uniform — the same
        // ratatui modal everywhere.
        Self {
            session: session.clone(),
            skip_selector: skip_selector.clone(),
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
        Box::pin(passphrase_prompt_on_console(
            console,
            label,
            &self.session,
            &self.skip_selector,
        ))
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
    skip_selector: &SkipSelector,
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
        // Checkbox starts UNCHECKED: a plain unlock skips the selector
        // and boots the default generation. Ctrl+G flips it.
        select_generation: false,
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
        //
        // Floor the idle cadence with a guaranteed `POLL_SLICE` sleep
        // (mirroring `devices::wait_for`): a real tty's `poll_event`
        // honours the timeout so this floor is a no-op there, but a
        // console whose `poll_event` returns instantly (e.g.
        // `NoopConsole`, which ignores the timeout) would otherwise
        // busy-spin this loop at 100% CPU and starve the single-threaded
        // runtime. Race the floor against `poll_event`; on an actionable
        // event we take it immediately, and on no event we honour the
        // rest of the floor before looping. `floor` is pinned OUTSIDE
        // the select and only `&mut`-borrowed, so an early `return` never
        // drops it; `poll_event` is cancel-safe.
        let mut floor = std::pin::pin!(tokio::time::sleep(POLL_SLICE));
        let event = tokio::select! {
            event = console.poll_event(POLL_SLICE) => {
                let event = event?;
                // poll_event resolved before the floor elapsed (an
                // instant-returning console); honour the rest of the
                // floor so the loop never spins faster than the cadence.
                if event.is_none() {
                    floor.await;
                }
                event
            }
            () = &mut floor => {
                // Floor elapsed before poll_event produced anything;
                // loop and re-render (caps-lock / resize repaint).
                None
            }
        };
        match event {
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
                    if let Screen::Passphrase {
                        buffer,
                        select_generation,
                        ..
                    } = app.screen
                    {
                        // Record the checkbox into the shared latch the
                        // post-phase dispatch reads: UNCHECKED ⇒ skip the
                        // selector and boot the default gen; CHECKED ⇒
                        // show the selector (today's behaviour). A
                        // wrong-password re-prompt re-runs this fn, so the
                        // last successful submit's checkbox value wins.
                        skip_selector.set(!select_generation);
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
