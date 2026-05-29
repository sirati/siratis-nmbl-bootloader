/// Outcome of a single [`ProgressSink::tick`] call.
///
/// `Aborted` lets a blocking wait loop bail out cleanly when the
/// operator presses Esc on the boot-status screen; the caller is
/// expected to surface this as [`crate::error::NmblError::OperatorAborted`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickOutcome {
    /// No operator input demanded an abort; keep polling.
    Continue,
    /// Operator pressed Esc; the wait loop should stop and propagate
    /// an [`crate::error::NmblError::OperatorAborted`] up to its caller.
    Aborted,
}

/// Animated progress sink for blocking wait loops.
///
/// Implementors advance whatever spinner / status the operator sees and
/// re-render the underlying surface. Phase code passes
/// `Option<&mut dyn ProgressSink>` down through the wait helpers so tests
/// and headless contexts can skip the UI cost; production wires a
/// [`super::BootReporter`] through.
///
/// Implementations should be cheap enough to call every ~100 ms (the
/// existing poll cadence in `devices::wait_for`); skipping a render when
/// the phase string is unchanged is allowed but not required.
pub trait ProgressSink {
    /// Update the visible phase label, advance the spinner one frame,
    /// refresh the log snapshot, push a frame to the backend, and poll
    /// the backend for an abort key (Esc).
    ///
    /// The implementation is expected to swallow non-fatal render errors
    /// (e.g. transient DRM hiccups) rather than abort the wait — the
    /// boot must not fail because the spinner couldn't repaint. The
    /// only way `tick` should return [`TickOutcome::Aborted`] is when
    /// the operator pressed Esc on the boot-status screen.
    fn tick(&mut self, phase: &str) -> TickOutcome;
}
