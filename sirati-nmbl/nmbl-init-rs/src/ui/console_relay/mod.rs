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

mod loop_impl;
mod modal;

use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use rustix::fs::{Mode, OFlags, fcntl_setfl, open};

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::nmbl_warn;
use crate::sys::pty::spawn_shell;
use crate::ui::console::Console;
use crate::ui::console_picker::display_overlaps_targets;

use loop_impl::{run_loop, run_loop_with_modal};

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
