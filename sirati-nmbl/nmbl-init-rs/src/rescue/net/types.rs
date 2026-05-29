//! Public types and the [`RescueUi`] trait for the network-rescue flow.

use crate::error::Result;

/// Snapshot of in-flight download progress used by [`RescueUi::progress`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownloadStatus {
    /// Bytes received so far.
    pub bytes: u64,
    /// Total bytes expected from the HTTP `Content-Length` header.
    /// `None` when the origin closed the connection to signal EOF
    /// instead.
    pub total: Option<u64>,
}

/// Three-way operator choice from the source picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RescueSource {
    /// Attempt the network-rescue path.
    Network,
    /// Reboot the system (operator opts out of rescue entirely).
    Reboot,
    /// Halt the system (operator opts out of rescue entirely).
    Halt,
}

/// Outcome of the hash-confirm screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashConfirmation {
    /// Operator confirmed the computed hash matches the expected one.
    Confirmed,
    /// Operator flagged the hashes as mismatched; redownload.
    Mismatch,
    /// Operator aborted the whole rescue attempt.
    Aborted,
}

/// Interaction surface the network-rescue orchestrator needs. E.2
/// supplies a ratatui-backed implementation; until then the
/// `ConsoleRescueUi` here keeps the path testable end-to-end.
pub trait RescueUi {
    /// Source picker. `disk_reason` is the error chain from the most
    /// recent disk-rescue attempt — surfaced verbatim so the operator
    /// knows why they're here.
    fn pick_source(&mut self, disk_reason: &str) -> Result<RescueSource>;

    /// URL entry screen, pre-filled with `prefill`. Returns the final
    /// URL the operator confirmed (empty string allowed only when the
    /// caller re-validates).
    fn prompt_url(&mut self, prefill: &str) -> Result<String>;

    /// Progress callback while bytes are streaming. Called at least
    /// once per chunk; implementations should be cheap.
    fn progress(&mut self, status: DownloadStatus);

    /// Hash confirm screen — show the computed hex digest, let the
    /// operator confirm against `prefill_expected`.
    fn confirm_hash(
        &mut self,
        computed_hex: &str,
        prefill_expected: &str,
    ) -> Result<HashConfirmation>;
}
