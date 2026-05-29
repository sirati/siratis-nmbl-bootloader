//! Multi-target multiplex relay for the in-process emergency shell.
//!
//! Architecture (see also `crate::ui::console_picker`):
//!
//! - One forked shell on a PTY pair (via [`crate::sys::pty::spawn_shell`]).
//! - N selected `/dev/<tty>` target fds opened RW.
//! - A `poll(2)`-driven event loop that fans bytes:
//!   - PTY master read → write to every target.
//!   - target read → write to PTY master (shell input).
//! - When the shell exits we stop the loop, drop fds, and return.
//!
//! ## Display-overlap toggle
//!
//! Two regimes, both decided up front by checking whether the live
//! [`Console`]'s display target appears in the selected target set
//! (see [`crate::ui::console_picker::display_overlaps_targets`]):
//!
//! 1. **Overlap**: NMBL calls [`Console::suspend`] so the kernel can
//!    paint the framebuffer / VT directly. The relay loop owns the
//!    full input pipeline until the shell exits, at which point
//!    [`Console::resume`] re-paints the TUI.
//! 2. **No overlap**: NMBL keeps the TUI alive, runs the relay loop
//!    in PID 1, and shows a "Shell running on /dev/X" modal so the
//!    operator sees that something is happening.
//!
//! In both regimes the relay loop runs in the SAME thread as the UI;
//! we don't spin up workers. `poll(2)` with a short timeout interleaves
//! the byte-pump with any modal repaint or keypress polling.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use rustix::event::{PollFd, PollFlags, poll};
use rustix::fs::{Mode, OFlags, fcntl_setfl, open};

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::nmbl_warn;
use crate::sys::pty::{PtyChild, spawn_shell};
use crate::ui::POLL_SLICE;
use crate::ui::console::Console;
use crate::ui::console_picker::display_overlaps_targets;

/// Conventional row/col for the multiplexed shell. The kernel's
/// `ws_col`/`ws_row` are advisory for non-VT consoles; we pick the
/// same 80x24 the rest of NMBL uses so curses-y rescue tools render
/// sensibly. Operators on wide serial lines can resize via `stty cols`
/// once the shell is up.
const SHELL_COLS: u16 = 80;
const SHELL_ROWS: u16 = 24;

/// poll(2) timeout while the relay runs. Short enough to keep the
/// "Shell running on X" modal responsive (the loop also polls the
/// console for cancel keystrokes); long enough that idle bytes don't
/// burn CPU.
const RELAY_POLL_MS: i32 = 100;

/// Per-target I/O buffer. Sized to match the PTY chunk size used by
/// `pretty_shell.rs` so a bursty `cat /var/log/messages` doesn't get
/// chopped into a hundred tiny writes.
const RELAY_BUF: usize = 4096;

/// Open one `/dev/<tty>` target read/write and put it in non-blocking
/// mode. The fd is `O_NOCTTY` so we don't accidentally take ownership
/// of the line as our controlling terminal — PID 1 already has one
/// (or none) and we want neither change.
///
/// Returns `None` (with a warning logged) on failure so a single bad
/// target doesn't abort the whole relay. The remaining selected
/// targets continue to work.
fn open_target_nonblocking(path: &Path) -> Option<OwnedFd> {
    let fd = match open(
        path,
        OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(f) => f,
        Err(e) => {
            nmbl_warn!(
                "console_relay: could not open target {}: {e}",
                path.display()
            );
            return None;
        }
    };
    if let Err(e) = fcntl_setfl(&fd, OFlags::NONBLOCK) {
        nmbl_warn!(
            "console_relay: F_SETFL O_NONBLOCK on {} failed: {e}",
            path.display()
        );
        // Even without non-blocking the relay can usually still cope
        // because we poll before reading — drop down to blocking mode
        // rather than killing the target outright.
    }
    Some(fd)
}

/// Spawn the shell + open every selected target, then drive the
/// multiplex loop until the shell exits.
///
/// `targets` MUST be non-empty (the picker enforces this).
///
/// `display_target` is the precomputed device path the live
/// [`Console`] is currently rendering to, supplied by the picker via
/// [`crate::ui::console_picker::display_target_for`]. It is the
/// authoritative source of truth for the overlap decision — the relay
/// MUST NOT re-derive it from `/sys/class/tty/console/active`, because
/// sysfs lists kernel-cmdline consoles in declaration order and the
/// picker's splash backend always renders to `/dev/tty1` regardless of
/// the cmdline ordering. Re-reading sysfs here used to flip the overlap
/// verdict and leave the operator staring at a frozen "Shell running"
/// modal with the shell painting invisibly behind it.
pub async fn run_relay(
    console: &mut dyn Console,
    config: &Config,
    targets: &[PathBuf],
    display_target: &Path,
) -> Result<()> {
    if targets.is_empty() {
        // Defence in depth: picker guarantees non-empty, but explicit
        // refusal here keeps the function total.
        return Ok(());
    }

    let overlap = display_overlaps_targets(display_target, targets);

    // Open every target. Bad targets are skipped (open_target_nonblocking
    // returns None + logs); the surviving set drives the relay.
    let target_fds: Vec<(PathBuf, OwnedFd)> = targets
        .iter()
        .filter_map(|p| open_target_nonblocking(p).map(|fd| (p.clone(), fd)))
        .collect();
    if target_fds.is_empty() {
        // Every selected tty failed to open (permissions, missing
        // device node, …). This used to return Ok(()) and silently
        // drop back to the menu — the operator saw NOTHING and had no
        // idea the shell never spawned. Surface it as a real error so
        // the emergency loop shows a modal naming the unopenable
        // targets instead of leaving the menu unchanged.
        let names = targets
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(NmblError::Tui {
            source: std::io::Error::other(format!(
                "could not open any selected shell target ({names}); \
                 check the device node exists and is writable"
            )),
        });
    }

    // Now the shell. spawn_shell forks, sets up controlling terminal,
    // execve's busybox; on return we own the master fd.
    let child = spawn_shell(&config.paths.shell, SHELL_COLS, SHELL_ROWS)?;

    if overlap {
        // Hand the framebuffer / kernel-VT back to the kernel for the
        // shell's lifetime. If suspend() fails we still proceed —
        // the shell is already forked, refusing to enter the relay
        // loop here would orphan it on PID 1.
        if let Err(e) = console.suspend() {
            nmbl_warn!(
                "console_relay: Console::suspend failed: {e}; \
                 the shell may render on top of stale TUI chrome"
            );
        }
        let outcome = run_loop(child, &target_fds, None).await;
        // Re-acquire. resume() failures aren't fatal — the operator
        // can press a key to force a redraw on the next render cycle.
        if let Err(e) = console.resume() {
            nmbl_warn!("console_relay: Console::resume failed: {e}");
        }
        outcome
    } else {
        // No overlap: keep the TUI live, show a modal, and pump the
        // relay loop on every render slice.
        run_loop_with_modal(child, &target_fds, console).await
    }
}

/// Modal-overlay relay loop. The TUI keeps the console; we paint a
/// "Shell running on /dev/X" banner once per render slice and call
/// [`run_loop_slice`] in between to keep bytes flowing.
async fn run_loop_with_modal(
    child: PtyChild,
    targets: &[(PathBuf, OwnedFd)],
    console: &mut dyn Console,
) -> Result<()> {
    let modal_targets: Vec<String> = targets
        .iter()
        .map(|(p, _)| p.display().to_string())
        .collect();
    let banner = format!(
        "Shell running on:\n  {}\nType into those consoles. Press Esc to stop the shell.",
        modal_targets.join("\n  ")
    );

    let mut child_exited = false;
    loop {
        // Repaint the modal each iteration. The text is small and
        // ratatui's double-buffer suppresses redundant draws at the
        // terminal level, so the cost is negligible compared to the
        // 100ms poll slice it sits between.
        let text = banner.clone();
        console.draw_with(&mut |frame| render_running_modal(frame, &text))?;

        // 1. Pump the relay for ~100ms.
        let _ = run_loop_slice(&child, targets);

        // 2. Poll the live console for an operator-side abort via the
        //    async `poll_event`; only a key matters, a resize is
        //    ignored (the modal repaints next iteration anyway). This
        //    matches the prior `poll_key` Esc-abort semantics.
        if let Some(crate::ui::console::ConsoleEvent::Key(key)) =
            console.poll_event(POLL_SLICE).await?
            && matches!(key.code, crossterm::event::KeyCode::Esc)
        {
            child.terminate();
            child_exited = true;
        }

        // 3. Has the shell exited?
        if !child_exited && let Ok(Some(_)) = child.try_wait() {
            child_exited = true;
        }
        if child_exited {
            // One last drain so the operator sees the shell's farewell
            // output land on the targets.
            let _ = run_loop_slice(&child, targets);
            // Best-effort terminate covers the "still running" cases.
            child.terminate();
            return Ok(());
        }
    }
}

/// Suspended-console relay loop. The display is on the same tty as
/// at least one target; the kernel paints the shell directly. The
/// loop just pumps bytes and waits for the shell to exit.
///
/// The optional second argument is reserved for future cancellation
/// channels (e.g. an operator-side abort token). Today the loop only
/// ends on shell exit.
async fn run_loop(
    child: PtyChild,
    targets: &[(PathBuf, OwnedFd)],
    _abort: Option<()>,
) -> Result<()> {
    loop {
        let _ = run_loop_slice(&child, targets);
        // Yield to the executor between byte-pump slices so the poller
        // driver and any future spawn_local task get a turn. The slice
        // itself already paces via its internal 100ms poll(2) timeout.
        // (Phase: a later phase may replace the slice's rustix poll with
        // a tokio::select! over AsyncFds; the byte-fan behaviour is kept
        // identical here since no concurrent consumer exists yet.)
        tokio::task::yield_now().await;
        if let Ok(Some(_)) = child.try_wait() {
            // Drain remaining bytes; the shell's exit message should
            // land on every target.
            let _ = run_loop_slice(&child, targets);
            child.terminate();
            return Ok(());
        }
    }
}

/// One iteration of the multiplex loop: poll, fan-out, fan-in. Returns
/// once `poll(2)` wakes (timeout or fd readability). All I/O errors
/// are logged and the loop continues; the only way out is the parent
/// observing `waitpid` success on the shell.
fn run_loop_slice(child: &PtyChild, targets: &[(PathBuf, OwnedFd)]) -> Result<()> {
    // PollFd::new is `PollFd::new<Fd: AsFd>(&Fd, PollFlags)`; we need
    // to keep the source references alive for the lifetime of the
    // PollFd slice, so we materialise both the master BorrowedFd and
    // each target's BorrowedFd into a Vec up front. The Vec then
    // outlives the pfd Vec.
    let master_borrowed: BorrowedFd<'_> = child.master_fd();
    let target_borrows: Vec<BorrowedFd<'_>> = targets.iter().map(|(_, fd)| fd.as_fd()).collect();

    // Build PollFd entries: index 0 is the master, 1..N are the targets.
    let mut pfds: Vec<PollFd<'_>> = Vec::with_capacity(target_borrows.len().saturating_add(1));
    pfds.push(PollFd::new(&master_borrowed, PollFlags::IN));
    for bfd in &target_borrows {
        pfds.push(PollFd::new(bfd, PollFlags::IN));
    }

    let _ = match poll(&mut pfds, RELAY_POLL_MS) {
        Ok(n) => n,
        Err(rustix::io::Errno::INTR) => return Ok(()),
        Err(e) => {
            nmbl_warn!("console_relay: poll(2) failed: {e}; sleeping one slice");
            return Ok(());
        }
    };

    // Pump master → targets first so output the shell just produced
    // reaches the operator before we feed their (possibly slow)
    // typed input back into the shell. This matches what a serial
    // console driver does naturally.
    let master_revents = pfds
        .first()
        .map(PollFd::revents)
        .unwrap_or_else(PollFlags::empty);
    if master_revents.intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR) {
        fan_out_master_to_targets(child, targets);
    }

    // Targets → master.
    for (idx, (path, fd)) in targets.iter().enumerate() {
        let entry_idx = idx.saturating_add(1);
        let revents = pfds
            .get(entry_idx)
            .map(PollFd::revents)
            .unwrap_or_else(PollFlags::empty);
        if revents.intersects(PollFlags::IN) {
            fan_in_target_to_master(child, path, fd.as_fd());
        }
    }
    Ok(())
}

/// Drain the PTY master (everything readable in one sweep, bounded so
/// a runaway producer doesn't starve the input direction) and write
/// the bytes to every target. Per-target write errors are logged and
/// the target is left in the set — a write that fails this iteration
/// may succeed next iteration (e.g. EAGAIN cleared).
fn fan_out_master_to_targets(child: &PtyChild, targets: &[(PathBuf, OwnedFd)]) {
    let master = child.master_fd();
    let mut buf = [0u8; RELAY_BUF];
    // Bound the drain so a misbehaving program (`yes` etc.) doesn't
    // starve fan-in. Eight reads × 4 KiB = 32 KiB / slice / target,
    // plenty for an interactive shell.
    for _ in 0..8 {
        match rustix::io::read(master, &mut buf) {
            Ok(0) => return,
            Ok(n) => {
                let bytes = buf.get(..n).unwrap_or(&[]);
                for (path, fd) in targets {
                    write_all_best_effort(fd.as_fd(), bytes, "master->target", path);
                }
            }
            Err(rustix::io::Errno::AGAIN) => return,
            Err(rustix::io::Errno::IO) => {
                // EIO on a PTY master usually means the slave hung up;
                // try_wait in the caller will reap the child shortly.
                return;
            }
            Err(e) => {
                nmbl_warn!("console_relay: master read failed: {e}");
                return;
            }
        }
    }
}

/// Drain one target fd and feed the bytes to the PTY master. Per-call
/// the loop reads at most eight times (same bound as the fan-out) so
/// no one target can dominate the slice.
fn fan_in_target_to_master(child: &PtyChild, path: &Path, fd: BorrowedFd<'_>) {
    let master = child.master_fd();
    let mut buf = [0u8; RELAY_BUF];
    for _ in 0..8 {
        match rustix::io::read(fd, &mut buf) {
            Ok(0) => return,
            Ok(n) => {
                let bytes = buf.get(..n).unwrap_or(&[]);
                write_all_best_effort(master, bytes, "target->master", path);
            }
            Err(rustix::io::Errno::AGAIN) => return,
            Err(e) => {
                nmbl_warn!("console_relay: target {} read failed: {e}", path.display());
                return;
            }
        }
    }
}

/// Push `bytes` to `fd`, retrying past EINTR and short writes. Logs
/// (but does not propagate) any other error so the loop keeps moving.
fn write_all_best_effort(fd: BorrowedFd<'_>, bytes: &[u8], direction: &str, path: &Path) {
    let mut written = 0usize;
    while written < bytes.len() {
        let slice = bytes.get(written..).unwrap_or(&[]);
        match rustix::io::write(fd, slice) {
            Ok(0) => return,
            Ok(n) => written = written.saturating_add(n),
            Err(rustix::io::Errno::INTR) => continue,
            Err(rustix::io::Errno::AGAIN) => {
                // The destination is a tty that filled its write buffer.
                // Drop the rest of this batch; we'll catch up on the
                // next slice. Surfacing this as a warning every time
                // would spam the log on a slow line, so we just bail.
                return;
            }
            Err(e) => {
                nmbl_warn!(
                    "console_relay: {direction} write to {} failed: {e}",
                    path.display()
                );
                return;
            }
        }
    }
}

/// Render a "shell is running" modal over the live console. Mirrors
/// `view::render_modal_error` shape but emphasises that the shell is
/// alive, not erroring.
fn render_running_modal(frame: &mut Frame<'_>, message: &str) {
    let area = frame.area();
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas::<3>(area);

    let title = Paragraph::new(Line::from(vec![Span::styled(
        "Emergency shell",
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )]))
    .alignment(Alignment::Left);
    frame.render_widget(title, header);

    let modal = centered_rect(body, 70, body.height.saturating_div(2).max(8));
    frame.render_widget(Clear, modal);

    let block = Block::bordered().title(Span::styled(
        "shell running",
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    ));
    let para = Paragraph::new(message.to_owned())
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(para, modal);

    frame.render_widget(
        Paragraph::new("Esc: terminate shell").alignment(Alignment::Left),
        footer,
    );
}

fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    let x = area.x.saturating_add(area.width.saturating_sub(w) / 2);
    let y = area.y.saturating_add(area.height.saturating_sub(h) / 2);
    Rect::new(x, y, w, h)
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

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn render_running_modal_includes_targets_and_hint() {
        let mut term = Terminal::new(TestBackend::new(80, 16)).expect("test terminal");
        let msg = "Shell running on:\n  /dev/tty0\nType into those consoles.";
        term.draw(|f| render_running_modal(f, msg)).expect("draw");
        let buf = term.backend().buffer();
        let dump: String = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol().to_owned()))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(dump.contains("Emergency shell"), "title missing: \n{dump}");
        assert!(dump.contains("/dev/tty0"), "target missing: \n{dump}");
        assert!(dump.contains("Esc"), "hint missing: \n{dump}");
    }
}
