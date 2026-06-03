//! The non-interactive secure-boot REFUSE countdown (R-1 / R-13 / FIX-18 /
//! FIX-41). ALWAYS-COMPILED (the always-compiled refuse terminus renders
//! through it; the countdown default is read cfg-aware).
//!
//! This is the ONE shared refuse-render entry: the `run_tui_session` Err
//! arm calls [`run_refuse_screen`] for an `NmblError::PolicyRefused`, and
//! the pre-console gate placements DEFER their countdown here too (FIX-35),
//! so there is exactly one place that renders a refuse. It:
//!
//! 1. Runs the security teardown via [`super::relock::relock_and_refuse`]
//!    (cap → close-mappers → sentinel → relock), which mints the
//!    [`crate::policy::Sealed`] witness and yields the type-gated
//!    [`TerminalAction::RebootIntoRescue`]. This happens BEFORE the
//!    countdown renders (R-13).
//! 2. Snapshots a SCRUBBED log (a fixed banner only, FIX-41) so the Ctrl+L
//!    viewer can never surface the pre-refuse transcript.
//! 3. Renders a non-interactive countdown by composing
//!    [`crate::ui::countdown::CountdownScreen`] with a CLOSED, default-DENY
//!    input dispatch: ONLY Enter (reboot now) and Ctrl+L (logs) act; every
//!    other key is ignored AND does not shorten the countdown (FIX-18). A
//!    timeout reboots.
//! 4. Returns the `RebootIntoRescue` value untouched; the actual
//!    `reboot(RB_AUTOBOOT)` fires only in `execute_terminal_action` after
//!    the stack unwinds and every `Drop` runs (R-13). This module renders
//!    no further UI and never execve's a shell.
//!
//! It owns its input and delegates only rendering; it holds NO
//! module-level / thread-local state.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Alignment;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Paragraph, Wrap};

use crate::config::Config;
use crate::error::NmblError;
use crate::sys::poller::LocalSender;
use crate::terminal::TerminalAction;
use crate::ui::console::{Console, ConsoleEvent};
use crate::ui::countdown::{CountdownAction, CountdownOutcome, CountdownScreen};
use crate::ui::log_viewer::LogViewer;

/// The fixed banner the scrubbed Ctrl+L log viewer shows (FIX-41): no
/// pre-refuse transcript, only this statement of what happened.
const SCRUBBED_BANNER: &str = "nmbl: boot REFUSED by secure-boot policy.";

/// The refuse countdown, in seconds. `secure-boot` builds read the operator
/// knob `[secure_boot].refuse_countdown_seconds`; feature-free builds use
/// the single-sourced default (R-13/FIX-39).
fn refuse_countdown_seconds(config: &Config) -> u32 {
    #[cfg(feature = "secure-boot")]
    {
        config.secure_boot.refuse_countdown_seconds
    }
    #[cfg(not(feature = "secure-boot"))]
    {
        let _ = config;
        crate::security_consts::REFUSE_COUNTDOWN_SECONDS
    }
}

/// What the closed refuse dispatch decided one input event means. The
/// classifier records this for the caller to read after the countdown
/// returns `Cancelled`; every value not listed here is an IGNORED key that
/// neither acts nor shortens the countdown (default-DENY — FIX-18).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefuseKey {
    /// Enter ⇒ reboot now.
    RebootNow,
    /// Ctrl+L ⇒ open the scrubbed log viewer, then resume the countdown.
    OpenLogs,
}

/// Classify one [`ConsoleEvent`] under the CLOSED refuse policy. Returns
/// `Some(RefuseKey)` for the two allowed keys (the countdown cancels so the
/// caller can act), `None` for everything else (default-DENY: the countdown
/// keeps ticking, unshortened). Pure — unit-testable without a console.
fn classify(event: ConsoleEvent) -> Option<RefuseKey> {
    let ConsoleEvent::Key(key) = event else {
        return None;
    };
    // Enter (no modifier needed) reboots now.
    if key.code == KeyCode::Enter {
        return Some(RefuseKey::RebootNow);
    }
    // Ctrl+L opens the logs.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('l') {
        return Some(RefuseKey::OpenLogs);
    }
    // DEFAULT-DENY: any other key is ignored and does NOT shorten the
    // countdown (FIX-18).
    None
}

/// Render + run the refuse countdown and return the `RebootIntoRescue`
/// terminal action. The security teardown (relock + sentinel + seal) runs
/// FIRST, before any UI (R-13). `cause` is the originating failure.
pub async fn run_refuse_screen(
    config: &Config,
    console: &mut dyn Console,
    cause: NmblError,
    sender: &LocalSender,
) -> TerminalAction {
    // (1) Security actions FIRST: cap → close-mappers → sentinel → relock,
    // minting the Sealed witness inside the returned RebootIntoRescue.
    let action = super::relock::relock_and_refuse(config, cause, sender).await;

    // (2) Scrub the log: a fixed banner only, so Ctrl+L cannot expose the
    // pre-refuse transcript (FIX-41).
    let scrubbed = vec![SCRUBBED_BANNER.to_string()];

    // (3) Render the countdown. A render failure must NOT keep us in the
    // refuse screen — we already hold the terminal action, so on any render
    // error we fall straight through to returning it (reboot).
    let secs = u64::from(refuse_countdown_seconds(config));
    let _ = drive_countdown(console, Duration::from_secs(secs), &scrubbed).await;

    // (4) Return the RebootIntoRescue; the reboot syscall fires in
    // execute_terminal_action after the stack unwinds.
    action
}

/// Drive the countdown with the closed Enter/Ctrl+L dispatch, PAUSING for
/// the log viewer so opening logs never shortens the countdown (FIX-18).
/// Returns once Enter is pressed or the deadline expires.
async fn drive_countdown(
    console: &mut dyn Console,
    total: Duration,
    scrubbed: &[String],
) -> crate::error::Result<()> {
    // Track the remaining budget across log-viewer excursions. Opening the
    // logs pauses the countdown (the operator is actively reading), so we
    // re-anchor `remaining` to exactly what it was when Ctrl+L was hit.
    let mut remaining = total;
    loop {
        let key = std::cell::Cell::new(None);
        let started = Instant::now();
        let outcome = CountdownScreen::new(remaining)
            .run(
                console,
                |event| match classify(event) {
                    Some(k) => {
                        key.set(Some(k));
                        CountdownAction::Cancel
                    }
                    None => CountdownAction::Continue,
                },
                |c, secs| render_frame(c, secs),
            )
            .await?;
        match outcome {
            // Deadline passed ⇒ reboot.
            CountdownOutcome::Expired => return Ok(()),
            CountdownOutcome::Cancelled => match key.get() {
                Some(RefuseKey::RebootNow) | None => return Ok(()),
                Some(RefuseKey::OpenLogs) => {
                    // Compute the budget left at the moment Ctrl+L was hit,
                    // open the scrubbed viewer, then resume from exactly
                    // that budget — the log view does not count against the
                    // countdown (FIX-18: "must NOT shorten the countdown").
                    remaining = remaining.saturating_sub(started.elapsed());
                    let mut viewer = LogViewer::open_scrubbed(scrubbed.to_vec());
                    viewer.run(console).await?;
                }
            },
        }
    }
}

/// Paint one countdown frame: a red REFUSED banner, the remaining seconds,
/// and the closed key hint. Self-contained ratatui (no selector `App`).
fn render_frame(console: &mut dyn Console, secs: u64) -> crate::error::Result<()> {
    console.draw_with(&mut |frame| paint(frame, secs))
}

/// The actual ratatui paint. ASCII-only glyphs so the splash framebuffer
/// glyph cache renders every character.
fn paint(frame: &mut Frame<'_>, secs: u64) {
    let area = frame.area();
    let title = Span::styled(
        "  SECURE BOOT: REFUSED  ",
        Style::default()
            .fg(Color::White)
            .bg(Color::Red)
            .add_modifier(Modifier::BOLD),
    );
    let body = Text::from(vec![
        Line::from(""),
        Line::from(Span::styled(
            "This boot was refused by secure-boot policy.",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from("The TPM is locked and the system will reboot into rescue."),
        Line::from(""),
        Line::from(Span::styled(
            format!("Rebooting in {secs}s ..."),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("[Enter] reboot now    [Ctrl+L] view logs"),
    ]);
    let paragraph = Paragraph::new(body)
        .block(Block::bordered().title(title))
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true });
    frame.render_widget(paragraph, area);
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert on contract failures"
)]
#[path = "refuse_screen_tests.rs"]
mod tests;
