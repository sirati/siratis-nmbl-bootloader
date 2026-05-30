use std::borrow::Cow;
use std::time::Duration;

use crossterm::event::KeyCode;

use crate::error::Result;
use crate::log;
use crate::ui::app::{App, ModalKind};
use crate::ui::console::Console;

use super::types::{ProgressSink, TickOutcome};

/// Slice we wait on input per tick. Matches the 100 ms poll cadence of
/// the device-wait loop so the tick stays cheap and the operator's
/// Esc keypress aborts within one iteration.
const TICK_POLL_SLICE: Duration = Duration::from_millis(100);

/// Number of log lines pulled from the ring on every refresh.
///
/// Larger than any plausible visible panel height — the renderer clips
/// to what fits, so we err on the side of "have enough" rather than
/// peeking at the backend's grid size every frame.
const LOG_SNAPSHOT_LINES: usize = 64;

/// Where the reporter writes its phase / log / spinner state.
///
/// Two modes:
/// - `Owned` carries its own `App<'static>` parked on
///   [`crate::ui::app::Screen::BootStatus`]. Used in early boot before
///   any selector / menu screen exists — there is nothing to overlay,
///   so the full-screen boot-status view is appropriate.
/// - `Overlay` borrows an externally-supplied `&mut App` and writes to
///   its [`crate::ui::app::App::modal`] field as a
///   [`ModalKind::Status`] overlay. Used by the emergency-action
///   subflows (Retry boot, Verify kexec readiness) so the underlying
///   emergency menu stays visible behind the progress dialog.
pub(super) enum ReporterApp<'a> {
    // App<'static> is ~296 bytes (Screen, modal, Vecs, options); the
    // Overlay variant is an 8-byte reference. Boxing the owned App
    // keeps both variants the same size — silences clippy's
    // `large_enum_variant` lint without adding indirection in the
    // hot path (the reporter is constructed once per phase, not per
    // tick).
    Owned(Box<App<'static>>),
    // The inner `'static` bound on `App<'static>` matches the fact
    // that the only App ever used in overlay mode is the emergency-
    // menu App built by `build_emergency_app(&[])` — its `generations`
    // slice is empty (`&[]`, `'static`). Two separate lifetimes would
    // require a HRTB on every BootReporter signature, which we avoid
    // by pinning the inner App parameter here.
    Overlay(&'a mut App<'static>),
}

impl ReporterApp<'_> {
    pub(super) fn as_ref(&self) -> &App<'_> {
        match self {
            ReporterApp::Owned(a) => a,
            ReporterApp::Overlay(a) => a,
        }
    }
}

/// Boot status reporter — a thin wrapper around `&mut dyn Console` plus
/// the active [`App`] so phase code can report status without needing
/// to know the underlying render plumbing.
///
/// In `Owned` mode the reporter parks its own [`App`] on
/// [`crate::ui::app::Screen::BootStatus`]; in `Overlay` mode it pumps
/// the supplied App's [`crate::ui::app::App::modal`] field with a
/// [`ModalKind::Status`] so the underlying menu stays visible behind.
pub struct BootReporter<'c, 'a> {
    pub console: &'c mut dyn Console,
    pub(super) app: ReporterApp<'a>,
}

impl<'c> BootReporter<'c, 'static> {
    /// Build an owned-mode reporter parked on the boot-status screen
    /// with the given initial phase label. Does NOT render — the caller
    /// decides when the first frame is meaningful (typically right after
    /// construction via [`Self::set_phase`] or [`Self::refresh_log`]).
    ///
    /// Used by the early-boot phases (phase 1, 2a, 2b, 4) where no
    /// underlying menu exists yet.
    pub fn new(console: &'c mut dyn Console, phase: impl Into<Cow<'static, str>>) -> Self {
        let app = App::boot_status(phase);
        Self {
            console,
            app: ReporterApp::Owned(Box::new(app)),
        }
    }
}

impl<'c, 'a> BootReporter<'c, 'a> {
    /// Borrow the App the reporter is currently driving. Lets test
    /// code inspect the latest phase / screen state without taking a
    /// dependency on the internal `ReporterApp` enum.
    pub fn app(&self) -> &App<'_> {
        self.app.as_ref()
    }

    /// Build an overlay-mode reporter that writes its status to the
    /// supplied App's `modal` field. The underlying screen (typically
    /// the emergency menu) keeps rendering behind so the operator can
    /// see "where they were"; closing the reporter (drop) clears the
    /// modal automatically.
    ///
    /// Used by emergency-action subflows so the menu stays visible.
    pub fn overlay(
        console: &'c mut dyn Console,
        app: &'a mut App<'static>,
        phase: impl Into<Cow<'static, str>>,
    ) -> Self {
        app.modal = Some(ModalKind::Status {
            phase: phase.into().into_owned(),
            log_lines: Vec::new(),
            spinner_frame: 0,
        });
        Self {
            console,
            app: ReporterApp::Overlay(app),
        }
    }

    /// Replace the phase label, refresh the log snapshot, and render.
    ///
    /// This is the canonical "phase transition" call: in one go we
    /// update everything the operator sees so a slow phase doesn't
    /// leave a stale label on screen.
    pub fn set_phase(&mut self, phase: impl Into<Cow<'static, str>>) -> Result<()> {
        let snap = log::snapshot(LOG_SNAPSHOT_LINES);
        write_phase(&mut self.app, phase, Some(snap), false);
        self.console.render(self.app.as_ref())
    }

    /// Refresh the log panel from the global ring and re-render.
    ///
    /// Cheap enough to call on every `tick()`; the ring is a small
    /// `VecDeque<String>` clone of the most recent lines. Does NOT
    /// change the phase string — both modes pull the prior phase
    /// through unmodified.
    pub fn refresh_log(&mut self) -> Result<()> {
        let snap = log::snapshot(LOG_SNAPSHOT_LINES);
        match &mut self.app {
            ReporterApp::Owned(a) => a.set_boot_log_lines(snap),
            ReporterApp::Overlay(a) => {
                if let Some(ModalKind::Status { log_lines, .. }) = &mut a.modal {
                    *log_lines = snap;
                }
            }
        }
        self.console.render(self.app.as_ref())
    }

    /// Advance the spinner one frame and render.
    ///
    /// Designed to be called inside device-wait spin loops by sibling
    /// subagent work so the operator sees the boot is alive even when
    /// no phase transition is firing.
    pub fn tick(&mut self) -> Result<()> {
        tick_spinner(&mut self.app);
        self.console.render(self.app.as_ref())
    }
}

// Intentionally no `Drop` impl: a non-trivial Drop holds the
// `&mut Console` borrow until end-of-scope (NLL can't release it
// early), which breaks every existing test of the form
// `let reporter = …; … ; assert_eq!(console.field, …)`. Overlay
// callers (`crate::ui::emergency_actions`) instead drop the reporter
// in a `{}` block; the next `BootReporter::overlay` overwrites
// `app.modal`, and `drop_to_emergency` clears `app.modal = None`
// at the top of every loop iteration so a stale overlay never
// reaches the picker.

/// Apply a phase / log-snapshot / spinner update to whichever App the
/// reporter is driving. In owned mode this mutates `Screen::BootStatus`;
/// in overlay mode it mutates the `ModalKind::Status` in `app.modal`.
fn write_phase(
    app: &mut ReporterApp<'_>,
    phase: impl Into<Cow<'static, str>>,
    log_lines: Option<Vec<String>>,
    spinner_advance: bool,
) {
    let phase = phase.into();
    match app {
        ReporterApp::Owned(a) => {
            a.set_boot_phase(phase);
            if let Some(lines) = log_lines {
                a.set_boot_log_lines(lines);
            }
            if spinner_advance {
                a.tick_boot_spinner();
            }
        }
        ReporterApp::Overlay(a) => {
            if let Some(ModalKind::Status {
                phase: p,
                log_lines: l,
                spinner_frame,
            }) = &mut a.modal
            {
                *p = phase.into_owned();
                if let Some(lines) = log_lines {
                    *l = lines;
                }
                if spinner_advance {
                    *spinner_frame = spinner_frame.wrapping_add(1) % crate::ui::app::SPINNER_FRAMES;
                }
            } else {
                // Defence-in-depth: a caller that swapped the modal out
                // from under us re-installs a fresh Status so the next
                // tick still paints.
                a.modal = Some(ModalKind::Status {
                    phase: phase.into_owned(),
                    log_lines: log_lines.unwrap_or_default(),
                    spinner_frame: 0,
                });
            }
        }
    }
}

fn tick_spinner(app: &mut ReporterApp<'_>) {
    match app {
        ReporterApp::Owned(a) => a.tick_boot_spinner(),
        ReporterApp::Overlay(a) => {
            if let Some(ModalKind::Status { spinner_frame, .. }) = &mut a.modal {
                *spinner_frame = spinner_frame.wrapping_add(1) % crate::ui::app::SPINNER_FRAMES;
            }
        }
    }
}

impl ProgressSink for BootReporter<'_, '_> {
    /// Update the phase label, refresh the log snapshot, advance the
    /// spinner, render, and poll the backend for an abort key.
    ///
    /// Errors from the backend are deliberately dropped: a flaky DRM
    /// ioctl shouldn't abort a 30 s device wait — the next iteration
    /// will retry. Phase code still sees a fatal error if the
    /// underlying wait itself fails.
    ///
    /// Returns [`TickOutcome::Aborted`] when the operator presses Esc
    /// on the boot-status screen — the caller (`devices::wait_for`,
    /// `activation` waits, …) surfaces this as
    /// [`crate::error::NmblError::OperatorAborted`] so the emergency
    /// menu can re-appear with the operator's explicit "abort"
    /// context.
    fn tick(&mut self, phase: &str) -> TickOutcome {
        let snap = log::snapshot(LOG_SNAPSHOT_LINES);
        write_phase(
            &mut self.app,
            Cow::<'static, str>::Owned(phase.to_string()),
            Some(snap),
            true,
        );
        let _ = self.console.render(self.app.as_ref());

        // Poll for a single key with a short timeout so the wait stays
        // responsive without adding latency beyond the existing 100 ms
        // POLL_INTERVAL in `devices::wait_for`. A failed poll (transient
        // DRM / tty error) is treated as "no key" — same swallowing
        // policy as the render above.
        match self.console.poll_key(TICK_POLL_SLICE) {
            Ok(Some(key)) if key.code == KeyCode::Esc => TickOutcome::Aborted,
            _ => TickOutcome::Continue,
        }
    }

    fn render_phase(&mut self, phase: &str) {
        let snap = log::snapshot(LOG_SNAPSHOT_LINES);
        write_phase(
            &mut self.app,
            Cow::<'static, str>::Owned(phase.to_string()),
            Some(snap),
            true,
        );
        let _ = self.console.render(self.app.as_ref());
    }
}
