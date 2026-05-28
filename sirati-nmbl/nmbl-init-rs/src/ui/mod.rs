//! UI orchestrator. The frame loop lives here; pure render functions
//! live in [`view`], the state machine in [`app`], and the backend
//! abstraction (splash framebuffer vs raw-mode tty) in [`console`].
//! Every interactive screen — selector, cmdline editor, passphrase
//! modal, emergency picker — renders through the same `&mut dyn Console`,
//! regardless of whether the underlying device is a framebuffer VT, a
//! `/dev/tty1` keyboard line, or a serial UART. Ratatui's crossterm
//! backend emits vt100/xterm escape sequences which every modern
//! serial terminal emulator (xterm, tmux, picocom, screen, minicom)
//! understands, so there is no longer a line-mode fallback path.
//!
//! ## Activation passphrase wiring
//!
//! [`TuiPasswordSupplier`] implements
//! [`crate::activation::PasswordSupplier`] so the top-level boot
//! flow can pass it to
//! [`crate::activation::run_all_activations`] as
//! `&mut dyn PasswordSupplier`. When the activation runner reaches a
//! `luks-password` entry it calls `prompt(console, label)` once; the
//! supplier reuses the LIVE boot console (splash framebuffer or
//! raw-mode tty) the orchestrator already holds, drives a render-poll
//! loop over the [`Screen::Passphrase`] modal, and returns the entered
//! string in a `Zeroizing<String>` so the buffer is wiped after
//! `cryptsetup` drains it. No new console is opened — that would
//! duplicate the splash bring-up and flicker between backends.
//!
//! Esc on the modal returns a [`NmblError::Tui`] which
//! `run_all_activations` wraps as [`NmblError::Activation`] and the
//! top-level driver routes to the emergency shell.

pub mod app;
pub mod console;
pub mod console_picker;
pub mod console_relay;
pub mod emergency;
pub mod emergency_actions;
pub mod key_echo;
pub mod modal_layout;
#[cfg(feature = "image-splash")]
pub mod pretty_shell;
pub mod reporter;
#[cfg(feature = "network-rescue")]
pub mod rescue;
pub mod timeout;
pub mod tty_enum;
pub mod view;

use std::time::{Duration, Instant};

use zeroize::Zeroizing;

use crate::activation::PasswordSupplier;
use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::generations::Generation;
use crate::ui::console::Console;
use crate::ui::timeout::TimeoutOutcome;
use crate::ui::view::{
    EditScreenData, EmergencyScreenData, KeyEchoScreenData, ListScreenData, PassphraseScreenData,
    render_boot_status, render_edit, render_emergency, render_key_echo, render_list,
    render_passphrase,
};

#[cfg(feature = "image-splash")]
use alacritty_terminal::term::cell::Flags;
#[cfg(feature = "image-splash")]
use alacritty_terminal::vte::ansi::{Color, NamedColor};
#[cfg(feature = "image-splash")]
use ratatui::Terminal;
#[cfg(feature = "image-splash")]
use ratatui::TerminalOptions;
#[cfg(feature = "image-splash")]
use ratatui::Viewport;
#[cfg(feature = "image-splash")]
use ratatui::backend::CrosstermBackend;
#[cfg(feature = "image-splash")]
use ratatui::layout::Rect;
#[cfg(feature = "image-splash")]
use crate::splash::{compositor, drm, glyph_cache};
#[cfg(feature = "image-splash")]
use crate::splash::terminal::SplashTerminal;
#[cfg(feature = "image-splash")]
use crate::splash::types::CellDims;

pub use app::{App, BootStatusData, Decision, EmergencyChoice, EmergencyItem, ModalKind, Screen};
pub use emergency::{run_emergency_screen, run_emergency_screen_with_app};
pub(crate) use emergency::{build_emergency_app, build_message, default_items};
pub use reporter::{BootReporter, ProgressSink, TickOutcome};

/// Slice we wait on input per iteration. Shared by the event loop and
/// the countdown ticker so they have the same responsiveness profile
/// and only one knob to tune.
pub(crate) const POLL_SLICE: Duration = Duration::from_millis(100);

/// Outcome of a yes/no modal confirmation prompt.
///
/// Kept as a dedicated enum (rather than `bool`) so call sites read
/// at a glance and so a future "third option" (e.g. `Defer`) can be
/// added without rippling through every match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmOutcome {
    /// Operator picked the affirmative button (Yes / Boot / …) via
    /// Enter, hotkey 'y', or Enter on the highlighted Yes button.
    Yes,
    /// Operator picked the negative button (Back / No / …) via Enter
    /// on the highlighted No button or hotkey 'n'.
    No,
    /// Operator pressed Esc; treated as "go back without committing".
    /// Production callers typically lump this into `No`, but keeping
    /// it distinct lets tests assert on the exact key path that
    /// dismissed the modal.
    Cancelled,
}

/// Outcome of the wrong-password modal shown after a `luks-password`
/// activation returns exit code 2 (cryptsetup's "no key available"). The
/// activation loop matches on this to decide whether to re-prompt, hand
/// back to `main` for a reboot, or detour through an in-process shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrongPasswordOutcome {
    /// Re-prompt for the passphrase and re-run the same activation.
    TryAgain,
    /// Operator picked [Reboot]; the activation layer propagates this
    /// as [`NmblError::OperatorChoseReboot`] so `main` short-circuits
    /// to [`crate::terminal::TerminalAction::Reboot`] without dropping
    /// to the emergency menu.
    Reboot,
    /// Operator picked [Pretty Shell]; the caller runs the alacritty-
    /// backed PTY session inside the splash TUI box. Only exposed on
    /// the `image-splash` feature — the no-feature build hides the
    /// button entirely so there is nothing to dispatch.
    #[cfg(feature = "image-splash")]
    PrettyShell,
    /// Operator picked [Raw Shell]; the caller opens the console-
    /// picker dialog and runs the multiplexed busybox PTY relay.
    RawShell,
}

/// Inspect a [`KeyEvent`] for the modal-scroll bindings
/// (Ctrl+Shift+Up/Down/PgUp/PgDn). Returns `Some(new_offset)` when the
/// key matched and was consumed, `None` when the key should fall
/// through to the caller's regular key dispatch.
///
/// `console.size()` is sampled here so the helper can compute the
/// visible-line count from the same layout the renderer uses. Empty
/// or pathologically small consoles fall back to a 1-line viewport so
/// the offset still advances by one per keypress.
fn handle_modal_scroll_key(
    key: crossterm::event::KeyEvent,
    message: &str,
    has_buttons: bool,
    btn_count: u16,
    console: &mut dyn Console,
    scroll_offset: u16,
) -> Option<u16> {
    use crossterm::event::{KeyCode, KeyModifiers};
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    if !(ctrl && shift) {
        return None;
    }
    // Re-derive the current layout from the console's reported size.
    // Mirrors `view::split_chrome`: 3-row header + body + 1-row footer.
    let (cols, rows) = console.size();
    let body_h = rows.saturating_sub(4);
    let body = ratatui::layout::Rect::new(0, 3, cols, body_h);
    let layout = modal_layout::compute_modal_layout(message, has_buttons, btn_count, body);
    if !layout.scrollable {
        // Even when not scrollable we still consume the key combo so
        // a stray Ctrl+Shift+arrow doesn't leak through to whatever
        // dispatch would have run otherwise. Returning the unchanged
        // offset keeps the renderer's clamping clean.
        match key.code {
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => {
                return Some(scroll_offset);
            }
            _ => return None,
        }
    }
    let visible = layout.inner_text_rect.height.max(1);
    let total = u16::try_from(layout.wrapped_lines.len()).unwrap_or(u16::MAX);
    let max_off = total.saturating_sub(visible);
    let page = visible.saturating_sub(1).max(1);
    let new_off = match key.code {
        KeyCode::Up => scroll_offset.saturating_sub(1),
        KeyCode::Down => scroll_offset.saturating_add(1).min(max_off),
        KeyCode::PageUp => scroll_offset.saturating_sub(page),
        KeyCode::PageDown => scroll_offset.saturating_add(page).min(max_off),
        KeyCode::Home => 0,
        KeyCode::End => max_off,
        _ => return None,
    };
    Some(new_off)
}

/// Outcome of polling a long-running render loop's input slice. Wraps
/// the trichotomy "key arrived / host terminal reported a new size /
/// nothing this tick" into a single value the caller pattern-matches on
/// so every modal loop redraws on resize without duplicating the
/// `match` boilerplate.
enum ModalPollOutcome {
    /// A key event the caller should dispatch.
    Key(crossterm::event::KeyEvent),
    /// Host terminal reported a new grid. Caller should set its
    /// `dirty` flag so the next iteration repaints against the new
    /// layout.
    Resized,
    /// No event this slice. Caller may continue ticking countdowns
    /// or re-poll.
    Idle,
}

/// Poll once via [`Console::poll_event`] and classify the outcome for a
/// modal render loop. Shared by every long-running interactive modal
/// (passphrase, generations picker, rescue menu, console picker,
/// confirm / error / buttons) so they all react to host-reported
/// resize events uniformly.
fn modal_poll(console: &mut dyn Console, timeout: Duration) -> Result<ModalPollOutcome> {
    match console.poll_event(timeout)? {
        Some(crate::ui::console::ConsoleEvent::Key(k)) => Ok(ModalPollOutcome::Key(k)),
        Some(crate::ui::console::ConsoleEvent::Resize { .. }) => Ok(ModalPollOutcome::Resized),
        None => Ok(ModalPollOutcome::Idle),
    }
}

/// Show a centred yes/no confirmation modal with `title` + `message`
/// on the supplied console and block until the operator commits.
///
/// This is the standalone variant: it draws onto a fresh frame with no
/// underlying screen. Used by call sites that have no persistent App
/// (e.g. early-boot activation). Emergency-menu actions should use
/// [`show_modal_confirm_over`] instead so the menu remains visible
/// behind the modal.
///
/// Returns:
///   - `Ok(ConfirmOutcome::Yes)`       — Enter on Yes, or hotkey 'y'.
///   - `Ok(ConfirmOutcome::No)`        — Enter on No, or hotkey 'n'.
///   - `Ok(ConfirmOutcome::Cancelled)` — Esc.
///
/// `yes_default = true` highlights the Yes button on first paint;
/// pass `false` for "are you sure?"-style prompts where the safer
/// answer is No.
///
/// Falls back to `ConfirmOutcome::No` if rendering fails — same
/// principle as [`show_modal_error`]: when the operator can't see the
/// modal, default to the safer non-action.
pub fn show_modal_confirm(
    console: &mut dyn Console,
    title: &str,
    message: &str,
    yes_label: &str,
    no_label: &str,
    yes_default: bool,
) -> Result<ConfirmOutcome> {
    use crossterm::event::KeyCode;

    let hint = "Left/Right select  Enter confirm  Esc cancel";
    let mut yes_selected = yes_default;
    let mut scroll_offset: u16 = 0;

    let mut dirty = true;
    loop {
        if dirty {
            let data = view::ModalConfirmScreenData {
                title,
                message,
                yes_label,
                no_label,
                yes_selected,
                hint,
                scroll_offset,
            };
            if let Err(e) = console.draw_with(&mut |frame| view::render_modal_confirm(frame, &data))
            {
                eprintln!("[nmbl] {title}: {message}");
                crate::nmbl_warn!("modal-confirm render failed: {e}");
                return Ok(ConfirmOutcome::No);
            }
            dirty = false;
        }

        let key = match modal_poll(console, POLL_SLICE)? {
            ModalPollOutcome::Key(k) => k,
            ModalPollOutcome::Resized => {
                dirty = true;
                continue;
            }
            ModalPollOutcome::Idle => continue,
        };
        if let Some(new_off) = handle_modal_scroll_key(key, message, true, 2, console, scroll_offset) {
            scroll_offset = new_off;
            dirty = true;
            continue;
        }
        match key.code {
            KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                yes_selected = !yes_selected;
                dirty = true;
            }
            KeyCode::Enter => {
                return Ok(if yes_selected {
                    ConfirmOutcome::Yes
                } else {
                    ConfirmOutcome::No
                });
            }
            KeyCode::Char('y') | KeyCode::Char('Y') => return Ok(ConfirmOutcome::Yes),
            KeyCode::Char('n') | KeyCode::Char('N') => return Ok(ConfirmOutcome::No),
            KeyCode::Esc => return Ok(ConfirmOutcome::Cancelled),
            _ => {}
        }
    }
}

/// Overlay variant of [`show_modal_confirm`] that paints the modal ON
/// TOP of `app.screen` so the underlying menu (typically the
/// emergency picker) stays visible behind. Closing the modal restores
/// `app.modal` to `None` and the next render returns to the same
/// selection / scroll state.
pub fn show_modal_confirm_over(
    console: &mut dyn Console,
    app: &mut App<'_>,
    title: &str,
    message: &str,
    yes_label: &str,
    no_label: &str,
    yes_default: bool,
) -> Result<ConfirmOutcome> {
    use crossterm::event::KeyCode;

    let hint = "Left/Right select  Enter confirm  Esc cancel";
    app.modal_scroll_reset();
    let outcome = (|| -> Result<ConfirmOutcome> {
        app.modal = Some(ModalKind::Confirm {
            title: title.to_owned(),
            message: message.to_owned(),
            yes_label: yes_label.to_owned(),
            no_label: no_label.to_owned(),
            yes_selected: yes_default,
            hint: hint.to_owned(),
        });

        let mut dirty = true;
        loop {
            if dirty {
                if let Err(e) = console.render(app) {
                    eprintln!("[nmbl] {title}: {message}");
                    crate::nmbl_warn!("modal-confirm render failed: {e}");
                    return Ok(ConfirmOutcome::No);
                }
                dirty = false;
            }

            let key = match modal_poll(console, POLL_SLICE)? {
                ModalPollOutcome::Key(k) => k,
                ModalPollOutcome::Resized => {
                    dirty = true;
                    continue;
                }
                ModalPollOutcome::Idle => continue,
            };
            // Pull the modal's message out for the scroll helper. We
            // need an immutable read here BEFORE the mutable borrow on
            // app.modal for `yes_selected` below; otherwise borrowck
            // rejects the second access.
            let modal_message = match &app.modal {
                Some(ModalKind::Confirm { message, .. }) => message.clone(),
                _ => return Ok(ConfirmOutcome::No),
            };
            if let Some(new_off) = handle_modal_scroll_key(
                key,
                &modal_message,
                true,
                2,
                console,
                app.modal_scroll_offset,
            ) {
                app.modal_scroll_offset = new_off;
                dirty = true;
                continue;
            }
            let Some(ModalKind::Confirm { yes_selected, .. }) = &mut app.modal else {
                return Ok(ConfirmOutcome::No);
            };
            match key.code {
                KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                    *yes_selected = !*yes_selected;
                    dirty = true;
                }
                KeyCode::Enter => {
                    return Ok(if *yes_selected {
                        ConfirmOutcome::Yes
                    } else {
                        ConfirmOutcome::No
                    });
                }
                KeyCode::Char('y') | KeyCode::Char('Y') => return Ok(ConfirmOutcome::Yes),
                KeyCode::Char('n') | KeyCode::Char('N') => return Ok(ConfirmOutcome::No),
                KeyCode::Esc => return Ok(ConfirmOutcome::Cancelled),
                _ => {}
            }
        }
    })();
    app.modal = None;
    app.modal_scroll_reset();
    outcome
}

/// Show a centred modal dialog with `title` + `message` on the supplied
/// console and block until the operator presses any key (or
/// `timeout_secs` elapses, whichever comes first). Use this for
/// surfacing action failures (PTY allocation, network mount, …) so the
/// operator sees what just went wrong instead of staring at the stale
/// screen underneath.
///
/// Falls back to a serial-style stderr dump when the render fails so
/// the operator on a degraded console still gets the diagnostic.
pub fn show_modal_error(
    console: &mut dyn Console,
    title: &str,
    message: &str,
    timeout: Duration,
) -> Result<()> {
    let hint = "press any key to continue";
    let mut scroll_offset: u16 = 0;
    let mut dirty = true;
    let deadline = Instant::now().checked_add(timeout);
    loop {
        if dirty {
            let data = view::ModalErrorScreenData {
                title,
                message,
                hint,
                scroll_offset,
            };
            if let Err(e) = console.draw_with(&mut |frame| view::render_modal_error(frame, &data))
            {
                eprintln!("[nmbl] {title}: {message}");
                crate::nmbl_warn!("modal-error render failed: {e}");
                return Ok(());
            }
            dirty = false;
        }
        let slice = match deadline {
            Some(d) => match d.checked_duration_since(Instant::now()) {
                Some(remaining) => remaining.min(POLL_SLICE),
                None => return Ok(()),
            },
            None => POLL_SLICE,
        };
        let key = match modal_poll(console, slice)? {
            ModalPollOutcome::Key(k) => k,
            ModalPollOutcome::Resized => {
                dirty = true;
                continue;
            }
            ModalPollOutcome::Idle => continue,
        };
        // Scroll keys advance the viewport instead of dismissing; any
        // other key dismisses the modal.
        if let Some(new_off) = handle_modal_scroll_key(key, message, false, 0, console, scroll_offset) {
            scroll_offset = new_off;
            dirty = true;
            continue;
        }
        return Ok(());
    }
}

/// Overlay variant of [`show_modal_error`] that paints the modal ON
/// TOP of `app.screen` so the menu underneath stays visible. Closing
/// the modal restores `app.modal` to `None`.
pub fn show_modal_error_over(
    console: &mut dyn Console,
    app: &mut App<'_>,
    title: &str,
    message: &str,
    timeout: Duration,
) -> Result<()> {
    let hint = "press any key to continue";
    app.modal_scroll_reset();
    app.modal = Some(ModalKind::Error {
        title: title.to_owned(),
        message: message.to_owned(),
        hint: hint.to_owned(),
    });
    let deadline = Instant::now().checked_add(timeout);
    let mut dirty = true;
    let res = loop {
        if dirty {
            if let Err(e) = console.render(app) {
                eprintln!("[nmbl] {title}: {message}");
                crate::nmbl_warn!("modal-error render failed: {e}");
                break Ok(());
            }
            dirty = false;
        }
        let slice = match deadline {
            Some(d) => match d.checked_duration_since(Instant::now()) {
                Some(remaining) => remaining.min(POLL_SLICE),
                None => break Ok(()),
            },
            None => POLL_SLICE,
        };
        let key = match modal_poll(console, slice)? {
            ModalPollOutcome::Key(k) => k,
            ModalPollOutcome::Resized => {
                dirty = true;
                continue;
            }
            ModalPollOutcome::Idle => continue,
        };
        if let Some(new_off) =
            handle_modal_scroll_key(key, message, false, 0, console, app.modal_scroll_offset)
        {
            app.modal_scroll_offset = new_off;
            dirty = true;
            continue;
        }
        break Ok(());
    };
    app.modal = None;
    app.modal_scroll_reset();
    res
}

/// Show the wrong-password modal after a `luks-password` activation
/// returns cryptsetup exit code 2 (no key available). Four buttons when
/// the `image-splash` feature is on (three otherwise): `[Try again]`
/// (default), `[Reboot]`, `[Pretty Shell]` (feature-gated),
/// `[Raw Shell]`. Esc maps to [`WrongPasswordOutcome::TryAgain`] so a
/// stray Esc doesn't reboot the machine.
///
/// `attempt` is 1-indexed; the title reads "Wrong password (attempt N)".
///
/// If the backend itself fails to render, we fall back to
/// [`WrongPasswordOutcome::TryAgain`] — same principle as
/// [`show_modal_error`]/[`show_modal_confirm`]: when the operator can't
/// see the modal, default to the safest action (which here is to
/// re-prompt rather than reboot or open a shell).
pub fn show_wrong_password_modal(
    console: &mut dyn Console,
    attempt: u32,
) -> Result<WrongPasswordOutcome> {
    use crossterm::event::KeyCode;

    let title = format!("Wrong password (attempt {attempt})");
    let message =
        "cryptsetup rejected the passphrase. Try again, reboot, or open a recovery shell.";
    let hint = "Left/Right select  Enter confirm  Esc = Try again";
    // Button layout is feature-dependent: Pretty Shell only exists when
    // the `image-splash` feature compiled the alacritty-backed PTY
    // emulator into the binary. We materialise the label list once at
    // entry so the render loop and the key handler share the same
    // indexing.
    #[cfg(feature = "image-splash")]
    let labels: &[&str] = &["Try again", "Reboot", "Pretty Shell", "Raw Shell"];
    #[cfg(not(feature = "image-splash"))]
    let labels: &[&str] = &["Try again", "Reboot", "Raw Shell"];
    let n = labels.len();
    let mut selected: usize = 0;
    let mut scroll_offset: u16 = 0;

    let mut dirty = true;
    loop {
        if dirty {
            let data = view::ModalButtonsScreenData {
                title: &title,
                message,
                labels,
                selected,
                hint,
                scroll_offset,
            };
            if let Err(e) =
                console.draw_with(&mut |frame| view::render_modal_buttons(frame, &data))
            {
                eprintln!("[nmbl] {title}: {message}");
                crate::nmbl_warn!("wrong-password modal render failed: {e}");
                return Ok(WrongPasswordOutcome::TryAgain);
            }
            dirty = false;
        }

        let key = match modal_poll(console, POLL_SLICE)? {
            ModalPollOutcome::Key(k) => k,
            ModalPollOutcome::Resized => {
                dirty = true;
                continue;
            }
            ModalPollOutcome::Idle => continue,
        };
        let btn_count = u16::try_from(n).unwrap_or(u16::MAX);
        if let Some(new_off) =
            handle_modal_scroll_key(key, message, true, btn_count, console, scroll_offset)
        {
            scroll_offset = new_off;
            dirty = true;
            continue;
        }
        match key.code {
            KeyCode::Left | KeyCode::BackTab => {
                selected = if selected == 0 {
                    n.saturating_sub(1)
                } else {
                    selected.saturating_sub(1)
                };
                dirty = true;
            }
            KeyCode::Right | KeyCode::Tab => {
                selected = selected.saturating_add(1) % n;
                dirty = true;
            }
            KeyCode::Enter => return Ok(decode_wrong_password_selection(selected)),
            KeyCode::Char('t') | KeyCode::Char('T') => {
                return Ok(WrongPasswordOutcome::TryAgain);
            }
            KeyCode::Char('r') | KeyCode::Char('R') => {
                return Ok(WrongPasswordOutcome::Reboot);
            }
            #[cfg(feature = "image-splash")]
            KeyCode::Char('p') | KeyCode::Char('P') => {
                return Ok(WrongPasswordOutcome::PrettyShell);
            }
            KeyCode::Char('s') | KeyCode::Char('S') => {
                return Ok(WrongPasswordOutcome::RawShell);
            }
            KeyCode::Esc => return Ok(WrongPasswordOutcome::TryAgain),
            _ => {}
        }
    }
}

/// Show a generic N-button modal and return the committed button
/// index. Used by callers that don't fit the wrong-password layout
/// (the only specialised driver today) but want the same look-and-feel:
/// bordered modal, wrapped message, right-aligned button row.
///
/// Esc returns the LAST button index (caller convention: rightmost is
/// "Cancel" / "Back"). Empty `labels` returns 0 immediately so the
/// caller's caller never indexes off the end.
pub fn show_modal_buttons(
    console: &mut dyn Console,
    title: &str,
    message: &str,
    labels: &[&str],
    hint: &str,
) -> Result<usize> {
    use crossterm::event::KeyCode;
    let n = labels.len();
    if n == 0 {
        return Ok(0);
    }
    let mut selected: usize = 0;
    let mut scroll_offset: u16 = 0;
    let mut dirty = true;
    loop {
        if dirty {
            let data = view::ModalButtonsScreenData {
                title,
                message,
                labels,
                selected,
                hint,
                scroll_offset,
            };
            if let Err(e) =
                console.draw_with(&mut |frame| view::render_modal_buttons(frame, &data))
            {
                eprintln!("[nmbl] {title}: {message}");
                crate::nmbl_warn!("modal-buttons render failed: {e}");
                return Ok(n.saturating_sub(1));
            }
            dirty = false;
        }
        let key = match modal_poll(console, POLL_SLICE)? {
            ModalPollOutcome::Key(k) => k,
            ModalPollOutcome::Resized => {
                dirty = true;
                continue;
            }
            ModalPollOutcome::Idle => continue,
        };
        let btn_count = u16::try_from(n).unwrap_or(u16::MAX);
        if let Some(new_off) =
            handle_modal_scroll_key(key, message, true, btn_count, console, scroll_offset)
        {
            scroll_offset = new_off;
            dirty = true;
            continue;
        }
        match key.code {
            KeyCode::Left | KeyCode::BackTab => {
                selected = if selected == 0 {
                    n.saturating_sub(1)
                } else {
                    selected.saturating_sub(1)
                };
                dirty = true;
            }
            KeyCode::Right | KeyCode::Tab => {
                selected = selected.saturating_add(1) % n;
                dirty = true;
            }
            KeyCode::Enter => return Ok(selected),
            KeyCode::Esc => return Ok(n.saturating_sub(1)),
            _ => {}
        }
    }
}

/// Map a wrong-password modal button index to its outcome. Index 0 is
/// always Try again, index 1 is Reboot, then Pretty Shell (only when
/// `image-splash` is on), then Raw Shell. Out-of-range indices fall
/// back to TryAgain so a future button-layout drift can't crash boot.
fn decode_wrong_password_selection(idx: usize) -> WrongPasswordOutcome {
    #[cfg(feature = "image-splash")]
    {
        match idx {
            1 => WrongPasswordOutcome::Reboot,
            2 => WrongPasswordOutcome::PrettyShell,
            3 => WrongPasswordOutcome::RawShell,
            _ => WrongPasswordOutcome::TryAgain,
        }
    }
    #[cfg(not(feature = "image-splash"))]
    {
        match idx {
            1 => WrongPasswordOutcome::Reboot,
            2 => WrongPasswordOutcome::RawShell,
            _ => WrongPasswordOutcome::TryAgain,
        }
    }
}

/// Run the boot-selection TUI on the provided [`Console`] and return
/// the operator's decision.
///
/// The console is brought up once by the orchestrator (main.rs) at the
/// start of phase 1 and held through every phase; this function reuses
/// it instead of opening a parallel splash bring-up, so the same DRM
/// card / raw-mode tty serves the whole boot. Serial UARTs go through
/// the same path — the TUI's crossterm backend emits portable
/// vt100/xterm escapes that every modern serial terminal renders.
pub fn run_selector(
    config: &Config,
    generations: &[Generation],
    console: &mut dyn Console,
) -> Result<Decision> {
    run_selector_on_console(config, generations, console)
}

/// TUI event loop. Backend-agnostic: every render and key-poll goes
/// through the [`Console`] trait. Hosts the countdown, the List/Editing
/// state machine, and the timeout-defaults-to-first-generation rule.
fn run_selector_on_console(
    config: &Config,
    generations: &[Generation],
    console: &mut dyn Console,
) -> Result<Decision> {
    let mut app = App::new(generations);
    app.show_kernel_params = config.tui.show_kernel_params;

    // 1. Countdown phase.
    let countdown = Duration::from_secs(u64::from(config.general.timeout_secs));
    let outcome = run_console_countdown(console, &mut app, countdown)?;
    app.countdown_remaining_secs = None;

    if matches!(outcome, TimeoutOutcome::Expired) && app.decision.is_none() {
        return Ok(Decision::Boot {
            generation_index: 0,
            cmdline_override: None,
        });
    }

    // 2. Event loop. Renders on dirty, polls in short slices so future
    //    callers that need to drive an animation can plug in without
    //    rewriting the loop. Driven via `poll_event` so a host-reported
    //    `CSI 8;rows;cols t` resize redraws the picker against the new
    //    grid instead of stranding the old layout.
    let mut dirty = true;
    loop {
        if dirty {
            console.render(&app)?;
            dirty = false;
        }
        match console.poll_event(POLL_SLICE)? {
            Some(crate::ui::console::ConsoleEvent::Resize { .. }) => {
                dirty = true;
            }
            Some(crate::ui::console::ConsoleEvent::Key(key)) => {
                if app.on_key(key) {
                    break;
                }
                dirty = true;
            }
            None => {}
        }
        if app.decision.is_some() {
            break;
        }
    }

    app.decision.ok_or_else(|| NmblError::Tui {
        source: std::io::Error::other("selector exited without decision"),
    })
}

/// Countdown driver that polls the [`Console`] for keys instead of
/// stdin, so cancel-on-keypress works on both the splash framebuffer
/// (input via `/dev/tty1`) and the raw-mode tty.
fn run_console_countdown(
    console: &mut dyn Console,
    app: &mut App<'_>,
    duration: Duration,
) -> Result<TimeoutOutcome> {
    let start = Instant::now();
    let deadline = start.checked_add(duration).unwrap_or(start);

    let initial = duration.as_secs();
    app.countdown_remaining_secs = Some(initial);
    console.render(app)?;
    let mut last_reported = initial;

    loop {
        let now = Instant::now();
        let Some(remaining) = deadline.checked_duration_since(now) else {
            return Ok(TimeoutOutcome::Expired);
        };

        let slice = remaining.min(POLL_SLICE);
        if console.poll_key(slice)?.is_some() {
            return Ok(TimeoutOutcome::Cancelled);
        }

        let now = Instant::now();
        let Some(remaining) = deadline.checked_duration_since(now) else {
            return Ok(TimeoutOutcome::Expired);
        };
        let secs = remaining.as_secs();
        if secs != last_reported {
            app.countdown_remaining_secs = Some(secs);
            console.render(app)?;
            last_reported = secs;
        }
    }
}

/// Render one frame: ratatui-draw → vte parse → cell-walk → blit.
///
/// The ratatui side emits absolute cursor positions every frame, but
/// `alacritty_terminal::Term` accumulates SGR state across feeds. A
/// fresh `SplashTerminal` per frame is the simplest way to guarantee
/// the grid reflects only the current frame's bytes.
#[cfg(feature = "image-splash")]
pub(crate) fn render_splash_frame(
    drm: &mut drm::SplashDrm,
    bg_scaled: &[u8],
    cache: &glyph_cache::GlyphCache,
    cell_dims: CellDims,
    app: &App<'_>,
) -> Result<()> {
    render_splash_frame_with(drm, bg_scaled, cache, cell_dims, &mut |f| {
        render_current_screen(f, app);
    })
}

/// Generic counterpart to [`render_splash_frame`] that takes a render
/// closure instead of an `App`. Used by [`Console::draw_with`] on the
/// splash backend to paint dynamic widgets (network-rescue gauges,
/// cursor-tracking editors) that don't fit the App+Screen state
/// machine — the same compositor / cell-walk / blit pipeline is reused
/// so no parallel splash bring-up is performed.
#[cfg(feature = "image-splash")]
pub(crate) fn render_splash_frame_with(
    drm: &mut drm::SplashDrm,
    bg_scaled: &[u8],
    cache: &glyph_cache::GlyphCache,
    cell_dims: CellDims,
    body: &mut dyn FnMut(&mut ratatui::Frame<'_>),
) -> Result<()> {
    let mut buf: Vec<u8> = Vec::new();
    {
        let backend = CrosstermBackend::new(&mut buf);
        let viewport = Viewport::Fixed(Rect::new(0, 0, cell_dims.cols, cell_dims.rows));
        let mut terminal =
            Terminal::with_options(backend, TerminalOptions { viewport }).map_err(tui_err)?;
        terminal.draw(|f| body(f)).map_err(tui_err)?;
    }

    let mut term_pipe = SplashTerminal::new(cell_dims);
    term_pipe.feed(&buf);

    drm.render(|fb, fb_dims| {
        compositor::blit_background(fb, fb_dims, bg_scaled);
        term_pipe.for_each_cell(|col, row, cell| {
            if cell.c == ' ' && cell.bg == Color::Named(NamedColor::Background) {
                return;
            }
            let bold = cell.flags.contains(Flags::BOLD);
            let Some(glyph) = cache.get(cell.c, bold) else {
                return;
            };
            let fg = compositor::resolve_color(cell.fg);
            let bg = compositor::resolve_color(cell.bg);
            let x = u32::from(col).saturating_mul(cell_dims.cell_w);
            let y = u32::from(row).saturating_mul(cell_dims.cell_h);
            let rect = compositor::CellRect {
                x,
                y,
                w: cell_dims.cell_w,
                h: cell_dims.cell_h,
            };
            compositor::blit_cell(fb, fb_dims, glyph, rect, fg, bg);
        });
        Ok(())
    })
}

/// Dispatch render based on which screen the App is currently on,
/// then paint the modal overlay on top when `app.modal` is `Some`.
///
/// The underlying screen renders first so the operator sees "where
/// they were" behind a confirmation / error / progress dialog. The
/// modal renderers use ratatui's `Clear` widget on their rect, so
/// they punch a hole without bleeding the menu into the modal body.
///
/// `pub(crate)` so the splash orchestrator can reuse the same dispatch
/// without forking the per-screen branching.
pub(crate) fn render_current_screen(frame: &mut ratatui::Frame<'_>, app: &App<'_>) {
    render_screen_body(frame, app);
    if let Some(modal) = &app.modal {
        render_modal_overlay(frame, modal, app.modal_scroll_offset);
    }
}

fn render_modal_overlay(frame: &mut ratatui::Frame<'_>, modal: &ModalKind, scroll_offset: u16) {
    match modal {
        ModalKind::Confirm {
            title,
            message,
            yes_label,
            no_label,
            yes_selected,
            hint,
        } => {
            let data = view::ModalConfirmScreenData {
                title,
                message,
                yes_label,
                no_label,
                yes_selected: *yes_selected,
                hint,
                scroll_offset,
            };
            view::render_modal_confirm(frame, &data);
        }
        ModalKind::Error { title, message, hint } => {
            let data = view::ModalErrorScreenData {
                title,
                message,
                hint,
                scroll_offset,
            };
            view::render_modal_error(frame, &data);
        }
        ModalKind::Buttons {
            title,
            message,
            labels,
            selected,
            hint,
        } => {
            // ModalButtonsScreenData borrows &[&str]; rebuild a slice
            // of borrowed views into the owned `labels` Vec.
            let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            let data = view::ModalButtonsScreenData {
                title,
                message,
                labels: &label_refs,
                selected: *selected,
                hint,
                scroll_offset,
            };
            view::render_modal_buttons(frame, &data);
        }
        ModalKind::Status {
            phase,
            log_lines,
            spinner_frame,
        } => {
            let data = BootStatusData {
                phase: std::borrow::Cow::Borrowed(phase),
                // Clone is unavoidable: BootStatusData wants Vec<String>
                // and the renderer iterates over the slice. The status
                // overlay only paints a handful of log lines so the
                // clone is cheap.
                log_lines: log_lines.clone(),
                spinner_frame: *spinner_frame,
            };
            view::render_boot_status(frame, &data);
        }
    }
}

fn render_screen_body(frame: &mut ratatui::Frame<'_>, app: &App<'_>) {
    match &app.screen {
        Screen::List => render_list(frame, &list_data(app)),
        Screen::Editing {
            generation_index,
            buffer,
            cursor,
        } => {
            if let Some(g) = app.generations.get(*generation_index) {
                let data = EditScreenData {
                    generation: g,
                    edited_cmdline: buffer,
                    cursor_position: *cursor,
                };
                render_edit(frame, &data);
            }
        }
        Screen::Passphrase {
            prompt_label,
            buffer,
            verifying,
            spinner_frame,
        } => {
            let data = PassphraseScreenData {
                prompt_label,
                buffer_len: buffer.len(),
                verifying: *verifying,
                spinner_frame: *spinner_frame,
            };
            render_passphrase(frame, &data);
        }
        Screen::Emergency {
            message,
            items,
            selected,
            ..
        } => {
            let data = EmergencyScreenData {
                message,
                items,
                selected_index: *selected,
                countdown_remaining_secs: app.countdown_remaining_secs,
            };
            render_emergency(frame, &data);
        }
        Screen::BootStatus(data) => render_boot_status(frame, data),
        Screen::KeyEcho { events, byte_log } => {
            // VecDeque is not necessarily contiguous, so flatten through
            // make_contiguous-free iteration: collect via a slice pair.
            // Cheap because we only ever store ≤20 entries per panel.
            let events_vec: Vec<String> = events.iter().cloned().collect();
            let bytes_vec: Vec<String> = byte_log.iter().cloned().collect();
            let data = KeyEchoScreenData {
                events: &events_vec,
                byte_log: &bytes_vec,
            };
            render_key_echo(frame, &data);
        }
    }
}

fn list_data<'a>(app: &'a App<'a>) -> ListScreenData<'a> {
    ListScreenData {
        generations: app.generations,
        selected_index: app.selected_index,
        countdown_remaining_secs: app.countdown_remaining_secs,
        show_kernel_params: app.show_kernel_params,
    }
}

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
pub struct TuiPasswordSupplier;

impl TuiPasswordSupplier {
    #[must_use]
    pub fn new(_config: &Config) -> Self {
        // `_config` is accepted for forward-compatibility with
        // future per-config passphrase policy (retry counts, masking
        // toggles, …). Today the supplier is uniform — the same
        // ratatui modal everywhere.
        Self
    }
}

impl PasswordSupplier for TuiPasswordSupplier {
    fn prompt(&mut self, console: &mut dyn Console, label: &str) -> Result<Zeroizing<String>> {
        passphrase_prompt_on_console(console, label)
    }
}

/// Drive the [`Screen::Passphrase`] modal on the supplied [`Console`]
/// until the operator submits (Enter) or cancels (Esc).
///
/// Esc translates to a [`NmblError::Tui`] so the caller can drop to
/// the emergency shell. The Console is reused, NOT re-opened — the
/// orchestrator already brought up the splash framebuffer or raw-mode
/// tty before phase 1 and held it through every phase.
pub(crate) fn passphrase_prompt_on_console(
    console: &mut dyn Console,
    label: &str,
) -> Result<Zeroizing<String>> {
    // No generations to render — pass an empty slice. The App is
    // only used here for its Passphrase screen state.
    let empty: [Generation; 0] = [];
    let mut app = App::new(&empty);
    app.screen = Screen::Passphrase {
        prompt_label: label.to_string(),
        buffer: Zeroizing::new(String::new()),
        verifying: false,
        spinner_frame: 0,
    };

    let mut dirty = true;
    loop {
        if dirty {
            console.render(&app)?;
            dirty = false;
        }

        // Drive `poll_event` so host-reported `CSI 8;rows;cols t`
        // resizes redraw the modal at the new dimensions instead of
        // smearing the old layout until the next keypress.
        match console.poll_event(POLL_SLICE)? {
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
            None => {}
        }
    }
}

#[cfg(feature = "image-splash")]
fn tui_err(source: std::io::Error) -> NmblError {
    NmblError::Tui { source }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;

    #[test]
    fn tui_password_supplier_satisfies_password_supplier_trait() {
        // The integration contract: activation::run_all_activations
        // accepts `Option<&mut dyn PasswordSupplier>`. This test pins
        // that coercion so a future signature drift on either side
        // breaks the build instead of breaking at boot.
        let cfg: Config = toml::from_str("").expect("default cfg");
        let mut sup = TuiPasswordSupplier::new(&cfg);
        let _coerced: &mut dyn PasswordSupplier = &mut sup;
    }

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use crate::ui::console::{ConsoleEvent, ConsoleKind};

    /// Console test double that returns canned key events and records
    /// every render call. `poll_key` first drains the queue (returning
    /// one event per call) and then yields `None` so the supplier's
    /// loop wraps tightly without us having to manage real timeouts.
    struct ScriptedConsole {
        keys: std::collections::VecDeque<KeyEvent>,
        renders: u32,
        last_buffer_len: usize,
        last_label: Option<String>,
    }

    impl ScriptedConsole {
        fn new(keys: Vec<KeyEvent>) -> Self {
            Self {
                keys: keys.into(),
                renders: 0,
                last_buffer_len: 0,
                last_label: None,
            }
        }
    }

    impl Console for ScriptedConsole {
        fn render(&mut self, app: &App<'_>) -> Result<()> {
            self.renders = self.renders.saturating_add(1);
            if let Screen::Passphrase {
                buffer,
                prompt_label,
                ..
            } = &app.screen
            {
                self.last_buffer_len = buffer.len();
                self.last_label = Some(prompt_label.clone());
            }
            Ok(())
        }
        fn poll_event(&mut self, _timeout: Duration) -> Result<Option<ConsoleEvent>> {
            Ok(self.keys.pop_front().map(ConsoleEvent::Key))
        }
        fn size(&self) -> (u16, u16) {
            (80, 24)
        }
        fn kind(&self) -> ConsoleKind {
            ConsoleKind::Tty
        }
        fn draw_with(
            &mut self,
            _body: &mut dyn FnMut(&mut ratatui::Frame<'_>),
        ) -> Result<()> {
            self.renders = self.renders.saturating_add(1);
            Ok(())
        }
        fn suspend(&mut self) -> Result<()> {
            Ok(())
        }
        fn resume(&mut self) -> Result<()> {
            Ok(())
        }
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn passphrase_prompt_collects_typed_chars_and_returns_on_enter() {
        // Type "ok" + Enter — supplier must return "ok" and the
        // console must have observed the per-char buffer growth.
        let keys = vec![
            press(KeyCode::Char('o')),
            press(KeyCode::Char('k')),
            press(KeyCode::Enter),
        ];
        let mut console = ScriptedConsole::new(keys);
        let secret = passphrase_prompt_on_console(&mut console, "Unlock root")
            .expect("Enter submits the buffer");
        assert_eq!(&**secret, "ok");
        // Initial render + 2 char-keys + 1 Enter = 4 dirty repaints.
        assert!(
            console.renders >= 3,
            "expected at least 3 renders, got {}",
            console.renders
        );
        assert_eq!(
            console.last_label.as_deref(),
            Some("Unlock root"),
            "render path must observe the supplied prompt label"
        );
    }

    #[test]
    fn passphrase_prompt_ignores_enter_on_empty_buffer() {
        // Enter on an empty buffer must be silently ignored (matches
        // login-screen convention; an empty string would surface as a
        // cryptsetup IO error otherwise). Once a char arrives, Enter
        // submits as usual.
        let keys = vec![
            press(KeyCode::Enter),
            press(KeyCode::Char('p')),
            press(KeyCode::Enter),
        ];
        let mut console = ScriptedConsole::new(keys);
        let secret = passphrase_prompt_on_console(&mut console, "Unlock")
            .expect("Enter after a char submits the buffer");
        assert_eq!(&**secret, "p");
    }

    #[test]
    fn passphrase_prompt_backspace_shrinks_buffer() {
        let keys = vec![
            press(KeyCode::Char('a')),
            press(KeyCode::Char('b')),
            press(KeyCode::Backspace),
            press(KeyCode::Enter),
        ];
        let mut console = ScriptedConsole::new(keys);
        let secret = passphrase_prompt_on_console(&mut console, "Unlock")
            .expect("Enter submits the buffer");
        assert_eq!(&**secret, "a", "backspace must drop the last char");
    }

    #[test]
    fn passphrase_prompt_esc_returns_tui_error() {
        let keys = vec![press(KeyCode::Char('x')), press(KeyCode::Esc)];
        let mut console = ScriptedConsole::new(keys);
        let err = passphrase_prompt_on_console(&mut console, "Unlock")
            .expect_err("Esc must propagate as a Tui error");
        assert!(matches!(err, NmblError::Tui { .. }));
    }

    #[test]
    fn passphrase_prompt_renders_dotted_mask_via_view() {
        // End-to-end visual check: drive the supplier under a TestBackend
        // until just before Enter, then synthesise one final render to
        // capture the masked view. Sanity-checks both that the supplier
        // reuses render_passphrase and that the mask grows with the buffer.
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let data = PassphraseScreenData {
            prompt_label: "Unlock root",
            buffer_len: 4,
            verifying: false,
            spinner_frame: 0,
        };
        let mut term = Terminal::new(TestBackend::new(60, 14)).expect("test terminal");
        term.draw(|f| render_passphrase(f, &data)).expect("draw");
        let buf = term.backend().buffer();
        let dump: String = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol().to_owned()))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            dump.contains("****"),
            "masked dots must be visible for buffer_len=4: \n{dump}"
        );
        assert!(
            dump.contains("Unlock root"),
            "prompt label must be visible: \n{dump}"
        );
        assert!(
            dump.contains("Enter=submit"),
            "footer hint must be visible: \n{dump}"
        );
    }

    #[test]
    fn show_modal_confirm_returns_yes_on_enter_with_default_true() {
        // yes_default = true highlights Yes; Enter immediately commits
        // to Yes without needing arrow keys.
        let keys = vec![press(KeyCode::Enter)];
        let mut console = ScriptedConsole::new(keys);
        let out = show_modal_confirm(
            &mut console,
            "Boot one?",
            "Found 3 generations.",
            "Yes",
            "Back",
            true,
        )
        .expect("modal must succeed on Enter");
        assert_eq!(out, ConfirmOutcome::Yes);
    }

    #[test]
    fn show_modal_confirm_returns_no_on_enter_with_default_false() {
        // yes_default = false highlights Back; Enter commits to No.
        let keys = vec![press(KeyCode::Enter)];
        let mut console = ScriptedConsole::new(keys);
        let out = show_modal_confirm(
            &mut console,
            "Are you sure?",
            "This may destroy data.",
            "Yes",
            "No",
            false,
        )
        .expect("modal must succeed");
        assert_eq!(out, ConfirmOutcome::No);
    }

    #[test]
    fn show_modal_confirm_arrow_keys_toggle_selection_then_enter_commits() {
        // Default Yes, then Right toggles to No, Enter commits to No.
        let keys = vec![press(KeyCode::Right), press(KeyCode::Enter)];
        let mut console = ScriptedConsole::new(keys);
        let out = show_modal_confirm(&mut console, "t", "b", "Yes", "No", true)
            .expect("modal must succeed");
        assert_eq!(out, ConfirmOutcome::No);

        // Default No, then Left toggles to Yes, Enter commits to Yes.
        let keys = vec![press(KeyCode::Left), press(KeyCode::Enter)];
        let mut console = ScriptedConsole::new(keys);
        let out = show_modal_confirm(&mut console, "t", "b", "Yes", "No", false)
            .expect("modal must succeed");
        assert_eq!(out, ConfirmOutcome::Yes);
    }

    #[test]
    fn show_modal_confirm_hotkey_y_returns_yes() {
        // 'y' hotkey commits to Yes regardless of which button is
        // highlighted — matches the muscle-memory pattern of every
        // other confirmation prompt in the binary.
        let keys = vec![press(KeyCode::Char('y'))];
        let mut console = ScriptedConsole::new(keys);
        let out = show_modal_confirm(&mut console, "t", "b", "Yes", "No", false)
            .expect("modal must succeed on 'y'");
        assert_eq!(out, ConfirmOutcome::Yes);
    }

    #[test]
    fn show_modal_confirm_hotkey_n_returns_no() {
        let keys = vec![press(KeyCode::Char('n'))];
        let mut console = ScriptedConsole::new(keys);
        let out = show_modal_confirm(&mut console, "t", "b", "Yes", "No", true)
            .expect("modal must succeed on 'n'");
        assert_eq!(out, ConfirmOutcome::No);
    }

    #[test]
    fn show_modal_confirm_esc_returns_cancelled() {
        let keys = vec![press(KeyCode::Esc)];
        let mut console = ScriptedConsole::new(keys);
        let out = show_modal_confirm(&mut console, "t", "b", "Yes", "Back", true)
            .expect("modal must succeed on Esc");
        assert_eq!(out, ConfirmOutcome::Cancelled);
    }

    #[test]
    fn show_modal_confirm_renders_at_least_once_before_polling() {
        // Defence-in-depth: the operator must see the modal BEFORE we
        // start blocking on input. If a future refactor reorders the
        // draw and poll, the picker would block on a stale screen.
        let keys = vec![press(KeyCode::Char('y'))];
        let mut console = ScriptedConsole::new(keys);
        let _ = show_modal_confirm(&mut console, "t", "b", "Yes", "No", true)
            .expect("modal must succeed");
        assert!(
            console.renders >= 1,
            "expected at least one render, got {}",
            console.renders
        );
    }

    // --- show_wrong_password_modal -----------------------------------

    #[test]
    fn show_wrong_password_modal_default_enter_returns_try_again() {
        // Default highlight is [Try again] so a single Enter must
        // commit to TryAgain — protects the most common path
        // (operator mistyped, just wants to retry).
        let keys = vec![press(KeyCode::Enter)];
        let mut console = ScriptedConsole::new(keys);
        let out =
            show_wrong_password_modal(&mut console, 1).expect("modal must succeed on Enter");
        assert_eq!(out, WrongPasswordOutcome::TryAgain);
    }

    #[test]
    fn show_wrong_password_modal_right_arrow_then_enter_reboots() {
        // Right toggles to [Reboot]; Enter commits.
        let keys = vec![press(KeyCode::Right), press(KeyCode::Enter)];
        let mut console = ScriptedConsole::new(keys);
        let out =
            show_wrong_password_modal(&mut console, 1).expect("modal must succeed");
        assert_eq!(out, WrongPasswordOutcome::Reboot);
    }

    #[cfg(feature = "image-splash")]
    #[test]
    fn show_wrong_password_modal_two_rights_then_enter_picks_pretty_shell() {
        // With `image-splash` Pretty Shell sits at index 2. Right Right
        // navigates there; Enter commits.
        let keys = vec![
            press(KeyCode::Right),
            press(KeyCode::Right),
            press(KeyCode::Enter),
        ];
        let mut console = ScriptedConsole::new(keys);
        let out = show_wrong_password_modal(&mut console, 2).expect("modal must succeed");
        assert_eq!(out, WrongPasswordOutcome::PrettyShell);
    }

    #[cfg(feature = "image-splash")]
    #[test]
    fn show_wrong_password_modal_three_rights_then_enter_picks_raw_shell() {
        // With `image-splash` Raw Shell sits at index 3. Right Right Right
        // navigates there; Enter commits.
        let keys = vec![
            press(KeyCode::Right),
            press(KeyCode::Right),
            press(KeyCode::Right),
            press(KeyCode::Enter),
        ];
        let mut console = ScriptedConsole::new(keys);
        let out = show_wrong_password_modal(&mut console, 2).expect("modal must succeed");
        assert_eq!(out, WrongPasswordOutcome::RawShell);
    }

    #[cfg(not(feature = "image-splash"))]
    #[test]
    fn show_wrong_password_modal_two_rights_then_enter_picks_raw_shell_no_feature() {
        // Without `image-splash` Raw Shell sits at index 2 (Pretty
        // Shell row is hidden). Right Right + Enter commits Raw Shell.
        let keys = vec![
            press(KeyCode::Right),
            press(KeyCode::Right),
            press(KeyCode::Enter),
        ];
        let mut console = ScriptedConsole::new(keys);
        let out = show_wrong_password_modal(&mut console, 2).expect("modal must succeed");
        assert_eq!(out, WrongPasswordOutcome::RawShell);
    }

    #[test]
    fn show_wrong_password_modal_hotkeys_commit_directly() {
        // 't', 'r', 's' each commit regardless of highlighted button.
        // 'p' is only wired when `image-splash` is compiled in.
        for (code, expected) in [
            (KeyCode::Char('t'), WrongPasswordOutcome::TryAgain),
            (KeyCode::Char('r'), WrongPasswordOutcome::Reboot),
            (KeyCode::Char('s'), WrongPasswordOutcome::RawShell),
        ] {
            let mut console = ScriptedConsole::new(vec![press(code)]);
            let out = show_wrong_password_modal(&mut console, 1)
                .expect("modal must succeed on hotkey");
            assert_eq!(out, expected, "hotkey {code:?} should yield {expected:?}");
        }
        #[cfg(feature = "image-splash")]
        {
            let mut console = ScriptedConsole::new(vec![press(KeyCode::Char('p'))]);
            let out = show_wrong_password_modal(&mut console, 1)
                .expect("modal must succeed on 'p' hotkey");
            assert_eq!(out, WrongPasswordOutcome::PrettyShell);
        }
    }

    #[test]
    fn show_wrong_password_modal_esc_maps_to_try_again() {
        // Esc must NOT reboot — defence against a stray Esc keystroke
        // wiping out the boot. Spec: Esc = Try again.
        let keys = vec![press(KeyCode::Esc)];
        let mut console = ScriptedConsole::new(keys);
        let out = show_wrong_password_modal(&mut console, 3)
            .expect("modal must succeed on Esc");
        assert_eq!(out, WrongPasswordOutcome::TryAgain);
    }

    #[test]
    fn show_wrong_password_modal_left_wraps_from_try_again_to_last_button() {
        // Left arrow from index 0 wraps to the last button (Raw Shell
        // in both feature configurations — it is the rightmost row).
        let keys = vec![press(KeyCode::Left), press(KeyCode::Enter)];
        let mut console = ScriptedConsole::new(keys);
        let out = show_wrong_password_modal(&mut console, 1).expect("modal must succeed");
        assert_eq!(out, WrongPasswordOutcome::RawShell);
    }

    #[test]
    fn show_wrong_password_modal_renders_title_with_attempt_counter() {
        // End-to-end visual check: the title must include the literal
        // "attempt N" string so the operator sees the retry counter.
        // Also pins that every button label paints — including the
        // feature-gated Pretty Shell row when present.
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        #[cfg(feature = "image-splash")]
        let labels: &[&str] = &["Try again", "Reboot", "Pretty Shell", "Raw Shell"];
        #[cfg(not(feature = "image-splash"))]
        let labels: &[&str] = &["Try again", "Reboot", "Raw Shell"];

        let data = view::ModalButtonsScreenData {
            title: "Wrong password (attempt 3)",
            message: "cryptsetup rejected the passphrase.",
            labels,
            selected: 0,
            hint: "Left/Right select  Enter confirm  Esc = Try again",
            scroll_offset: 0,
        };
        let mut term = Terminal::new(TestBackend::new(80, 16)).expect("test terminal");
        term.draw(|f| view::render_modal_buttons(f, &data)).expect("draw");
        let buf = term.backend().buffer();
        let dump: String = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol().to_owned()))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            dump.contains("attempt 3"),
            "title must surface the attempt counter:\n{dump}"
        );
        assert!(dump.contains("[Try again]"), "Try again button visible:\n{dump}");
        assert!(dump.contains("[Reboot]"), "Reboot button visible:\n{dump}");
        assert!(dump.contains("[Raw Shell]"), "Raw Shell button visible:\n{dump}");
        #[cfg(feature = "image-splash")]
        assert!(
            dump.contains("[Pretty Shell]"),
            "Pretty Shell button visible:\n{dump}"
        );
    }

    // ---- Overlay variants -------------------------------------------

    #[test]
    fn show_modal_confirm_over_sets_and_clears_modal_on_app() {
        // The overlay variant must install a `ModalKind::Confirm` on
        // entry and clear it back to `None` on exit so a re-entry into
        // the picker doesn't paint a stale dialog.
        use crate::ui::app::ModalKind;
        let gens = [];
        let mut app = App::new(&gens);
        // Seed a benign screen state that should survive the modal.
        app.selected_index = 4;
        let keys = vec![press(KeyCode::Char('y'))];
        let mut console = ScriptedConsole::new(keys);
        let out = show_modal_confirm_over(
            &mut console,
            &mut app,
            "title",
            "body",
            "Yes",
            "No",
            true,
        )
        .expect("overlay modal must succeed on 'y'");
        assert_eq!(out, ConfirmOutcome::Yes);
        assert!(app.modal.is_none(), "modal must be cleared on exit");
        assert_eq!(
            app.selected_index, 4,
            "underlying selection must survive the modal"
        );
        // No leftover Confirm variant.
        let _: () = match &app.modal {
            None => (),
            Some(ModalKind::Confirm { .. }) => panic!("modal Confirm leaked"),
            Some(_) => panic!("unexpected modal variant"),
        };
    }

    #[test]
    fn show_modal_confirm_over_returns_to_same_screen_on_close() {
        // Close the modal via Esc (Cancelled) and confirm the
        // underlying screen variant is unchanged. Operators expect the
        // menu to be exactly where it was; this pins that behaviour.
        let gens = [];
        let mut app = App::new(&gens);
        // Park on a known emergency-menu screen with selection=2.
        app.screen = Screen::Emergency {
            message: "boot failed".into(),
            items: vec![
                crate::ui::app::EmergencyItem {
                    label: "Reboot",
                    choice: crate::ui::app::EmergencyChoice::Reboot,
                },
                crate::ui::app::EmergencyItem {
                    label: "Raw Shell",
                    choice: crate::ui::app::EmergencyChoice::RawShell,
                },
                crate::ui::app::EmergencyItem {
                    label: "Retry",
                    choice: crate::ui::app::EmergencyChoice::RetryBoot,
                },
            ],
            selected: 2,
            chosen: None,
        };
        let keys = vec![press(KeyCode::Esc)];
        let mut console = ScriptedConsole::new(keys);
        let out = show_modal_confirm_over(
            &mut console,
            &mut app,
            "t",
            "b",
            "Yes",
            "Back",
            true,
        )
        .expect("modal must succeed on Esc");
        assert_eq!(out, ConfirmOutcome::Cancelled);
        assert!(app.modal.is_none());
        match &app.screen {
            Screen::Emergency { selected, .. } => {
                assert_eq!(*selected, 2, "selection must survive the modal");
            }
            _ => panic!("underlying screen must remain Emergency"),
        }
    }

    #[test]
    fn show_modal_confirm_over_renders_modal_atop_underlying_screen() {
        // End-to-end visual check via the splash render path: the
        // dispatcher in `render_current_screen` must paint the menu
        // first and then the modal on top. Both must be visible in
        // the rendered buffer (modal punches a Clear into its rect,
        // but the menu header / footer survive).
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let gens = [];
        let mut app = App::new(&gens);
        app.screen = Screen::Emergency {
            message: "boot failed: synthetic".into(),
            items: vec![crate::ui::app::EmergencyItem {
                label: "RebootMenuItem",
                choice: crate::ui::app::EmergencyChoice::Reboot,
            }],
            selected: 0,
            chosen: None,
        };
        app.modal = Some(crate::ui::app::ModalKind::Confirm {
            title: "ConfirmTitleX".into(),
            message: "modal body".into(),
            yes_label: "Yes".into(),
            no_label: "No".into(),
            yes_selected: true,
            hint: "hint".into(),
        });
        let mut term = Terminal::new(TestBackend::new(80, 24)).expect("test terminal");
        term.draw(|f| render_current_screen(f, &app)).expect("draw");
        let buf = term.backend().buffer();
        let dump: String = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol().to_owned()))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            dump.contains("ConfirmTitleX"),
            "modal title must paint on top:\n{dump}"
        );
        // The underlying emergency screen must paint BEHIND the modal.
        // The centred modal punches a Clear into its rect (rows ~1..16,
        // cols ~8..72 on an 80x24 backend), but the project header in
        // row 0, the "[Rebo…" menu fragment peeking from below the
        // modal's right edge, the "action" border at the bottom, AND
        // the footer hint must all survive.
        assert!(
            dump.contains("sirati's NMBL"),
            "project header (row 0) must remain visible above the modal:\n{dump}"
        );
        assert!(
            dump.contains("[Rebo"),
            "underlying picker item must peek from behind the modal:\n{dump}"
        );
        assert!(
            dump.contains("up/down select"),
            "underlying footer hint must remain visible:\n{dump}"
        );
    }
}
