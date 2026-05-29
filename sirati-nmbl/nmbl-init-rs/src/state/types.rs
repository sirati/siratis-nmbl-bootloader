use nonmax::NonMaxU32;
use serde::{Deserialize, Serialize};

/// `state_format_version` that this binary knows how to write. A read
/// that decodes a *higher* version logs a warning and falls back to the
/// non-stateful path (so an old `nmbl-init` doesn't trample the file).
pub(crate) const KNOWN_VERSION: u32 = 1;

/// Fixed on-disk slot size. Sized generously so we can grow the schema
/// across many minor releases before bumping the layout.
pub(crate) const FILE_SIZE: usize = 16 * 1024;

/// Persistent boot state.
///
/// **Forward-compat contract:** this struct intentionally does NOT
/// carry `#[serde(deny_unknown_fields)]` — every other config struct in
/// this crate does, but `State` is the deliberate exception. An older
/// `nmbl-init` MUST be able to decode a `state.bin` written by a newer
/// installer, ignoring any fields it doesn't recognise. Conversely,
/// every post-v1 field MUST carry `#[serde(default)]` so a newer binary
/// reading an older `state.bin` fills the gap rather than erroring.
///
/// The exceptions are `state_format_version` (no sensible default; the
/// file must always carry it explicitly) and the v1 fields — but even
/// those get `#[serde(default)]` for permissive decoding when an
/// out-of-band tool writes a hand-rolled state.bin.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct State {
    /// Wire-format version. `KNOWN_VERSION` for files this binary
    /// writes; a read of a higher version triggers the fallback path.
    pub state_format_version: u32,

    /// The generation the most recent boot attempted (or `None` if the
    /// installer just wrote a fresh state.bin and nothing has booted
    /// yet). `NonMaxU32` lets `Option<…>` fit in four bytes via niche
    /// optimisation.
    #[serde(default)]
    pub last_attempted_generation: Option<NonMaxU32>,

    /// `true` once `--boot-succeeded` has been invoked since the last
    /// installer-initiated rewrite.
    #[serde(default)]
    pub last_boot_succeeded: bool,

    /// Counter that increments each time the boot-decision logic falls
    /// back to a known-good generation. Reset to zero on a successful
    /// boot (`--boot-succeeded`).
    #[serde(default)]
    pub recovery_attempt: u32,

    /// Ring of recently-good generations. Sized at 20 — comfortably
    /// covers the default NixOS retention window without blowing the
    /// 16 KiB on-disk slot. Empty slots are `None`.
    #[serde(default = "default_known_good")]
    pub known_good_generations: [Option<NonMaxU32>; 20],
}

pub(crate) fn default_known_good() -> [Option<NonMaxU32>; 20] {
    [None; 20]
}

impl Default for State {
    fn default() -> Self {
        // `last_boot_succeeded` starts `true` so "no failure has been
        // recorded yet" is the semantic of a fresh state.bin. A `false`
        // value means we positively know the previous boot did not reach
        // its success target. Without this, the installer's
        // `--init-state` would write a file that the next boot reads as
        // "failed boot, no rollback target" and routes straight to the
        // emergency screen.
        Self {
            state_format_version: KNOWN_VERSION,
            last_attempted_generation: None,
            last_boot_succeeded: true,
            recovery_attempt: 0,
            known_good_generations: [None; 20],
        }
    }
}

/// Outcome of the boot-time rollback decision.
///
/// Returned by [`decide`] and consumed by the caller (see Phase 4.2's
/// `select_and_act`) which performs the on-disk write-back and `kexec_into`
/// dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatefulDecision {
    /// Healthy boot — honour whatever generation the operator (TUI /
    /// timeout default) picked. The caller still records the choice in
    /// `last_attempted_generation` before kexec.
    HonourTui,
    /// In-progress recovery — boot the generation at this index in the
    /// scanned `generations` slice. The caller MUST persist `state`
    /// (which `decide` has already mutated) before invoking kexec.
    ForcePick(usize),
    /// Recovery budget exhausted; the caller must surface this as a
    /// rescue condition. `decide` deliberately leaves `state` untouched
    /// in this branch.
    Exhausted,
}
