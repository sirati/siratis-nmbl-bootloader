//! Ratatui screens for the network-rescue flow (PLAN.md Phase E.2).
//!
//! E.1 landed [`crate::rescue::net::try_network_rescue`] driving a
//! [`crate::rescue::net::RescueUi`] trait object. This module ships
//! [`RatatuiRescueUi`] — the production implementation that paints
//! through the orchestrator-held [`crate::ui::console::Console`]
//! handle. The fallback [`crate::rescue::net::ConsoleRescueUi`] stays
//! in `src/rescue/net.rs` as a test/serial-console double.
//!
//! Four screens, each in its own private function:
//!
//! * [`pick_source`] — operator chooses Network / Reboot / Halt after
//!   disk rescue failed. Header surfaces the disk error reason verbatim.
//! * [`prompt_url`] — single-line URL editor pre-filled from
//!   `rescue.default_url`. Enter confirms, Esc aborts.
//! * [`progress`] — gauge bar over the download. Falls back to a byte
//!   counter + spinner when `Content-Length` is unknown.
//! * [`confirm_hash`] — side-by-side computed vs. expected hex panes
//!   with an editable expected field and a red MISMATCH banner when
//!   the two disagree.
//!
//! Every screen renders through the same `&mut dyn Console` the boot
//! selector and emergency screen already hold, so no parallel
//! /dev/console session is opened — the splash framebuffer or
//! raw-mode tty in the orchestrator's hand stays the single render
//! target for the whole boot.

use std::time::Duration;

use crate::error::Result;
use crate::rescue::net::{DownloadStatus, HashConfirmation, RescueSource, RescueUi};
use crate::ui::console::Console;

mod confirm_hash;
mod helpers;
mod pick_source;
mod progress;
mod prompt_url;

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert on render contract"
)]
mod tests;

/// Throttle progress repaints so a multi-megabyte download doesn't
/// burn the serial line at gigabyte-per-second redraw rates.
const PROGRESS_REDRAW_INTERVAL: Duration = Duration::from_millis(100);

/// Production [`RescueUi`] backed by ratatui, painting through the
/// orchestrator-held [`Console`] handle. No new console is opened —
/// the same splash or tty backend the boot selector used keeps owning
/// `/dev/console` for the lifetime of the rescue flow.
pub struct RatatuiRescueUi<'c> {
    /// Live boot console borrowed from the orchestrator. Every screen
    /// renders into this through [`Console::draw_with`] and polls
    /// keystrokes through [`Console::poll_key`].
    console: &'c mut dyn Console,
    /// Cursor position in the URL editor — preserved across redraws
    /// inside `prompt_url`. Stored on the struct so future screens
    /// can resume the same buffer.
    pub(crate) url_cursor: usize,
    /// Last expected-hash buffer the operator typed in `confirm_hash`.
    /// Lets repeated calls (e.g. after a Mismatch loop) keep the
    /// operator's manual override.
    pub(crate) expected_cursor: usize,
    /// Spinner phase index for the indeterminate progress bar. Stays
    /// across `progress` calls so the spinner actually spins instead
    /// of resetting to frame 0 on every chunk.
    pub(crate) spinner_phase: usize,
    /// Last time we painted a progress frame — used to throttle the
    /// redraw cadence.
    pub(crate) last_redraw: Option<std::time::Instant>,
}

impl<'c> RatatuiRescueUi<'c> {
    /// Construct a fresh UI bound to the orchestrator-held console.
    /// Cheap; allocates no terminal resources of its own.
    pub fn new(console: &'c mut dyn Console) -> Self {
        Self {
            console,
            url_cursor: 0,
            expected_cursor: 0,
            spinner_phase: 0,
            last_redraw: None,
        }
    }
}

impl RescueUi for RatatuiRescueUi<'_> {
    fn pick_source(&mut self, disk_reason: &str) -> Result<RescueSource> {
        // The rescue dispatcher runs on the synchronous force-on-boot
        // path with no enclosing runtime, but the screen loops drive the
        // async `Console::poll_event`. Spin up a throwaway current-thread
        // `LocalRuntime` and `block_on` the screen future.
        let rt = crate::ui::build_local_runtime()?;
        rt.block_on(pick_source::run_pick_source(self.console, disk_reason))
    }

    fn prompt_url(&mut self, prefill: &str) -> Result<String> {
        let cursor_seed = if self.url_cursor == 0 {
            prefill.len()
        } else {
            self.url_cursor.min(prefill.len())
        };
        let rt = crate::ui::build_local_runtime()?;
        let (out, final_cursor) = rt.block_on(prompt_url::run_prompt_url(
            self.console,
            prefill,
            cursor_seed,
        ))?;
        self.url_cursor = final_cursor;
        Ok(out)
    }

    fn progress(&mut self, status: DownloadStatus) {
        // Drop the redraw entirely if we painted too recently. Avoids
        // saturating /dev/console on multi-MB downloads.
        let now = std::time::Instant::now();
        if let Some(prev) = self.last_redraw
            && now.duration_since(prev) < PROGRESS_REDRAW_INTERVAL
        {
            return;
        }
        self.last_redraw = Some(now);
        self.spinner_phase = self.spinner_phase.wrapping_add(1);
        let phase = self.spinner_phase;
        // Errors on the progress repaint must not abort the download —
        // the operator can still confirm-or-abort on the hash screen.
        let _ = self
            .console
            .draw_with(&mut |f| progress::render_progress(f, status, phase));
    }

    fn confirm_hash(
        &mut self,
        computed_hex: &str,
        prefill_expected: &str,
    ) -> Result<HashConfirmation> {
        let cursor_seed = if self.expected_cursor == 0 {
            prefill_expected.len()
        } else {
            self.expected_cursor.min(prefill_expected.len())
        };
        let rt = crate::ui::build_local_runtime()?;
        let (out, final_cursor) = rt.block_on(confirm_hash::run_confirm_hash(
            self.console,
            computed_hex,
            prefill_expected,
            cursor_seed,
        ))?;
        self.expected_cursor = final_cursor;
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Constructor for the rescue dispatch path
// ---------------------------------------------------------------------------

/// Convenience constructor for the rescue dispatcher: returns the
/// production ratatui-backed UI bound to the orchestrator-held
/// [`Console`]. Kept here rather than in `rescue/mod.rs` so the
/// trait wiring stays inside the `ui` module.
#[must_use]
pub fn make_rescue_ui(console: &mut dyn Console) -> RatatuiRescueUi<'_> {
    RatatuiRescueUi::new(console)
}
