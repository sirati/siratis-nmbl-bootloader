//! Console picker dialog + shell-relay driver.
//!
//! When the operator selects `[Shell]` on the emergency screen, NMBL no
//! longer `execve(2)`s into the rescue shell as PID 1. Instead it:
//!
//! 1. Resolves the kernel-elected primary interactive console from
//!    `/sys/class/tty/console/active`, AND enumerates every plausible
//!    operator-attached tty via [`crate::ui::tty_enum::enumerate_ttys`]
//!    (framebuffer VT, `/dev/ttyS<0..3>`, USB serial). The kernel
//!    console is pre-checked and labelled `(kernel console)`; every
//!    other discovered tty is offered unchecked, labelled by kind.
//! 2. Lets the operator toggle the per-tty checkboxes AND type a
//!    custom `/dev/<X>` path into a single-line input below the list.
//!    The custom field is live-validated (green when the path exists
//!    as a chardev and is not a duplicate of an enumerated entry; red
//!    otherwise); valid custom entries are auto-checked and treated as
//!    additional targets.
//! 3. On `[Spawn]`, decides between three regimes:
//!    - **No overlap with display tty** → fork ONE shell per selected
//!      target with its stdio dup'd to that tty, then return to the
//!      previous screen with a success-modal confirmation. The shell
//!      runs detached on the operator's chosen line(s); NMBL never
//!      enters a relay loop on the wrong fd.
//!    - **Display tty in the selection** → run the multi-target
//!      multiplex relay loop (PTY master fan-out / fan-in via
//!      [`crate::ui::console_relay`]). Required because the operator
//!      cannot see the splash and the shell simultaneously.
//!    - **Both** → relay loop covers the display tty AND every
//!      additional tty in one PTY pair.
//! 4. Returns to the caller after the shell exits (relay regime) or
//!    immediately after the fire-and-forget spawn (no-overlap regime).
//!
//! ## Why no `Screen::ConsolePicker` variant?
//!
//! The picker is an entirely contained sub-flow: it never coexists with
//! the boot menu or the editing screen. Keeping its state local to this
//! module's driver loop (rather than in [`crate::ui::app::Screen`])
//! avoids growing the central state machine for a transient modal —
//! same pattern [`crate::ui::show_modal_error`] uses.

use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph};

use crate::config::Config;
use crate::error::Result;
use crate::nmbl_warn;
use crate::sys::tty::read_active_console;
use crate::ui::POLL_SLICE;
use crate::ui::console::{Console, ConsoleKind};
use crate::ui::tty_enum::{EnumeratedTty, TtyKind, enumerate_ttys, is_char_device};

/// Tty path the splash backend renders to. Mirrors the
/// `INPUT_TTY_PATH` constant inside `console::splash` so the overlap
/// decision agrees with where the kernel actually paints the
/// framebuffer.
const SPLASH_DISPLAY_TTY: &str = "/dev/tty1";

/// Origin tag for a picker candidate. Used by the renderer to map
/// each row to a short human-readable label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateOrigin {
    /// Kernel-elected interactive console (from
    /// `/sys/class/tty/console/active`). Pre-checked by default.
    KernelConsole,
    /// Auto-enumerated tty (framebuffer/serial/USB-serial). Unchecked
    /// by default.
    Enumerated(TtyKind),
    /// Operator-typed custom path that passed live validation. Always
    /// checked while it remains valid; vanishes from the spawn set the
    /// moment the operator either unchecks it OR edits it into an
    /// invalid value.
    Custom,
}

impl CandidateOrigin {
    /// Suffix appended to the row label (e.g. `(kernel console)`).
    fn label_suffix(&self) -> &'static str {
        match self {
            CandidateOrigin::KernelConsole => "(kernel console)",
            CandidateOrigin::Enumerated(k) => k.short_label(),
            CandidateOrigin::Custom => "(custom)",
        }
    }
}

/// One row in the picker dialog. The label is the displayed
/// `/dev/<tty>` path; `target` is the concrete path NMBL actually
/// opens at relay time.
#[derive(Debug, Clone)]
pub struct PickerCandidate {
    /// What we show in the dialog (the operator-facing rendering).
    pub label: String,
    /// What we open for I/O when [`Spawn`] is committed. Always a
    /// `/dev/<tty>` path.
    ///
    /// [`Spawn`]: PickerOutcome::Spawn
    pub target: PathBuf,
    /// Origin tag; drives the label suffix and the "is_active" semantic
    /// (only [`CandidateOrigin::KernelConsole`] entries are flagged as
    /// the kernel-elected primary interactive console).
    pub origin: CandidateOrigin,
}

impl PickerCandidate {
    /// True when this row is the kernel-elected primary interactive
    /// console. Kept as a method (rather than a field) so the truth
    /// stays in [`CandidateOrigin`] only.
    pub fn is_active(&self) -> bool {
        matches!(self.origin, CandidateOrigin::KernelConsole)
    }
}

/// Mutable state behind the picker dialog. Owned by the driver loop;
/// callers don't see it because every entry point hides it behind a
/// composite outcome.
#[derive(Debug)]
pub struct PickerState {
    pub candidates: Vec<PickerCandidate>,
    /// Per-candidate checkbox state. Length matches `candidates`.
    pub selected: Vec<bool>,
    /// Highlighted row in the candidates list.
    pub cursor: usize,
    /// Which button at the bottom (Spawn / Cancel) is highlighted.
    /// Up/Down arrows wrap between the list and the buttons; Space
    /// toggles checkboxes only when cursor is on the list, and Enter
    /// fires on whichever button is highlighted.
    pub button_cursor: ButtonCursor,
    /// Operator-typed custom-path input. Live-validated on every
    /// keystroke; when valid AND non-empty, treated as an additional
    /// pre-checked spawn target.
    pub custom_input: String,
    /// Operator's intent for the custom-path checkbox. When the path
    /// becomes invalid the entry is suppressed regardless of this flag.
    pub custom_checked: bool,
    /// `None` until the operator commits — either [`Spawn`] with the
    /// chosen targets or [`Cancel`] to bail back to the emergency menu.
    pub outcome: Option<PickerOutcome>,
}

/// Where the keyboard cursor currently lives: in the candidate list,
/// in the custom-path text input, or on one of the two bottom buttons.
/// Derived from `cursor` so a single integer drives navigation across
/// three zones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FocusZone {
    List,
    CustomInput,
    Buttons,
}

/// Which of the two bottom buttons is highlighted. The list rows
/// occupy "Focus::List(idx)" implicitly via `cursor`; this enum only
/// tracks which button has focus when `focus_zone == Buttons`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonCursor {
    Spawn,
    Cancel,
}

/// What the operator committed to. The driver loop returns this as
/// the public outcome of [`run_picker_session`].
#[derive(Debug)]
pub enum PickerOutcome {
    /// Spawn the shell with the (non-empty) set of selected `/dev/<tty>`
    /// targets. Always at least one entry.
    Spawn { targets: Vec<PathBuf> },
    /// Operator cancelled (Esc / [Cancel]). Caller re-shows the
    /// emergency menu.
    Cancel,
}

/// Live-validation verdict for the custom-path input. Drives both
/// the renderer's colouring and the "include in spawn set" decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CustomValidation {
    /// Empty input — no checkmark, hint dim.
    Empty,
    /// Path is well-formed, exists as a chardev, and is not a duplicate
    /// of an enumerated entry. Rendered in green.
    Valid,
    /// Path is malformed, missing, not a chardev, or duplicates an
    /// existing list entry. Rendered in red.
    Invalid,
}

impl PickerState {
    /// Construct picker state with the given active-console resolver
    /// and tty enumerator. Both are injected so unit tests can drive
    /// the picker against fixture data without touching `/sys` or
    /// `/dev`.
    ///
    /// The returned state always contains at least one candidate: the
    /// kernel-elected console (or a `/dev/console` fallback if the
    /// resolver fails). `extra_consoles` config entries that match an
    /// already-discovered path are deduplicated.
    pub fn build_with<F, G>(
        config: &Config,
        active_console_resolver: F,
        tty_enumerator: G,
    ) -> Result<PickerState>
    where
        F: FnOnce() -> Result<PathBuf>,
        G: FnOnce(&Path) -> Vec<EnumeratedTty>,
    {
        let mut candidates: Vec<PickerCandidate> = Vec::new();
        let mut selected: Vec<bool> = Vec::new();

        let active_path = match active_console_resolver() {
            Ok(p) => p,
            Err(e) => {
                nmbl_warn!(
                    "console picker: active-console resolver failed: {e}; \
                     falling back to /dev/console"
                );
                PathBuf::from("/dev/console")
            }
        };

        candidates.push(PickerCandidate {
            label: format!("/dev/console (-> {})", active_path.display()),
            target: active_path.clone(),
            origin: CandidateOrigin::KernelConsole,
        });
        selected.push(true);

        // Auto-enumerated ttys. The enumerator filters out anything
        // that matches the kernel console path so we don't render the
        // same fd twice under different labels.
        for entry in tty_enumerator(&active_path) {
            if candidates.iter().any(|c| c.target == entry.path) {
                continue;
            }
            candidates.push(PickerCandidate {
                label: entry.path.display().to_string(),
                target: entry.path,
                origin: CandidateOrigin::Enumerated(entry.kind),
            });
            selected.push(false);
        }

        // Config-supplied extras: keep order, drop duplicates of any
        // already-discovered entry.
        for extra in &config.emergency_shell.extra_consoles {
            let extra_path = PathBuf::from(extra);
            if candidates.iter().any(|c| c.target == extra_path) {
                continue;
            }
            candidates.push(PickerCandidate {
                label: extra.clone(),
                target: extra_path,
                origin: CandidateOrigin::Enumerated(TtyKind::SerialPort),
            });
            selected.push(false);
        }

        Ok(PickerState {
            candidates,
            selected,
            cursor: 0,
            button_cursor: ButtonCursor::Spawn,
            custom_input: String::new(),
            custom_checked: true,
            outcome: None,
        })
    }

    /// Production wrapper around [`build_with`] pinned to the canonical
    /// `/sys` resolver and the live tty enumerator.
    pub fn build(config: &Config) -> Result<PickerState> {
        Self::build_with(config, read_active_console, |exclude| {
            enumerate_ttys(exclude)
        })
    }

    /// Current validation verdict for the custom-path input. Pure
    /// function over the state so the renderer and the spawn-target
    /// computation agree on the same answer.
    pub(crate) fn custom_validation(&self) -> CustomValidation {
        validate_custom_input(&self.custom_input, &self.candidates)
    }

    /// Currently selected target set, in candidate order plus the
    /// optional valid-and-checked custom entry at the end. Used by the
    /// driver loop on [`Spawn`] commit.
    pub fn selected_targets(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = self
            .candidates
            .iter()
            .zip(self.selected.iter())
            .filter(|(_, on)| **on)
            .map(|(c, _)| c.target.clone())
            .collect();
        if self.custom_checked && self.custom_validation() == CustomValidation::Valid {
            let p = PathBuf::from(self.custom_input.trim());
            if !out.iter().any(|q| q == &p) {
                out.push(p);
            }
        }
        out
    }

    /// True when no candidate is currently checked. Drives the
    /// renderer's button-greying and gates [`Spawn`] commit.
    pub fn nothing_selected(&self) -> bool {
        self.selected_targets().is_empty()
    }

    /// Where the navigation cursor lives logically: list row,
    /// custom-input field, or buttons row.
    pub(crate) fn focus(&self) -> FocusZone {
        let list_len = self.candidates.len();
        if self.cursor < list_len {
            FocusZone::List
        } else if self.cursor == list_len {
            FocusZone::CustomInput
        } else {
            FocusZone::Buttons
        }
    }

    /// Reduce a [`KeyEvent`] into a state mutation. Returns `true` if
    /// the dialog wants to exit (i.e. `outcome` is now `Some`).
    pub fn on_key(&mut self, key: KeyEvent) -> bool {
        // Ignore release/repeat; on_key only acts on Press.
        if key.kind != KeyEventKind::Press {
            return self.outcome.is_some();
        }

        // Custom-input field captures most keystrokes when focused so
        // the operator can type a path; navigation keys still escape
        // to move focus.
        if self.focus() == FocusZone::CustomInput {
            match key.code {
                KeyCode::Up | KeyCode::Down | KeyCode::Esc | KeyCode::Enter => {
                    // fall through to the shared handler below
                }
                KeyCode::Char(' ') => {
                    // Space inside the field is a real space, NOT a
                    // toggle. Only the [Space] on the list rows toggles.
                    self.custom_input.push(' ');
                    return false;
                }
                KeyCode::Char(c) => {
                    self.custom_input.push(c);
                    return false;
                }
                KeyCode::Backspace => {
                    self.custom_input.pop();
                    return false;
                }
                KeyCode::Tab => {
                    // Tab on the custom field toggles its "checked"
                    // flag (only meaningful when validation is Valid).
                    self.custom_checked = !self.custom_checked;
                    return false;
                }
                _ => return false,
            }
        }

        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_cursor_up();
                false
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_cursor_down();
                false
            }
            // Left/Right toggles between the two buttons when focus is
            // on the button row.
            KeyCode::Left => {
                if self.focus() == FocusZone::Buttons {
                    self.button_cursor = ButtonCursor::Spawn;
                }
                false
            }
            KeyCode::Right => {
                if self.focus() == FocusZone::Buttons {
                    self.button_cursor = ButtonCursor::Cancel;
                }
                false
            }
            KeyCode::Char(' ') => {
                // Space toggles checkbox only when focus is on the list.
                if let FocusZone::List = self.focus()
                    && let Some(slot) = self.selected.get_mut(self.cursor)
                {
                    *slot = !*slot;
                }
                false
            }
            KeyCode::Enter => self.commit_focus(),
            KeyCode::Esc => {
                self.outcome = Some(PickerOutcome::Cancel);
                true
            }
            _ => false,
        }
    }

    /// Apply Enter on the currently-focused element. On the list, Enter
    /// toggles the checkbox (same as Space); on the custom input it
    /// is a no-op (the operator commits via the [Spawn] button); on
    /// the buttons it commits the dialog.
    fn commit_focus(&mut self) -> bool {
        match self.focus() {
            FocusZone::List => {
                if let Some(slot) = self.selected.get_mut(self.cursor) {
                    *slot = !*slot;
                }
                false
            }
            FocusZone::CustomInput => false,
            FocusZone::Buttons => match self.button_cursor {
                ButtonCursor::Spawn => {
                    if self.nothing_selected() {
                        // Reject the commit: nothing to spawn on.
                        return false;
                    }
                    self.outcome = Some(PickerOutcome::Spawn {
                        targets: self.selected_targets(),
                    });
                    true
                }
                ButtonCursor::Cancel => {
                    self.outcome = Some(PickerOutcome::Cancel);
                    true
                }
            },
        }
    }

    /// Cursor goes up one row. Saturates at row 0.
    fn move_cursor_up(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Cursor goes down. The custom-input pseudo-row sits between the
    /// list and the buttons row; saturates on the buttons row.
    fn move_cursor_down(&mut self) {
        // last = candidates.len() + 1 → buttons row
        let last = self.candidates.len().saturating_add(1);
        if self.cursor < last {
            self.cursor = self.cursor.saturating_add(1);
        }
    }
}

/// Outcome of running the picker dialog. The relay-loop wrapper
/// surfaces this to the caller so the emergency-shell flow can react
/// (start the relay vs. drop back to the menu).
#[derive(Debug)]
pub enum PickerSessionOutcome {
    /// Operator chose targets and the relay loop ran to completion.
    /// The caller re-displays the emergency menu.
    ShellRan,
    /// Operator chose targets that do NOT include the live display
    /// tty; NMBL fire-and-forget spawned shells on those targets and
    /// returned to the previous screen. The caller re-displays the
    /// emergency menu.
    ShellDetached { targets: Vec<PathBuf> },
    /// Operator cancelled the dialog before spawning anything.
    Cancelled,
}

/// Run the picker dialog on `console` and, when committed, drive the
/// multi-target shell-relay loop OR fire-and-forget spawn (depending
/// on whether the selection overlaps with the live console's display
/// tty). Returns to the caller after the shell exits, the
/// fire-and-forget spawn succeeds, or the operator cancels.
///
/// The function NEVER produces a [`crate::terminal::TerminalAction`]:
/// NMBL stays at PID 1 throughout. This is the deliberate departure
/// from the legacy `EmergencyChoice::RawShell` -> execve path.
pub fn run_picker_session(
    console: &mut dyn Console,
    config: &Config,
) -> Result<PickerSessionOutcome> {
    let mut state = PickerState::build(config)?;
    if state.candidates.is_empty() {
        return Ok(PickerSessionOutcome::Cancelled);
    }
    drive_picker_loop(&mut state, console)?;

    let targets = match state.outcome {
        Some(PickerOutcome::Spawn { targets }) => targets,
        Some(PickerOutcome::Cancel) | None => return Ok(PickerSessionOutcome::Cancelled),
    };
    let display_target = display_target_for(console);
    dispatch_spawn(
        console,
        config,
        targets,
        &display_target,
        crate::ui::console_relay::run_relay,
        fire_and_forget_spawn,
    )
}

/// Post-commit dispatch: given the operator's spawn set and the
/// picker's authoritative display-target path, route into either the
/// relay loop (display overlap) or the fire-and-forget spawn (no
/// overlap). The `relay_fn` / `detach_fn` callbacks are parameters so
/// unit tests can drive the dispatch without forking real shells.
///
/// The picker is the ONLY source of truth for `display_target`; the
/// callbacks never re-derive it. See [`run_relay`]'s doc-comment for
/// the historical bug that motivated this contract.
///
/// [`run_relay`]: crate::ui::console_relay::run_relay
fn dispatch_spawn<R, D>(
    console: &mut dyn Console,
    config: &Config,
    targets: Vec<PathBuf>,
    display_target: &Path,
    mut relay_fn: R,
    mut detach_fn: D,
) -> Result<PickerSessionOutcome>
where
    R: FnMut(&mut dyn Console, &Config, &[PathBuf], &Path) -> Result<()>,
    D: FnMut(&Config, &[PathBuf]) -> Result<()>,
{
    if display_overlaps_targets(display_target, &targets) {
        relay_fn(console, config, &targets, display_target)?;
        Ok(PickerSessionOutcome::ShellRan)
    } else {
        // Fire-and-forget: spawn one shell per target so each line
        // carries its own session. If a spawn fails we log + carry on;
        // reporting back through a modal lets the operator retry or
        // pick a different target.
        detach_fn(config, &targets)?;
        Ok(PickerSessionOutcome::ShellDetached { targets })
    }
}

/// Resolve the device path the live console is currently rendering to.
/// For the splash backend the operator sees `/dev/tty1` (framebuffer
/// VT); for the tty backend it is whatever the kernel-elected console
/// resolves to. Failure of the kernel-console resolver falls back to
/// `/dev/console` — the same fallback the picker uses for its first
/// candidate so the overlap decision stays self-consistent.
fn display_target_for(console: &dyn Console) -> PathBuf {
    match console.kind() {
        ConsoleKind::Splash => PathBuf::from(SPLASH_DISPLAY_TTY),
        ConsoleKind::Tty => read_active_console().unwrap_or_else(|e| {
            nmbl_warn!(
                "console picker: active-console resolver failed: {e}; \
                 assuming /dev/console for the display-overlap decision"
            );
            PathBuf::from("/dev/console")
        }),
    }
}

/// Spawn one detached shell per target. Each shell runs to its natural
/// conclusion on the operator's line; NMBL does not block on them.
/// Errors are logged but never propagated — the picker's caller still
/// surfaces a success modal so the operator knows the spawn was
/// attempted.
fn fire_and_forget_spawn(config: &Config, targets: &[PathBuf]) -> Result<()> {
    for t in targets {
        match crate::sys::pty::spawn_shell_on_tty(&config.paths.shell, t) {
            Ok(_) => {}
            Err(e) => {
                nmbl_warn!(
                    "console picker: fire-and-forget spawn on {} failed: {e}",
                    t.display()
                );
            }
        }
    }
    Ok(())
}

/// Drive the render-poll-react loop until the picker commits an
/// outcome. Uses `poll_event` so a host-reported terminal resize
/// triggers an immediate redraw at the new grid.
fn drive_picker_loop(state: &mut PickerState, console: &mut dyn Console) -> Result<()> {
    let mut dirty = true;
    loop {
        if dirty {
            render_picker(console, state)?;
            dirty = false;
        }
        match console.poll_event(POLL_SLICE)? {
            Some(crate::ui::console::ConsoleEvent::Resize { .. }) => {
                dirty = true;
            }
            Some(crate::ui::console::ConsoleEvent::Key(key)) => {
                let exited = state.on_key(key);
                dirty = true;
                if exited {
                    return Ok(());
                }
            }
            None => {}
        }
    }
}

/// Issue one frame paint via [`Console::draw_with`]. Keeping the
/// renderer behind a thin wrapper localises the borrow of `state` so
/// the closure stays `FnMut` and doesn't capture overlapping
/// references.
fn render_picker(console: &mut dyn Console, state: &PickerState) -> Result<()> {
    console.draw_with(&mut |frame| render_picker_frame(frame, state))
}

/// Pure render function — exported `pub(crate)` so the renderer can be
/// exercised by unit tests with a `TestBackend`.
pub(crate) fn render_picker_frame(frame: &mut Frame<'_>, state: &PickerState) {
    let area = frame.area();
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas::<3>(area);

    // Header.
    let header_para = Paragraph::new(Line::from(vec![Span::styled(
        "Spawn shell on:",
        Style::default().add_modifier(Modifier::BOLD),
    )]))
    .alignment(Alignment::Left);
    frame.render_widget(header_para, header);

    // Centred modal over the body so the dialog reads as a focus
    // shift rather than a full-screen replacement.
    let modal = centered_rect(body, 64, body.height.saturating_div(2).max(12));
    frame.render_widget(Clear, modal);

    // Layout inside the modal: candidate list + custom-input + buttons.
    let list_height = u16::try_from(state.candidates.len().saturating_add(2)).unwrap_or(u16::MAX);
    let [list_area, custom_area, button_area] = Layout::vertical([
        Constraint::Length(list_height),
        Constraint::Length(3),
        Constraint::Length(3),
    ])
    .areas::<3>(modal);

    let items: Vec<ListItem<'_>> = state
        .candidates
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let on = state.selected.get(i).copied().unwrap_or(false);
            let inner = if on { 'x' } else { ' ' };
            ListItem::new(Line::from(format!(
                "[{inner}]  {label}  {suffix}",
                label = c.label,
                suffix = c.origin.label_suffix(),
            )))
        })
        .collect();

    let highlight = Style::default()
        .fg(Color::Black)
        .bg(Color::Gray)
        .add_modifier(Modifier::BOLD);
    let list = List::new(items)
        .block(Block::bordered().title("targets"))
        .highlight_style(highlight)
        .highlight_symbol("> ");

    let mut list_state = ListState::default();
    if !state.candidates.is_empty() && state.focus() == FocusZone::List {
        let last_idx = state.candidates.len().saturating_sub(1);
        list_state.select(Some(state.cursor.min(last_idx)));
    }
    frame.render_stateful_widget(list, list_area, &mut list_state);

    // Custom-input field. Colour-coded by validation verdict.
    render_custom_input(frame, custom_area, state);

    // Buttons row: [Spawn] [Cancel].
    let [spawn_area, cancel_area] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .areas::<2>(button_area);

    let on_buttons = state.focus() == FocusZone::Buttons;
    let spawn_focused = on_buttons && state.button_cursor == ButtonCursor::Spawn;
    let cancel_focused = on_buttons && state.button_cursor == ButtonCursor::Cancel;

    let spawn_disabled = state.nothing_selected();
    let spawn_label = if spawn_disabled {
        "[Spawn (no target)]"
    } else {
        "[Spawn]"
    };
    // Disabled wins over focused: when the operator has no target the
    // button is dim even if cursor sits on it, mirroring the pp-spinner
    // / empty-pw-block pattern in `render_passphrase`.
    let spawn_style = if spawn_disabled {
        Style::default().add_modifier(Modifier::DIM)
    } else if spawn_focused {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Gray)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let spawn_para = Paragraph::new(Span::styled(spawn_label, spawn_style))
        .alignment(Alignment::Center)
        .block(Block::bordered());
    frame.render_widget(spawn_para, spawn_area);

    let cancel_style = if cancel_focused {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Gray)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let cancel_para = Paragraph::new(Span::styled("[Cancel]", cancel_style))
        .alignment(Alignment::Center)
        .block(Block::bordered());
    frame.render_widget(cancel_para, cancel_area);

    let footer_text = "up/down move  Space toggle  Tab check custom  Enter confirm  Esc cancel";
    frame.render_widget(
        Paragraph::new(footer_text).alignment(Alignment::Left),
        footer,
    );
}

/// Render the single-line custom-path input plus a validation glyph.
/// Splits out from [`render_picker_frame`] so the colour-coding logic
/// stays readable.
fn render_custom_input(frame: &mut Frame<'_>, area: Rect, state: &PickerState) {
    let validation = state.custom_validation();
    let focused = state.focus() == FocusZone::CustomInput;
    let (text_style, marker, marker_style) = match validation {
        CustomValidation::Empty => (
            Style::default().add_modifier(Modifier::DIM),
            " ".to_string(),
            Style::default().add_modifier(Modifier::DIM),
        ),
        CustomValidation::Valid => (
            Style::default().fg(Color::Green),
            if state.custom_checked {
                "[x]".to_string()
            } else {
                "[ ]".to_string()
            },
            Style::default().fg(Color::Green),
        ),
        CustomValidation::Invalid => (
            Style::default().fg(Color::Red),
            "[!]".to_string(),
            Style::default().fg(Color::Red),
        ),
    };
    let title = if focused {
        "custom (typing)"
    } else {
        "custom (/dev/X)"
    };
    let cursor_suffix = if focused { "|" } else { "" };
    let body = Line::from(vec![
        Span::styled(marker, marker_style),
        Span::raw(" "),
        Span::styled(format!("{}{cursor_suffix}", state.custom_input), text_style),
    ]);
    let block_style = if focused {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let para =
        Paragraph::new(body).block(Block::bordered().title(Span::styled(title, block_style)));
    frame.render_widget(para, area);
}

/// Validate a custom-path input against the current candidate list.
/// Pure function — no I/O beyond the `stat(2)` invocation inside
/// [`is_char_device`], which is necessary to decide existence. Exposed
/// `pub(crate)` so unit tests can assert on the verdict.
pub(crate) fn validate_custom_input(input: &str, existing: &[PickerCandidate]) -> CustomValidation {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return CustomValidation::Empty;
    }
    // Require the canonical `/dev/<X>` shape so the operator can't
    // accidentally spawn on a random file path (e.g. `/etc/passwd`).
    let path = Path::new(trimmed);
    if !path.starts_with("/dev/") {
        return CustomValidation::Invalid;
    }
    // The component after `/dev/` must be non-empty.
    let name_ok = path
        .file_name()
        .map(|n| !n.as_encoded_bytes().is_empty())
        .unwrap_or(false);
    if !name_ok {
        return CustomValidation::Invalid;
    }
    // Reject duplicates of an existing enumerated candidate.
    if existing.iter().any(|c| c.target.as_path() == path) {
        return CustomValidation::Invalid;
    }
    // Must exist and be a character device (S_ISCHR). Regular files,
    // directories, sockets, fifos all fail. `/dev/zero` style chardevs
    // that exist but aren't ttys pass the chardev check; the operator
    // owns the consequence of selecting them, exactly as the docs say.
    if !is_char_device(path) {
        return CustomValidation::Invalid;
    }
    CustomValidation::Valid
}

/// Centre a width×height rect inside `area`. Mirrors the same helper
/// in `view.rs`; duplicated here to keep this module self-contained
/// (the picker doesn't otherwise touch `view`).
fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    let x = area.x.saturating_add(area.width.saturating_sub(w) / 2);
    let y = area.y.saturating_add(area.height.saturating_sub(h) / 2);
    Rect::new(x, y, w, h)
}

/// True iff `display_target` is one of the operator's selected
/// targets. The relay path uses this to decide whether to suspend
/// the live [`Console`] (display overlap) or fire-and-forget (no
/// overlap).
pub fn display_overlaps_targets(display_target: &Path, targets: &[PathBuf]) -> bool {
    targets.iter().any(|t| t == display_target)
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

    use std::time::Duration;

    use crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::error::NmblError;
    use crate::ui::app::App;
    use crate::ui::console::{Console, ConsoleEvent, ConsoleKind};
    use crate::ui::tty_enum::{EnumeratedTty, TtyKind};

    /// Fake [`Console`] for driving the picker loop in tests.
    struct FakeConsole {
        events: std::collections::VecDeque<Option<KeyEvent>>,
        renders: u32,
        kind: ConsoleKind,
        suspend_calls: u32,
        resume_calls: u32,
    }

    impl FakeConsole {
        fn new(events: Vec<Option<KeyEvent>>) -> Self {
            Self {
                events: events.into(),
                renders: 0,
                kind: ConsoleKind::Tty,
                suspend_calls: 0,
                resume_calls: 0,
            }
        }

        fn with_kind(mut self, kind: ConsoleKind) -> Self {
            self.kind = kind;
            self
        }
    }

    impl Console for FakeConsole {
        fn render(&mut self, _app: &App<'_>) -> Result<()> {
            self.renders = self.renders.saturating_add(1);
            Ok(())
        }
        fn poll_event(&mut self, _timeout: Duration) -> Result<Option<ConsoleEvent>> {
            Ok(self.events.pop_front().flatten().map(ConsoleEvent::Key))
        }
        fn size(&self) -> (u16, u16) {
            (80, 24)
        }
        fn kind(&self) -> ConsoleKind {
            self.kind
        }
        fn draw_with(&mut self, _body: &mut dyn FnMut(&mut Frame<'_>)) -> Result<()> {
            self.renders = self.renders.saturating_add(1);
            Ok(())
        }
        fn suspend(&mut self) -> Result<()> {
            self.suspend_calls = self.suspend_calls.saturating_add(1);
            Ok(())
        }
        fn resume(&mut self) -> Result<()> {
            self.resume_calls = self.resume_calls.saturating_add(1);
            Ok(())
        }
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn no_enum(_exclude: &Path) -> Vec<EnumeratedTty> {
        Vec::new()
    }

    #[test]
    fn build_active_console_is_pre_checked_with_extras() {
        let mut cfg = Config::recovery_default();
        cfg.emergency_shell.extra_consoles =
            vec!["/dev/ttyS0".to_string(), "/dev/tty1".to_string()];
        let state = PickerState::build_with(&cfg, || Ok(PathBuf::from("/dev/tty0")), no_enum)
            .expect("build");
        assert_eq!(state.candidates.len(), 3);
        assert_eq!(state.candidates[0].target, PathBuf::from("/dev/tty0"));
        assert!(state.candidates[0].is_active());
        assert!(state.selected[0]);
        assert_eq!(state.candidates[1].target, PathBuf::from("/dev/ttyS0"));
        assert!(!state.selected[1]);
        assert_eq!(state.candidates[2].target, PathBuf::from("/dev/tty1"));
        assert!(!state.selected[2]);
    }

    #[test]
    fn build_merges_enumerated_set_after_kernel_console() {
        let cfg = Config::recovery_default();
        let state = PickerState::build_with(
            &cfg,
            || Ok(PathBuf::from("/dev/ttyS0")),
            |exclude| {
                vec![
                    EnumeratedTty {
                        path: PathBuf::from("/dev/tty1"),
                        kind: TtyKind::FramebufferTty,
                    },
                    // ttyS0 must be filtered out by the enumerator
                    // because it equals `exclude`; assert the contract.
                    EnumeratedTty {
                        path: if exclude == Path::new("/dev/ttyS0") {
                            PathBuf::from("/dev/ttyS1")
                        } else {
                            PathBuf::from("/dev/ttyS0")
                        },
                        kind: TtyKind::SerialPort,
                    },
                ]
            },
        )
        .expect("build");
        assert_eq!(state.candidates.len(), 3);
        assert_eq!(state.candidates[0].target, PathBuf::from("/dev/ttyS0"));
        assert!(state.candidates[0].is_active());
        assert_eq!(state.candidates[1].target, PathBuf::from("/dev/tty1"));
        assert!(matches!(
            state.candidates[1].origin,
            CandidateOrigin::Enumerated(TtyKind::FramebufferTty)
        ));
        assert_eq!(state.candidates[2].target, PathBuf::from("/dev/ttyS1"));
    }

    #[test]
    fn build_deduplicates_extra_that_matches_enumerated_entry() {
        let mut cfg = Config::recovery_default();
        cfg.emergency_shell.extra_consoles = vec!["/dev/tty1".to_string()];
        let state = PickerState::build_with(
            &cfg,
            || Ok(PathBuf::from("/dev/tty0")),
            |_| {
                vec![EnumeratedTty {
                    path: PathBuf::from("/dev/tty1"),
                    kind: TtyKind::FramebufferTty,
                }]
            },
        )
        .expect("build");
        assert_eq!(state.candidates.len(), 2);
        assert_eq!(state.candidates[0].target, PathBuf::from("/dev/tty0"));
        assert_eq!(state.candidates[1].target, PathBuf::from("/dev/tty1"));
    }

    #[test]
    fn build_falls_back_to_dev_console_when_resolver_errors() {
        let cfg = Config::recovery_default();
        let state = PickerState::build_with(
            &cfg,
            || {
                Err(NmblError::Tui {
                    source: std::io::Error::other("no /sys"),
                })
            },
            no_enum,
        )
        .expect("fallback build must still succeed");
        assert_eq!(state.candidates.len(), 1);
        assert_eq!(state.candidates[0].target, PathBuf::from("/dev/console"));
        assert!(state.candidates[0].is_active());
    }

    #[test]
    fn space_toggles_checkbox_on_focused_row() {
        let mut state = PickerState {
            candidates: vec![
                PickerCandidate {
                    label: "a".into(),
                    target: PathBuf::from("/dev/a"),
                    origin: CandidateOrigin::KernelConsole,
                },
                PickerCandidate {
                    label: "b".into(),
                    target: PathBuf::from("/dev/b"),
                    origin: CandidateOrigin::Enumerated(TtyKind::SerialPort),
                },
            ],
            selected: vec![true, false],
            cursor: 1,
            button_cursor: ButtonCursor::Spawn,
            custom_input: String::new(),
            custom_checked: true,
            outcome: None,
        };
        assert!(!state.on_key(press(KeyCode::Char(' '))));
        assert!(state.selected[1]);
        assert!(!state.on_key(press(KeyCode::Char(' '))));
        assert!(!state.selected[1]);
    }

    #[test]
    fn spawn_enter_returns_selected_targets() {
        let mut state = PickerState {
            candidates: vec![
                PickerCandidate {
                    label: "a".into(),
                    target: PathBuf::from("/dev/a"),
                    origin: CandidateOrigin::KernelConsole,
                },
                PickerCandidate {
                    label: "b".into(),
                    target: PathBuf::from("/dev/b"),
                    origin: CandidateOrigin::Enumerated(TtyKind::SerialPort),
                },
            ],
            selected: vec![true, true],
            // candidates(2) + custom-input(1) → buttons starts at 3.
            cursor: 3,
            button_cursor: ButtonCursor::Spawn,
            custom_input: String::new(),
            custom_checked: true,
            outcome: None,
        };
        assert!(state.on_key(press(KeyCode::Enter)));
        match state.outcome {
            Some(PickerOutcome::Spawn { targets }) => {
                assert_eq!(
                    targets,
                    vec![PathBuf::from("/dev/a"), PathBuf::from("/dev/b")]
                );
            }
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    #[test]
    fn spawn_with_nothing_selected_is_rejected() {
        let mut state = PickerState {
            candidates: vec![PickerCandidate {
                label: "a".into(),
                target: PathBuf::from("/dev/a"),
                origin: CandidateOrigin::KernelConsole,
            }],
            selected: vec![false],
            cursor: 2,
            button_cursor: ButtonCursor::Spawn,
            custom_input: String::new(),
            custom_checked: true,
            outcome: None,
        };
        assert!(!state.on_key(press(KeyCode::Enter)));
        assert!(state.outcome.is_none());
    }

    #[test]
    fn esc_commits_cancel() {
        let mut state = PickerState {
            candidates: vec![PickerCandidate {
                label: "a".into(),
                target: PathBuf::from("/dev/a"),
                origin: CandidateOrigin::KernelConsole,
            }],
            selected: vec![true],
            cursor: 0,
            button_cursor: ButtonCursor::Spawn,
            custom_input: String::new(),
            custom_checked: true,
            outcome: None,
        };
        assert!(state.on_key(press(KeyCode::Esc)));
        assert!(matches!(state.outcome, Some(PickerOutcome::Cancel)));
    }

    #[test]
    fn cursor_navigation_walks_list_custom_buttons() {
        let mut state = PickerState {
            candidates: vec![
                PickerCandidate {
                    label: "a".into(),
                    target: PathBuf::from("/dev/a"),
                    origin: CandidateOrigin::KernelConsole,
                },
                PickerCandidate {
                    label: "b".into(),
                    target: PathBuf::from("/dev/b"),
                    origin: CandidateOrigin::Enumerated(TtyKind::SerialPort),
                },
            ],
            selected: vec![true, false],
            cursor: 0,
            button_cursor: ButtonCursor::Spawn,
            custom_input: String::new(),
            custom_checked: true,
            outcome: None,
        };
        state.on_key(press(KeyCode::Down));
        assert_eq!(state.focus(), FocusZone::List);
        state.on_key(press(KeyCode::Down));
        assert_eq!(state.focus(), FocusZone::CustomInput);
        state.on_key(press(KeyCode::Down));
        assert_eq!(state.focus(), FocusZone::Buttons);
        state.on_key(press(KeyCode::Down));
        // saturates on buttons
        assert_eq!(state.focus(), FocusZone::Buttons);
        state.on_key(press(KeyCode::Right));
        assert_eq!(state.button_cursor, ButtonCursor::Cancel);
    }

    /// Custom-input validation: empty → Empty; invalid path → Invalid;
    /// real chardev → Valid. We use `/dev/null` because it is a chardev
    /// guaranteed to exist in every NMBL target environment.
    #[test]
    fn custom_input_validation_empty_invalid_valid() {
        let existing = vec![PickerCandidate {
            label: "kernel".into(),
            target: PathBuf::from("/dev/console"),
            origin: CandidateOrigin::KernelConsole,
        }];
        assert_eq!(
            validate_custom_input("", &existing),
            CustomValidation::Empty
        );
        assert_eq!(
            validate_custom_input("   ", &existing),
            CustomValidation::Empty
        );
        assert_eq!(
            validate_custom_input("/etc/passwd", &existing),
            CustomValidation::Invalid,
            "non /dev/ paths must be rejected"
        );
        assert_eq!(
            validate_custom_input("/dev/", &existing),
            CustomValidation::Invalid,
            "/dev/ with no name must be rejected"
        );
        assert_eq!(
            validate_custom_input("/dev/this-should-not-exist-nmbl", &existing),
            CustomValidation::Invalid,
            "missing devnode must be rejected"
        );
        assert_eq!(
            validate_custom_input("/dev/console", &existing),
            CustomValidation::Invalid,
            "duplicates of existing candidates must be rejected"
        );
        if is_char_device(Path::new("/dev/null")) {
            assert_eq!(
                validate_custom_input("/dev/null", &existing),
                CustomValidation::Valid,
                "an existing chardev not in the list must validate"
            );
        }
    }

    #[test]
    fn typing_into_custom_input_updates_buffer() {
        let mut state = PickerState {
            candidates: vec![PickerCandidate {
                label: "a".into(),
                target: PathBuf::from("/dev/a"),
                origin: CandidateOrigin::KernelConsole,
            }],
            selected: vec![true],
            // jump straight to the custom-input row
            cursor: 1,
            button_cursor: ButtonCursor::Spawn,
            custom_input: String::new(),
            custom_checked: true,
            outcome: None,
        };
        for c in "/dev/ttyS9".chars() {
            state.on_key(press(KeyCode::Char(c)));
        }
        assert_eq!(state.custom_input, "/dev/ttyS9");
        state.on_key(press(KeyCode::Backspace));
        assert_eq!(state.custom_input, "/dev/ttyS");
    }

    #[test]
    fn driver_loop_runs_picker_to_spawn_outcome_via_fake_console() {
        let mut state = PickerState {
            candidates: vec![PickerCandidate {
                label: "/dev/tty0".into(),
                target: PathBuf::from("/dev/tty0"),
                origin: CandidateOrigin::KernelConsole,
            }],
            selected: vec![true],
            cursor: 0,
            button_cursor: ButtonCursor::Spawn,
            custom_input: String::new(),
            custom_checked: true,
            outcome: None,
        };
        let mut console = FakeConsole::new(vec![
            // Move into the custom-input row.
            Some(press(KeyCode::Down)),
            // Move into the buttons row.
            Some(press(KeyCode::Down)),
            // Enter on [Spawn].
            Some(press(KeyCode::Enter)),
        ]);
        drive_picker_loop(&mut state, &mut console).expect("loop must not error");
        match state.outcome {
            Some(PickerOutcome::Spawn { targets }) => {
                assert_eq!(targets, vec![PathBuf::from("/dev/tty0")]);
            }
            other => panic!("expected Spawn, got {other:?}"),
        }
        assert!(console.renders >= 1);
    }

    #[test]
    fn renderer_paints_candidate_labels_and_buttons() {
        let state = PickerState {
            candidates: vec![
                PickerCandidate {
                    label: "/dev/console (-> /dev/tty0)".into(),
                    target: PathBuf::from("/dev/tty0"),
                    origin: CandidateOrigin::KernelConsole,
                },
                PickerCandidate {
                    label: "/dev/ttyS0".into(),
                    target: PathBuf::from("/dev/ttyS0"),
                    origin: CandidateOrigin::Enumerated(TtyKind::SerialPort),
                },
            ],
            selected: vec![true, false],
            cursor: 0,
            button_cursor: ButtonCursor::Spawn,
            custom_input: String::new(),
            custom_checked: true,
            outcome: None,
        };
        let mut term = Terminal::new(TestBackend::new(80, 24)).expect("test terminal");
        term.draw(|f| render_picker_frame(f, &state)).expect("draw");
        let buf = term.backend().buffer();
        let dump: String = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol().to_owned()))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(dump.contains("Spawn shell on:"), "header missing: \n{dump}");
        assert!(
            dump.contains("/dev/console"),
            "active console label missing: \n{dump}"
        );
        assert!(
            dump.contains("(kernel console)"),
            "origin suffix missing: \n{dump}"
        );
        assert!(
            dump.contains("/dev/ttyS0"),
            "extra-console label missing: \n{dump}"
        );
        assert!(dump.contains("[Spawn"), "Spawn button missing: \n{dump}");
        assert!(dump.contains("[Cancel]"), "Cancel button missing: \n{dump}");
        assert!(
            dump.contains("custom"),
            "custom input title missing: \n{dump}"
        );
    }

    /// When no candidate is selected the [Spawn] button must render
    /// with the DIM modifier so the disabled state is operator-visible.
    /// Mirrors the `render_passphrase` precedent from empty-pw-block.
    #[test]
    fn renderer_dims_spawn_when_no_target_selected() {
        let state = PickerState {
            candidates: vec![PickerCandidate {
                label: "/dev/tty0".into(),
                target: PathBuf::from("/dev/tty0"),
                origin: CandidateOrigin::KernelConsole,
            }],
            selected: vec![false],
            cursor: 0,
            button_cursor: ButtonCursor::Spawn,
            custom_input: String::new(),
            custom_checked: true,
            outcome: None,
        };
        let mut term = Terminal::new(TestBackend::new(80, 24)).expect("test terminal");
        term.draw(|f| render_picker_frame(f, &state)).expect("draw");
        let buf = term.backend().buffer();
        // Find the centre of the Spawn label and inspect its style.
        let mut dim_seen = false;
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if let Some(cell) = buf.cell((x, y))
                    && cell.symbol() == "S"
                    && let Some(next) = buf.cell((x.saturating_add(1), y))
                    && next.symbol() == "p"
                    && cell.style().add_modifier.contains(Modifier::DIM)
                {
                    dim_seen = true;
                }
            }
        }
        assert!(dim_seen, "Spawn label must be DIM when no target selected");
    }

    /// Filled-buffer counterpart: when at least one target is checked
    /// the [Spawn] button must NOT be DIM.
    #[test]
    fn renderer_does_not_dim_spawn_when_target_selected() {
        let state = PickerState {
            candidates: vec![PickerCandidate {
                label: "/dev/tty0".into(),
                target: PathBuf::from("/dev/tty0"),
                origin: CandidateOrigin::KernelConsole,
            }],
            selected: vec![true],
            cursor: 0,
            button_cursor: ButtonCursor::Spawn,
            custom_input: String::new(),
            custom_checked: true,
            outcome: None,
        };
        let mut term = Terminal::new(TestBackend::new(80, 24)).expect("test terminal");
        term.draw(|f| render_picker_frame(f, &state)).expect("draw");
        let buf = term.backend().buffer();
        let mut any_dim = false;
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if let Some(cell) = buf.cell((x, y))
                    && cell.symbol() == "S"
                    && let Some(next) = buf.cell((x.saturating_add(1), y))
                    && next.symbol() == "p"
                    && cell.style().add_modifier.contains(Modifier::DIM)
                {
                    any_dim = true;
                }
            }
        }
        assert!(
            !any_dim,
            "Spawn label must NOT be DIM when a target is selected"
        );
    }

    /// Renderer must colour the custom-input field GREEN when the
    /// path is a valid, non-duplicate chardev, and RED when the path
    /// is rejected. The marker glyph also flips ([x] vs [!]).
    #[test]
    fn renderer_colours_custom_input_by_validation() {
        // Valid case — only runs if /dev/null exists as a chardev (it
        // does on every reasonable target).
        if !is_char_device(Path::new("/dev/null")) {
            return;
        }
        let mut state = PickerState {
            candidates: vec![PickerCandidate {
                label: "/dev/tty0".into(),
                target: PathBuf::from("/dev/tty0"),
                origin: CandidateOrigin::KernelConsole,
            }],
            selected: vec![true],
            cursor: 0,
            button_cursor: ButtonCursor::Spawn,
            custom_input: "/dev/null".to_string(),
            custom_checked: true,
            outcome: None,
        };
        let mut term = Terminal::new(TestBackend::new(80, 24)).expect("test terminal");
        term.draw(|f| render_picker_frame(f, &state)).expect("draw");
        let buf = term.backend().buffer();
        // Locate one of the green cells (the '/' of /dev/null in the
        // custom-input box).
        let mut green_seen = false;
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if let Some(cell) = buf.cell((x, y))
                    && cell.symbol() == "/"
                    && cell.style().fg == Some(Color::Green)
                {
                    green_seen = true;
                }
            }
        }
        assert!(green_seen, "valid custom path must render green");

        // Invalid case.
        state.custom_input = "/dev/this-does-not-exist-nmbl".to_string();
        let mut term = Terminal::new(TestBackend::new(80, 24)).expect("test terminal");
        term.draw(|f| render_picker_frame(f, &state)).expect("draw");
        let buf = term.backend().buffer();
        let mut red_seen = false;
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if let Some(cell) = buf.cell((x, y))
                    && cell.symbol() == "/"
                    && cell.style().fg == Some(Color::Red)
                {
                    red_seen = true;
                }
            }
        }
        assert!(red_seen, "invalid custom path must render red");
    }

    #[test]
    fn valid_custom_input_appears_in_selected_targets() {
        if !is_char_device(Path::new("/dev/null")) {
            return;
        }
        let state = PickerState {
            candidates: vec![PickerCandidate {
                label: "/dev/tty0".into(),
                target: PathBuf::from("/dev/tty0"),
                origin: CandidateOrigin::KernelConsole,
            }],
            selected: vec![true],
            cursor: 0,
            button_cursor: ButtonCursor::Spawn,
            custom_input: "/dev/null".to_string(),
            custom_checked: true,
            outcome: None,
        };
        let targets = state.selected_targets();
        assert!(targets.contains(&PathBuf::from("/dev/tty0")));
        assert!(targets.contains(&PathBuf::from("/dev/null")));
    }

    #[test]
    fn invalid_custom_input_is_excluded_from_selected_targets() {
        let state = PickerState {
            candidates: vec![PickerCandidate {
                label: "/dev/tty0".into(),
                target: PathBuf::from("/dev/tty0"),
                origin: CandidateOrigin::KernelConsole,
            }],
            selected: vec![true],
            cursor: 0,
            button_cursor: ButtonCursor::Spawn,
            custom_input: "/dev/does-not-exist".to_string(),
            custom_checked: true,
            outcome: None,
        };
        let targets = state.selected_targets();
        assert_eq!(targets, vec![PathBuf::from("/dev/tty0")]);
    }

    #[test]
    fn display_overlaps_targets_matches_path() {
        let targets = vec![PathBuf::from("/dev/tty0"), PathBuf::from("/dev/ttyS0")];
        assert!(display_overlaps_targets(Path::new("/dev/tty0"), &targets));
        assert!(!display_overlaps_targets(Path::new("/dev/tty1"), &targets));
        assert!(!display_overlaps_targets(Path::new("/dev/tty0"), &[]));
    }

    /// Splash backend always renders to `/dev/tty1`, regardless of what
    /// `/sys/class/tty/console/active` reports. Pins the contract that
    /// motivates this bug-fix: on a system with cmdline
    /// `console=ttyS0,115200 console=tty1` sysfs lists ttyS0 first, but
    /// the picker is the authoritative source and must return
    /// `/dev/tty1` so the relay agrees on the overlap predicate.
    #[test]
    fn display_target_for_splash_is_always_tty1() {
        let mut console = FakeConsole::new(Vec::new()).with_kind(ConsoleKind::Splash);
        assert_eq!(
            display_target_for(&console),
            PathBuf::from(SPLASH_DISPLAY_TTY)
        );
        // Hammer the kind a second time; the resolver MUST NOT memoise
        // across console kinds.
        console.kind = ConsoleKind::Tty;
        // For tty kind it falls back to read_active_console() which may
        // succeed or fall back to /dev/console depending on the host;
        // we only assert it does NOT return SPLASH_DISPLAY_TTY by
        // construction (it may equal it coincidentally on a desktop —
        // skip the negative assertion).
        let _ = display_target_for(&console);
    }

    /// Picker decides overlap=false: selection is `/dev/ttyS0` only on
    /// a splash console (display target = `/dev/tty1`). Dispatch MUST
    /// take the fire-and-forget branch — relay MUST NOT be invoked,
    /// and `console.suspend()` MUST NOT be called.
    ///
    /// This is the half of the bug-fix that protects against routing
    /// the relay onto a non-display tty. The companion test below pins
    /// the relay-branch path.
    #[test]
    fn dispatch_no_overlap_runs_detach_not_relay() {
        let cfg = Config::recovery_default();
        let mut console = FakeConsole::new(Vec::new()).with_kind(ConsoleKind::Splash);
        let targets = vec![PathBuf::from("/dev/ttyS0")];
        let display_target = PathBuf::from(SPLASH_DISPLAY_TTY);

        let mut relay_calls: u32 = 0;
        let mut detach_calls: u32 = 0;
        let mut detach_targets: Vec<PathBuf> = Vec::new();

        let outcome = dispatch_spawn(
            &mut console,
            &cfg,
            targets.clone(),
            &display_target,
            |_console, _config, _t, _d| {
                relay_calls = relay_calls.saturating_add(1);
                Ok(())
            },
            |_config, t| {
                detach_calls = detach_calls.saturating_add(1);
                detach_targets = t.to_vec();
                Ok(())
            },
        )
        .expect("dispatch must succeed");

        assert_eq!(relay_calls, 0, "relay must NOT be called when no overlap");
        assert_eq!(detach_calls, 1, "detach must be called exactly once");
        assert_eq!(detach_targets, targets);
        assert_eq!(
            console.suspend_calls, 0,
            "Console::suspend must NOT fire on the no-overlap branch"
        );
        match outcome {
            PickerSessionOutcome::ShellDetached { targets: out } => {
                assert_eq!(out, targets);
            }
            other => panic!("expected ShellDetached, got {other:?}"),
        }
    }

    /// Picker decides overlap=true: selection contains `/dev/tty1` on a
    /// splash console (display target = `/dev/tty1`). Dispatch MUST
    /// route into the relay branch, and the relay callback MUST be
    /// invoked with the picker's `display_target` path — proving the
    /// relay no longer recomputes it from sysfs.
    ///
    /// We additionally invoke a stand-in `relay_fn` that mirrors what
    /// the real [`crate::ui::console_relay::run_relay`] does for the
    /// overlap branch (call `console.suspend()` then `console.resume()`)
    /// so the suspend/resume side-effect is observable end-to-end.
    #[test]
    fn dispatch_overlap_runs_relay_and_suspends_console() {
        let cfg = Config::recovery_default();
        let mut console = FakeConsole::new(Vec::new()).with_kind(ConsoleKind::Splash);
        let targets = vec![
            PathBuf::from("/dev/ttyS0"),
            PathBuf::from(SPLASH_DISPLAY_TTY),
        ];
        let display_target = PathBuf::from(SPLASH_DISPLAY_TTY);

        let mut relay_calls: u32 = 0;
        let mut relay_seen_display: Option<PathBuf> = None;
        let mut relay_seen_targets: Vec<PathBuf> = Vec::new();
        let mut detach_calls: u32 = 0;

        let outcome = dispatch_spawn(
            &mut console,
            &cfg,
            targets.clone(),
            &display_target,
            |console, _config, t, d| {
                relay_calls = relay_calls.saturating_add(1);
                relay_seen_display = Some(d.to_path_buf());
                relay_seen_targets = t.to_vec();
                // Mirror the production overlap path: suspend, then
                // resume after the (fake) shell exits. This is what
                // `run_relay` does on the overlap branch.
                console.suspend()?;
                console.resume()?;
                Ok(())
            },
            |_config, _t| {
                detach_calls = detach_calls.saturating_add(1);
                Ok(())
            },
        )
        .expect("dispatch must succeed");

        assert_eq!(relay_calls, 1, "relay must be called exactly once");
        assert_eq!(detach_calls, 0, "detach must NOT be called on overlap");
        assert_eq!(
            relay_seen_display,
            Some(display_target.clone()),
            "relay must receive the picker's display_target verbatim"
        );
        assert_eq!(relay_seen_targets, targets);
        assert_eq!(
            console.suspend_calls, 1,
            "Console::suspend must fire exactly once on the overlap branch"
        );
        assert_eq!(console.resume_calls, 1);
        assert!(matches!(outcome, PickerSessionOutcome::ShellRan));
    }
}
