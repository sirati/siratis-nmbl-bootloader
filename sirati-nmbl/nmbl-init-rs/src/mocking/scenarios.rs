//! Per-scenario runner functions for the mocking harness.

use std::time::Duration;

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::ui::POLL_SLICE;
use crate::ui::app::App;
use crate::ui::console::{Console, ConsoleEvent};
use crate::ui::{
    SessionInteraction, passphrase_prompt_on_console, show_modal_buttons, show_modal_confirm,
    show_modal_error, show_wrong_password_modal,
};

use super::console::MockConsole;

pub(super) async fn run_modal_error(console: &mut MockConsole, args: &[String]) -> Result<()> {
    let title = arg_or_default(args, 0, "Error");
    let body = arg_or_default(args, 1, "");
    // Long timeout so the test runner has time to capture the pane.
    show_modal_error(console, &title, &body, Duration::from_secs(3600)).await?;
    eprintln!("[mocking] modal-error dismissed");
    Ok(())
}

pub(super) async fn run_modal_confirm(console: &mut MockConsole, args: &[String]) -> Result<()> {
    let title = arg_or_default(args, 0, "Confirm");
    let body = arg_or_default(args, 1, "");
    let yes = arg_or_default(args, 2, "Yes");
    let no = arg_or_default(args, 3, "No");
    let outcome = show_modal_confirm(console, &title, &body, &yes, &no, true).await?;
    eprintln!("[mocking] modal-confirm outcome={outcome:?}");
    Ok(())
}

pub(super) async fn run_modal_buttons(console: &mut MockConsole, args: &[String]) -> Result<()> {
    let Some(title) = args.first().cloned() else {
        return Err(NmblError::Io {
            source: std::io::Error::other("modal-buttons requires <title> <body> <label…>"),
            context: "mocking harness".to_string(),
        });
    };
    let body = arg_or_default(args, 1, "");
    let labels: Vec<String> = args.iter().skip(2).cloned().collect();
    if labels.is_empty() {
        return Err(NmblError::Io {
            source: std::io::Error::other("modal-buttons requires at least one button label"),
            context: "mocking harness".to_string(),
        });
    }
    let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    let outcome = show_modal_buttons(
        console,
        &title,
        &body,
        &label_refs,
        "Left/Right select  Enter confirm  Esc cancel",
    )
    .await?;
    eprintln!("[mocking] modal-buttons outcome_idx={outcome:?}");
    Ok(())
}

pub(super) async fn run_wrong_password(console: &mut MockConsole, args: &[String]) -> Result<()> {
    let attempt: u32 = args.first().map(|s| s.parse().unwrap_or(1)).unwrap_or(1);
    let outcome = show_wrong_password_modal(console, attempt).await?;
    eprintln!("[mocking] wrong-password outcome={outcome:?}");
    Ok(())
}

/// Drive the production passphrase modal on the harness console. Same
/// `passphrase_prompt_on_console` entry point the LUKS activation path
/// calls, so a tmux-driven smoke test exercises the exact code that
/// runs at boot. On Enter the entered string is reported on stderr (in
/// quotes so leading/trailing whitespace is visible); on Esc-cancel the
/// supplier returns `NmblError::Tui`, which we surface as
/// `[mocking] passphrase cancelled` on stderr and exit cleanly so the
/// test harness can distinguish the two outcomes from the exit code.
pub(super) async fn run_passphrase(console: &mut MockConsole, args: &[String]) -> Result<()> {
    let label = arg_or_default(args, 0, "Unlock root");
    match passphrase_prompt_on_console(console, &label, &SessionInteraction::new()).await {
        Ok(secret) => {
            eprintln!("[mocking] passphrase entered='{}'", &**secret);
            Ok(())
        }
        Err(_) => {
            eprintln!("[mocking] passphrase cancelled");
            Ok(())
        }
    }
}

/// Drive the resize-event plumbing end-to-end on the harness console.
///
/// Scripts two synthetic [`ConsoleEvent::Resize`] events at different
/// sizes and a final key press. Between each event the modal repaints
/// against the new size so a tmux harness can `capture-pane` the
/// before / after dimensions and confirm the layout actually changed.
///
/// The exact sizes can be overridden on the command line:
/// `--debug-tui resize <r1> <c1> <r2> <c2>` — defaults are
/// `40x100`, `20x60`, then any key to dismiss.
pub(super) async fn run_resize(console: &mut MockConsole, args: &[String]) -> Result<()> {
    let rows1: u16 = parse_u16_arg(args, 0).unwrap_or(40);
    let cols1: u16 = parse_u16_arg(args, 1).unwrap_or(100);
    let rows2: u16 = parse_u16_arg(args, 2).unwrap_or(20);
    let cols2: u16 = parse_u16_arg(args, 3).unwrap_or(60);

    let title = "Resize harness";
    let body = format!(
        "Stage 1: waiting for resize to {cols1}x{rows1}.\n\
         Then resize to {cols2}x{rows2}.\n\
         Then press any key to exit."
    );
    let hint = "drives two synthetic ConsoleEvent::Resize events, then a key";
    let labels = ["OK"];

    // Stage 1: paint at the harness's current size.
    paint_resize_stage(console, title, &body, &labels, hint)?;
    eprintln!("[mocking] resize stage=0 size={:?}", Console::size(console));

    // Stage 2: fire the first synthetic resize then re-paint.
    console.script(ConsoleEvent::Resize {
        rows: rows1,
        cols: cols1,
    });
    drain_one_event(console).await?;
    paint_resize_stage(console, title, &body, &labels, hint)?;
    eprintln!("[mocking] resize stage=1 size={:?}", Console::size(console));

    // Stage 3: second resize.
    console.script(ConsoleEvent::Resize {
        rows: rows2,
        cols: cols2,
    });
    drain_one_event(console).await?;
    paint_resize_stage(console, title, &body, &labels, hint)?;
    eprintln!("[mocking] resize stage=2 size={:?}", Console::size(console));

    // Stage 4: wait for a real key press so tmux captures can land.
    let deadline = std::time::Instant::now() + Duration::from_secs(3600);
    while let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) {
        let slice = remaining.min(POLL_SLICE);
        match console.poll_event(slice).await? {
            Some(ConsoleEvent::Key(_)) => break,
            // Any further resize / scroll / interaction-notice / no event:
            // re-paint and wait again.
            Some(
                ConsoleEvent::Resize { .. }
                | ConsoleEvent::Scroll { .. }
                | ConsoleEvent::UserHasInteracted,
            )
            | None => continue,
        }
    }
    eprintln!("[mocking] resize dismissed");
    Ok(())
}

fn paint_resize_stage(
    console: &mut MockConsole,
    title: &str,
    body: &str,
    labels: &[&str],
    hint: &str,
) -> Result<()> {
    let (cols, rows) = Console::size(console);
    let resized_body = format!("{body}\n\nObserved size: cols={cols} rows={rows}");
    let data = crate::ui::view::ModalButtonsScreenData {
        title,
        message: &resized_body,
        labels,
        selected: 0,
        hint,
        scroll_offset: 0,
    };
    console.draw_with(&mut |frame| crate::ui::view::render_modal_buttons(frame, &data))
}

/// Drain a single event from the harness queue, ignoring whatever it
/// is. Used after `script()` to ensure the synthetic event has been
/// applied to `last_resize` before the next paint.
async fn drain_one_event(console: &mut MockConsole) -> Result<()> {
    let _ = console.poll_event(Duration::from_millis(0)).await?;
    Ok(())
}

fn parse_u16_arg(args: &[String], idx: usize) -> Option<u16> {
    args.get(idx).and_then(|s| s.parse().ok())
}

pub(super) async fn run_boot_status(console: &mut MockConsole, args: &[String]) -> Result<()> {
    use crate::ui::app::Screen;
    let phase = arg_or_default(args, 0, "phase X");
    let log_lines: Vec<String> = args.iter().skip(1).cloned().collect();
    let mut app = App::boot_status(phase.clone());
    if let Screen::BootStatus(data) = &mut app.screen {
        data.log_lines = log_lines;
    }
    // One paint then wait for any key (or 1h timeout) so tmux can
    // capture the rendered cells.
    console.render(&app)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(3600);
    while let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) {
        let slice = remaining.min(POLL_SLICE);
        if let Some(ConsoleEvent::Key(_)) = console.poll_event(slice).await? {
            break;
        }
    }
    eprintln!("[mocking] boot-status dismissed");
    Ok(())
}

/// Drive the real emergency menu (`shell::drop_to_emergency`) on the
/// host terminal so the menu, the console picker, Raw Shell, and the
/// error-display behaviour can be exercised without a VM. A synthetic
/// boot error seeds the screen; the optional first arg overrides the
/// shell path (default `/bin/sh`) so the picker can spawn a real shell
/// on the host's controlling tty.
///
/// This is primarily a manual / tmux-driven smoke test for the
/// latest-error display fix and the Raw Shell spawn: pick `Raw Shell`,
/// keep the current tty checked, hit `Spawn`, and confirm a live shell
/// appears. The picker resolves real `/dev/...` targets, so run it from
/// a real terminal (a tmux pane is ideal).
pub(super) async fn run_emergency(
    _console: &mut MockConsole,
    args: &[String],
    sender: &crate::sys::poller::LocalSender,
) -> Result<()> {
    let mut config = Config::recovery_default();
    if let Some(shell) = args.first() {
        config.paths.shell = std::path::PathBuf::from(shell);
    }
    // drop_to_emergency owns its console; hand it a fresh boxed
    // MockConsole (same stdin/stdout this process already uses). We are
    // already inside the poller-backed runtime, so await the async
    // emergency session directly with the live `LocalSender`.
    let boxed: Box<dyn Console> = Box::new(MockConsole::new()?);
    let err = NmblError::Io {
        source: std::io::Error::other("synthetic boot failure (mocking harness)"),
        context: "phase-3 generation discovery".to_string(),
    };
    let session = crate::ui::app::SessionInteraction::new();
    let action = crate::shell::drop_to_emergency(boxed, &config, err, &session, sender).await;
    eprintln!("[mocking] emergency action={action:?}");
    Ok(())
}

pub(super) fn arg_or_default(args: &[String], idx: usize, default: &str) -> String {
    args.get(idx)
        .cloned()
        .unwrap_or_else(|| default.to_string())
}
