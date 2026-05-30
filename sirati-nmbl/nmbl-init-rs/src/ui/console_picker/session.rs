use std::path::{Path, PathBuf};

use ratatui::Frame;

use crate::config::Config;
use crate::error::Result;
use crate::nmbl_warn;
use crate::sys::ops::ExecOps;
use crate::sys::tty::read_active_console;
use crate::ui::POLL_SLICE;
use crate::ui::console::{Console, ConsoleKind};

use super::render::render_picker_frame;
use super::types::{PickerOutcome, PickerState};

/// Tty path the splash backend renders to. Mirrors the
/// `INPUT_TTY_PATH` constant inside `console::splash` so the overlap
/// decision agrees with where the kernel actually paints the
/// framebuffer.
pub(super) const SPLASH_DISPLAY_TTY: &str = "/dev/tty1";

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
pub async fn run_picker_session<E: ExecOps>(
    ops: &mut E,
    console: &mut dyn Console,
    config: &Config,
) -> Result<PickerSessionOutcome> {
    let mut state = PickerState::build(config)?;
    if state.candidates.is_empty() {
        return Ok(PickerSessionOutcome::Cancelled);
    }
    drive_picker_loop(&mut state, console).await?;

    let targets = match state.outcome {
        Some(PickerOutcome::Spawn { targets }) => targets,
        Some(PickerOutcome::Cancel) | None => return Ok(PickerSessionOutcome::Cancelled),
    };
    let display_target = display_target_for(console);
    // The relay (overlap) branch routes the shell spawn through
    // `ops.spawn_shell`; the fire-and-forget (no-overlap) branch uses
    // `spawn_shell_on_tty`, which is not part of `ExecOps` and builds its
    // own sync-only `FsOps` for the pre-fork presence check. The decision
    // is the pure [`display_overlaps_targets`] predicate (directly
    // unit-tested); the two arms below are trivial wrappers around the
    // already-tested relay / fire-and-forget helpers.
    if display_overlaps_targets(&display_target, &targets) {
        crate::ui::console_relay::run_relay(ops, console, config, &targets, &display_target)
            .await?;
        Ok(PickerSessionOutcome::ShellRan)
    } else {
        fire_and_forget_spawn(config, &targets)?;
        Ok(PickerSessionOutcome::ShellDetached { targets })
    }
}

/// Resolve the device path the live console is currently rendering to.
/// For the splash backend the operator sees `/dev/tty1` (framebuffer
/// VT); for the tty backend it is whatever the kernel-elected console
/// resolves to. Failure of the kernel-console resolver falls back to
/// `/dev/console` — the same fallback the picker uses for its first
/// candidate so the overlap decision stays self-consistent.
pub(super) fn display_target_for(console: &dyn Console) -> PathBuf {
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
    // `spawn_shell_on_tty` is fire-and-forget and not part of `ExecOps`;
    // its only pre-fork fs work is the shell-presence preflight, which a
    // sender-less `RealSys` satisfies (the preflight is sync and never
    // touches the poller). The fork/exec stays a genuine syscall.
    let fs = crate::sys::ops::RealSys::sync_only();
    for t in targets {
        match crate::sys::pty::spawn_shell_on_tty(&fs, &config.paths.shell, t) {
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
pub(super) async fn drive_picker_loop(
    state: &mut PickerState,
    console: &mut dyn Console,
) -> Result<()> {
    let mut dirty = true;
    loop {
        if dirty {
            render_picker(console, state)?;
            dirty = false;
        }
        match console.poll_event(POLL_SLICE).await? {
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
            // No scrollback in the console picker; ignore wheel notches.
            // The central layer's `UserHasInteracted` notice is a no-op
            // here — the real key that follows drives the picker.
            Some(
                crate::ui::console::ConsoleEvent::Scroll { .. }
                | crate::ui::console::ConsoleEvent::UserHasInteracted,
            )
            | None => {}
        }
    }
}

/// Issue one frame paint via [`Console::draw_with`]. Keeping the
/// renderer behind a thin wrapper localises the borrow of `state` so
/// the closure stays `FnMut` and doesn't capture overlapping
/// references.
fn render_picker(console: &mut dyn Console, state: &PickerState) -> Result<()> {
    console.draw_with(&mut |frame: &mut Frame<'_>| render_picker_frame(frame, state))
}

/// True iff `display_target` is one of the operator's selected
/// targets. The relay path uses this to decide whether to suspend
/// the live [`Console`] (display overlap) or fire-and-forget (no
/// overlap).
pub fn display_overlaps_targets(display_target: &Path, targets: &[PathBuf]) -> bool {
    targets.iter().any(|t| t == display_target)
}
