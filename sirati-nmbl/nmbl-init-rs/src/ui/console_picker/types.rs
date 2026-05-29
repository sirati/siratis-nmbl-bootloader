use std::path::PathBuf;

use crate::ui::tty_enum::TtyKind;

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
    pub(super) fn label_suffix(&self) -> &'static str {
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
    /// Byte-index cursor into `custom_input` for line editing. Always on
    /// a char boundary in `0..=custom_input.len()`. A path is not secret,
    /// so word motion is permitted here.
    pub custom_cursor: usize,
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
///
/// [`run_picker_session`]: super::run_picker_session
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
