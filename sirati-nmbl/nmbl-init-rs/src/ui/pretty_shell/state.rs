//! Core state types and SSH-style escape recogniser for the pretty-shell.

use alacritty_terminal::Term;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::Config as TermConfig;
use alacritty_terminal::vte::ansi::Processor;

use crate::sys::pty::PtyChild;

/// Single read budget per loop iteration. Bounded so the driver loop
/// always falls back to render-and-poll-key on time. `alacritty`'s VTE
/// parser handles partial sequences across feeds.
pub(super) const PTY_READ_CHUNK: usize = 4096;

/// `Dimensions` impl for the pretty-shell grid. Local copy of the same
/// trait impl used by `splash::terminal::SplashTerminal` — the upstream
/// crate's `TermSize` is `cfg(test)`-gated and not reusable.
pub(super) struct GridSize {
    pub columns: usize,
    pub screen_lines: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }
    fn screen_lines(&self) -> usize {
        self.screen_lines
    }
    fn columns(&self) -> usize {
        self.columns
    }
}

/// State of the SSH-style `~.` escape recogniser. The escape char is
/// only honoured immediately after a line break (or at session start),
/// mirroring OpenSSH client behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EscapeState {
    /// At the start of a line — a `~` here arms the escape.
    LineStart,
    /// Mid-line — `~` is an ordinary character.
    MidLine,
    /// A line-leading `~` was seen; the next byte selects the command
    /// (`.` quits, anything else is passed through).
    Armed,
}

/// What the escape recogniser decided for a chunk of outgoing bytes.
pub(super) enum EscapeOutcome {
    /// Forward these bytes to the child (may differ from the input when
    /// an escape was partially consumed), then continue.
    Forward(Vec<u8>),
    /// `~.` was completed — quit the pretty shell.
    Quit,
}

/// Compute the escape line-state implied by having just sent byte `b`:
/// a carriage return or newline puts us at the start of a fresh line
/// (where a `~` arms the escape); any other byte is mid-line.
pub(super) fn next_line_state(b: u8) -> EscapeState {
    if b == b'\r' || b == b'\n' {
        EscapeState::LineStart
    } else {
        EscapeState::MidLine
    }
}

/// Pure SSH-style `<newline>~.` recogniser over a byte chunk. Mutates
/// `escape` in place and returns the bytes to forward (or `Quit`). Split
/// out as a free function so it can be unit-tested without a live PTY.
pub(super) fn run_escape(escape: &mut EscapeState, bytes: &[u8]) -> EscapeOutcome {
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len().saturating_add(1));
    for &b in bytes {
        match *escape {
            EscapeState::Armed => {
                if b == b'.' {
                    return EscapeOutcome::Quit;
                }
                if b == b'~' {
                    // `~~` is the escape for a single literal tilde
                    // (OpenSSH convention): emit one `~` and stay
                    // mid-line so a following `.` is not a quit.
                    out.push(b'~');
                    *escape = EscapeState::MidLine;
                } else {
                    // Not an escape command: emit the deferred `~` then
                    // the current byte, recomputing line state.
                    out.push(b'~');
                    out.push(b);
                    *escape = next_line_state(b);
                }
            }
            EscapeState::LineStart if b == b'~' => {
                // Defer the `~`: don't forward it yet — it may be the
                // start of an escape.
                *escape = EscapeState::Armed;
            }
            _ => {
                out.push(b);
                *escape = next_line_state(b);
            }
        }
    }
    EscapeOutcome::Forward(out)
}

/// All mutable state for the pretty-shell screen. The driver loop
/// constructs this once, drives it to exit, and drops it (which kills
/// the child via [`PtyChild::terminate`] on the cleanup path).
pub struct PtyShellState {
    pub term: Term<VoidListener>,
    pub parser: Processor,
    pub child: PtyChild,
    /// True once the master fd reads return EOF or the child has been
    /// reaped via `try_wait`.
    pub child_exited: bool,
    pub cols: u16,
    pub rows: u16,
    /// SSH-style `<newline>~.` quit-escape recogniser. Tracks where in
    /// the input stream we are so a bare `~` only triggers when typed at
    /// the start of a line, exactly like OpenSSH's `~.` escape.
    pub(super) escape: EscapeState,
}

impl PtyShellState {
    pub(super) fn new(child: PtyChild, cols: u16, rows: u16) -> Self {
        let size = GridSize {
            columns: cols as usize,
            screen_lines: rows as usize,
        };
        let term = Term::new(TermConfig::default(), &size, VoidListener);
        Self {
            term,
            parser: Processor::new(),
            child,
            child_exited: false,
            cols,
            rows,
            // The shell prompt starts a fresh line, so a `~` typed as
            // the very first keystroke arms the escape.
            escape: EscapeState::LineStart,
        }
    }

    /// Run a chunk of outgoing bytes (the encoding of one keystroke)
    /// through the SSH-style escape recogniser, updating the line-start
    /// tracking and detecting the `~.` quit sequence.
    pub(super) fn process_escape(&mut self, bytes: &[u8]) -> EscapeOutcome {
        run_escape(&mut self.escape, bytes)
    }

    /// Current scrollback offset (rows above the live tail). Zero means
    /// the live grid is visible.
    pub fn scroll_offset(&self) -> usize {
        self.term.grid().display_offset()
    }
}
