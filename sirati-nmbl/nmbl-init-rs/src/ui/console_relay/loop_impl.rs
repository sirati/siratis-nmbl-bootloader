//! Poll-driven multiplex loops for the console relay.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};

use rustix::event::{PollFd, PollFlags, poll};

use crate::error::Result;
use crate::nmbl_warn;
use crate::sys::pty::PtyChild;
use crate::ui::POLL_SLICE;
use crate::ui::console::Console;

use super::modal::render_running_modal;
use super::{RELAY_BUF, RELAY_POLL_MS};

/// Modal-overlay relay loop. The TUI keeps the console; we paint a
/// "Shell running on /dev/X" banner once per render slice and call
/// [`run_loop_slice`] in between to keep bytes flowing.
pub(super) async fn run_loop_with_modal(
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
pub(super) async fn run_loop(
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
pub(super) fn run_loop_slice(child: &PtyChild, targets: &[(PathBuf, OwnedFd)]) -> Result<()> {
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
pub(super) fn write_all_best_effort(
    fd: BorrowedFd<'_>,
    bytes: &[u8],
    direction: &str,
    path: &Path,
) {
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
