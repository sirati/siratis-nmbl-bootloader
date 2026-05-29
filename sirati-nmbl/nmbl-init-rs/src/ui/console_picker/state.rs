use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};

use crate::config::Config;
use crate::error::Result;
use crate::nmbl_warn;
use crate::sys::tty::read_active_console;
use crate::ui::tty_enum::{EnumeratedTty, TtyKind, enumerate_ttys, is_char_device};

use super::types::{
    ButtonCursor, CandidateOrigin, CustomValidation, FocusZone, PickerCandidate, PickerOutcome,
    PickerState,
};

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
            custom_cursor: 0,
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

        // Custom-input field captures editing keystrokes when focused so
        // the operator can type AND edit a path with a real cursor
        // (Left/Right/Home/End, Backspace/Delete, word motion). A few
        // keys still escape to move focus / commit: Up/Down navigate,
        // Esc/Enter fall through to the shared handler, and Tab toggles
        // the custom checkbox.
        if self.focus() == FocusZone::CustomInput {
            match key.code {
                KeyCode::Up | KeyCode::Down | KeyCode::Esc | KeyCode::Enter => {
                    // fall through to the shared navigation handler below
                }
                KeyCode::Tab => {
                    // Tab on the custom field toggles its "checked"
                    // flag (only meaningful when validation is Valid).
                    self.custom_checked = !self.custom_checked;
                    return false;
                }
                _ => {
                    // Everything else (printable chars incl. Space,
                    // Left/Right/Home/End, Backspace/Delete, word
                    // motion) edits the buffer through the shared
                    // line-editing helper. Space here is a literal
                    // space, NOT a checkbox toggle. A path is not a
                    // secret, so word motion is allowed.
                    let (new_cursor, _handled) = crate::ui::editline::handle_key_on(
                        &mut self.custom_input,
                        self.custom_cursor,
                        key,
                        true,
                    );
                    self.custom_cursor = new_cursor;
                    if self.custom_input.is_empty() {
                        self.custom_cursor = 0;
                    }
                    return false;
                }
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
