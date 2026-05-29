//! Keystroke translation and key-handling for the pretty-shell.

use std::os::fd::AsFd;

use alacritty_terminal::grid::Scroll;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::error::{NmblError, Result};

use super::state::{EscapeOutcome, PtyShellState};

/// Outcome of a single keystroke. The driver loop reads this to decide
/// whether to repaint, exit, or proceed silently.
pub(super) enum KeyOutcome {
    Quit,
    Redraw,
    Noop,
}

/// Translate one [`KeyEvent`] into either a state mutation (scroll
/// shortcut, quit) or a stream of bytes written to the master fd.
pub(super) fn handle_key(state: &mut PtyShellState, key: KeyEvent) -> Result<KeyOutcome> {
    use crossterm::event::KeyEventKind;
    if key.kind != KeyEventKind::Press {
        // crossterm reports key releases on some backends; ignore them.
        return Ok(KeyOutcome::Noop);
    }

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    // Ctrl+Shift+* — scroll bindings + emergency quit.
    if ctrl && shift {
        match key.code {
            KeyCode::Up => {
                state.term.grid_mut().scroll_display(Scroll::Delta(1));
                return Ok(KeyOutcome::Redraw);
            }
            KeyCode::Down => {
                state.term.grid_mut().scroll_display(Scroll::Delta(-1));
                return Ok(KeyOutcome::Redraw);
            }
            KeyCode::PageUp => {
                state.term.grid_mut().scroll_display(Scroll::PageUp);
                return Ok(KeyOutcome::Redraw);
            }
            KeyCode::PageDown => {
                state.term.grid_mut().scroll_display(Scroll::PageDown);
                return Ok(KeyOutcome::Redraw);
            }
            KeyCode::End => {
                state.term.grid_mut().scroll_display(Scroll::Bottom);
                return Ok(KeyOutcome::Redraw);
            }
            KeyCode::Home => {
                state.term.grid_mut().scroll_display(Scroll::Top);
                return Ok(KeyOutcome::Redraw);
            }
            _ => {}
        }
    }

    // Any non-scroll keystroke snaps the view to the bottom so the
    // operator sees their own input land in the live grid.
    if state.scroll_offset() != 0 {
        state.term.grid_mut().scroll_display(Scroll::Bottom);
    }

    let bytes = key_to_bytes(key);
    if bytes.is_empty() {
        return Ok(KeyOutcome::Noop);
    }
    // Run the keystroke's bytes through the SSH-style `<newline>~.`
    // quit recogniser before forwarding. A line-leading `~` is held
    // back until the next byte decides whether it begins the `~.` quit
    // command or is just a literal tilde.
    match state.process_escape(&bytes) {
        EscapeOutcome::Quit => return Ok(KeyOutcome::Quit),
        EscapeOutcome::Forward(forward) => {
            if !forward.is_empty() {
                write_to_pty(state, &forward)?;
            }
        }
    }
    // The terminal grid won't change until the shell echoes the byte
    // back; let the read pump trigger the next repaint.
    Ok(KeyOutcome::Noop)
}

/// Write `bytes` to the master fd, retrying on partial writes. EAGAIN
/// is treated as a hard error here because we just polled-then-wrote on
/// a fd that should always accept a keystroke's worth of data; if it
/// refuses, the shell is wedged and the operator wants to know.
pub(super) fn write_to_pty(state: &mut PtyShellState, bytes: &[u8]) -> Result<()> {
    let fd = state.child.master.as_fd();
    let mut written = 0usize;
    while written < bytes.len() {
        match rustix::io::write(fd, bytes.get(written..).unwrap_or(&[])) {
            Ok(0) => break,
            Ok(n) => written = written.saturating_add(n),
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => {
                return Err(NmblError::Tui {
                    source: std::io::Error::from(e),
                });
            }
        }
    }
    Ok(())
}

/// Convert one [`KeyEvent`] into the byte sequence a typical terminal
/// emulator would send to the slave. The mapping intentionally targets
/// busybox / xterm conventions and ignores OS-specific keymap features
/// (Meta-as-Esc, application-mode arrows). Programs that need
/// application mode work by emitting their own escapes via DECSET.
pub fn key_to_bytes(key: KeyEvent) -> Vec<u8> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                // Map Ctrl+letter to control bytes (^A=0x01, …). Pass
                // through punctuation unchanged so e.g. Ctrl+] still
                // does the right thing for shells that bind it.
                let upper = c.to_ascii_uppercase();
                if upper.is_ascii_alphabetic() {
                    return vec![(upper as u8) & 0x1F];
                }
                match upper {
                    '@' => return vec![0x00],
                    '[' => return vec![0x1B],
                    '\\' => return vec![0x1C],
                    ']' => return vec![0x1D],
                    '^' => return vec![0x1E],
                    '_' => return vec![0x1F],
                    _ => {}
                }
            }
            let mut buf = [0u8; 4];
            c.encode_utf8(&mut buf).as_bytes().to_vec()
        }
        KeyCode::Enter => b"\r".to_vec(),
        KeyCode::Backspace => b"\x7f".to_vec(),
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Esc => b"\x1b".to_vec(),
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        // Function keys — VT100 / xterm sequences. Rarely used in a
        // recovery shell but cheap to include.
        KeyCode::F(1) => b"\x1bOP".to_vec(),
        KeyCode::F(2) => b"\x1bOQ".to_vec(),
        KeyCode::F(3) => b"\x1bOR".to_vec(),
        KeyCode::F(4) => b"\x1bOS".to_vec(),
        KeyCode::F(n @ 5..=12) => {
            // CSI sequences for F5-F12.
            let code = match n {
                5 => 15,
                6 => 17,
                7 => 18,
                8 => 19,
                9 => 20,
                10 => 21,
                11 => 23,
                12 => 24,
                _ => return Vec::new(),
            };
            format!("\x1b[{code}~").into_bytes()
        }
        _ => Vec::new(),
    }
}
