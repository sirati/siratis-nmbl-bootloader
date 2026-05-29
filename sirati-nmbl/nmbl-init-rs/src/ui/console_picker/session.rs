use std::path::{Path, PathBuf};

use ratatui::Frame;

use crate::config::Config;
use crate::error::Result;
use crate::nmbl_warn;
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
pub async fn run_picker_session(
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
    dispatch_spawn(
        console,
        config,
        targets,
        &display_target,
        |console, config, targets, display_target| {
            Box::pin(crate::ui::console_relay::run_relay(
                console,
                config,
                targets,
                display_target,
            ))
        },
        fire_and_forget_spawn,
    )
    .await
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
pub(super) async fn dispatch_spawn<R, D>(
    console: &mut dyn Console,
    config: &Config,
    targets: Vec<PathBuf>,
    display_target: &Path,
    mut relay_fn: R,
    mut detach_fn: D,
) -> Result<PickerSessionOutcome>
where
    R: for<'a> FnMut(
        &'a mut dyn Console,
        &'a Config,
        &'a [PathBuf],
        &'a Path,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + 'a>>,
    D: FnMut(&Config, &[PathBuf]) -> Result<()>,
{
    if display_overlaps_targets(display_target, &targets) {
        // The relay (overlap path) is async — it suspends the console,
        // pumps the PTY relay loop, then resumes. The callback is a
        // boxed-future seam so tests can drive the dispatch without
        // forking a real shell.
        relay_fn(console, config, &targets, display_target).await?;
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
            None => {}
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
