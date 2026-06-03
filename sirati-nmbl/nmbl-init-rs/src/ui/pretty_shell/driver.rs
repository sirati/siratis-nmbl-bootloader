//! Driver loop: entry point and main render-poll-pump cycle.

use crate::config::Config;
use crate::error::Result;
use crate::nmbl_warn;
use crate::sys::ops::ExecOps;
use crate::ui::POLL_SLICE;
use crate::ui::console::{Console, ConsoleEvent};

use super::keys::{KeyOutcome, handle_key, handle_scroll};
use super::pump::{PumpError, pump_pty};
use super::render::{apply_resize, render};
use super::state::PtyShellState;
use super::{CHROME_COLS, CHROME_ROWS, PRETTY_SHELL_MIN_COLS, PRETTY_SHELL_MIN_ROWS};

/// Open a pretty-shell session on the supplied console. Forks
/// `config.paths.shell` onto a fresh PTY, then drives the render-poll-
/// pump loop until the child exits or NMBL detects an I/O failure.
///
/// Returns `Ok(())` on a clean exit (the shell ran to completion) and
/// `Err` only when the supporting plumbing fails (fork, openpty,
/// terminal backend write). The caller in `src/shell.rs` treats both
/// outcomes the same way: re-display the emergency menu.
pub async fn run_pretty_shell<E: ExecOps>(
    sealed: crate::policy::Sealed,
    ops: &mut E,
    console: &mut dyn Console,
    config: &Config,
) -> Result<()> {
    // `sealed` proves `policy::seal_secrets` ran before this PTY-shell
    // fork (G3): the lock PCR is capped and every TPM-unsealed mapper is
    // closed. The witness is required by type so a pretty shell cannot
    // start without a seal (re-audit C-1); we thread it through the
    // `ExecOps::spawn_shell` seam down into the real `spawn_shell`
    // fork/execve waist below.
    // Derive the PTY grid size from the live console dimensions so the
    // alacritty terminal fills the bordered block. The renderer paints
    // a 3-row header + 1-row footer + bordered block (2 rows of border
    // + 2 cols of border); see [`CHROME_ROWS`] / [`CHROME_COLS`].
    let (frame_cols, frame_rows) = console.size();
    let cols = frame_cols
        .saturating_sub(CHROME_COLS)
        .max(PRETTY_SHELL_MIN_COLS);
    let rows = frame_rows
        .saturating_sub(CHROME_ROWS)
        .max(PRETTY_SHELL_MIN_ROWS);

    let child = ops.spawn_shell(sealed, &config.paths.shell, cols, rows)?;
    let mut state = PtyShellState::new(child, cols, rows);

    let outcome = drive(&mut state, console).await;

    // Best-effort kill + reap; safe on a child that has already exited.
    state.child.terminate();

    outcome
}

/// Main loop. Render-then-poll-then-pump. Exits when the child is
/// reaped and the master fd has been drained, or when the operator
/// types the SSH-style `<newline>~.` quit escape.
async fn drive(state: &mut PtyShellState, console: &mut dyn Console) -> Result<()> {
    let mut dirty = true;
    loop {
        if dirty {
            render(state, console)?;
            dirty = false;
        }

        // 1. Poll for one input event with a short timeout so we get
        //    back to pumping the PTY promptly. We use `poll_event` (not
        //    `poll_key`) so host-terminal resizes reach us: the default
        //    `poll_key` adapter silently drops `ConsoleEvent::Resize`,
        //    which would leave the shell box stuck at its old geometry.
        match console.poll_event(POLL_SLICE).await? {
            Some(ConsoleEvent::Key(k)) => match handle_key(state, k)? {
                KeyOutcome::Quit => return Ok(()),
                KeyOutcome::Redraw => dirty = true,
                KeyOutcome::Noop => {}
            },
            // The backend has already cached the new size; re-derive the
            // grid geometry and push it to the emulator + child. The guard
            // applies the resize and only marks dirty when geometry changed.
            Some(ConsoleEvent::Resize { .. }) if apply_resize(state, console) => {
                dirty = true;
            }
            // Mouse wheel drives NMBL's scrollback exactly like
            // Ctrl+Shift+Up/Down — a few rows per notch. A wheel notch is
            // a scroll, not a keystroke, so it must NOT snap the view to
            // the bottom and is never forwarded to the child PTY.
            Some(ConsoleEvent::Scroll { up }) => {
                handle_scroll(state, up);
                dirty = true;
            }
            _ => {}
        }

        // 2. Drain whatever the child has produced this slice. Multiple
        //    small reads keep memory bounded and let the parser see the
        //    full incremental state.
        match pump_pty(state) {
            Ok(read_any) => {
                if read_any {
                    dirty = true;
                }
            }
            Err(PumpError::Eof) => {
                state.child_exited = true;
            }
            Err(PumpError::Io(e)) => {
                nmbl_warn!("pretty-shell PTY read failed: {e}");
                return Ok(());
            }
        }

        // 3. Reap zombies opportunistically. Don't bail on the FIRST
        //    sight of an exit — the master fd may still hold the
        //    shell's farewell output. Wait for both events.
        if !state.child_exited
            && let Ok(Some(_)) = state.child.try_wait()
        {
            state.child_exited = true;
            dirty = true;
        }

        if state.child_exited {
            // Drain remaining output one last time before bailing.
            let _ = pump_pty(state);
            // One final repaint so the operator sees the shell's last
            // line (typically "exit").
            render(state, console)?;
            return Ok(());
        }
    }
}
