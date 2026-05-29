//! Persistent boot state ("state.bin") shared between the installer and
//! the booted `nmbl-init`.
//!
//! The on-disk file lives in a tiny FAT/ext2 partition that both the
//! installer (`--init-state`, `--boot-succeeded`, …) and the booted
//! `/init` can mount RW. The wire format is CBOR via `ciborium` because
//! CBOR is self-describing — an older `nmbl-init` reading a newer file
//! can skip over unknown fields, which is the forward-compat property
//! that keeps a fleet bootable across upgrades.
//!
//! The file is always padded out to a fixed 16 KiB slot so we can rewrite
//! it in place without dancing around a smaller-then-larger payload
//! (which the FS would happily fragment). The `ciborium` decoder stops at
//! the end of the top-level map, so the trailing-zero padding is
//! transparent on read.

use std::fs::OpenOptions;
use std::io::{ErrorKind, Read, Write};
use std::path::Path;

use nonmax::NonMaxU32;
use serde::{Deserialize, Serialize};

use crate::error::NmblError;
use crate::{nmbl_info, nmbl_warn};

/// `state_format_version` that this binary knows how to write. A read
/// that decodes a *higher* version logs a warning and falls back to the
/// non-stateful path (so an old `nmbl-init` doesn't trample the file).
const KNOWN_VERSION: u32 = 1;

/// Fixed on-disk slot size. Sized generously so we can grow the schema
/// across many minor releases before bumping the layout.
const FILE_SIZE: usize = 16 * 1024;

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

fn default_known_good() -> [Option<NonMaxU32>; 20] {
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

/// Decode `state.bin` from `path`.
///
/// Returns `Ok(None)` for two distinct "graceful fallback" cases:
///   - The file doesn't exist yet (the installer never ran).
///   - The file decodes with a `state_format_version` strictly newer
///     than this binary supports — we emit a warning and let the caller
///     boot non-stateful rather than risk clobbering a future schema.
///
/// Lower-or-equal versions are accepted; serde defaults fill any gaps.
pub fn read(path: &Path) -> Result<Option<State>, NmblError> {
    let file = match OpenOptions::new().read(true).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(NmblError::Io {
                source: e,
                context: format!("opening state.bin at {}", path.display()),
            });
        }
    };

    // Defence in depth: refuse to read anything wildly larger than the
    // 16 KiB slot. A 32 KiB cap leaves room for future slot-size growth
    // while still keeping us out of pathological-file territory.
    let mut buf = Vec::with_capacity(FILE_SIZE);
    let cap = (FILE_SIZE * 2) as u64;
    let read_bytes = file
        .take(cap + 1)
        .read_to_end(&mut buf)
        .map_err(|e| NmblError::Io {
            source: e,
            context: format!("reading state.bin at {}", path.display()),
        })?;
    if read_bytes as u64 > cap {
        return Err(NmblError::Io {
            source: std::io::Error::new(
                ErrorKind::InvalidData,
                "state.bin larger than the 32 KiB sanity cap",
            ),
            context: format!("reading state.bin at {}", path.display()),
        });
    }

    let decoded: State = ciborium::from_reader(&buf[..]).map_err(|e| NmblError::Io {
        source: std::io::Error::new(ErrorKind::InvalidData, e.to_string()),
        context: format!("decoding state.bin at {}", path.display()),
    })?;

    if decoded.state_format_version > KNOWN_VERSION {
        // Emit through the standard log channel so the warning makes
        // it into the journal once the booted system imports the early
        // log. Phase 6 will VM-verify the line shape end-to-end.
        nmbl_warn!(
            "state.bin format version {} newer than this binary supports ({}); falling back to non-stateful boot",
            decoded.state_format_version,
            KNOWN_VERSION
        );
        return Ok(None);
    }

    Ok(Some(decoded))
}

/// Encode `state`, pad to exactly `FILE_SIZE`, write+fsync to `path`.
///
/// Truncates and overwrites unconditionally — callers that need
/// read-modify-write semantics must read first. fsync is mandatory: if
/// the system crashes before the next boot, the partial write would
/// leave the file in an undecodable state.
pub fn write_padded(path: &Path, state: &State) -> Result<(), NmblError> {
    let mut buf: Vec<u8> = Vec::with_capacity(FILE_SIZE);
    ciborium::into_writer(state, &mut buf).map_err(|e| NmblError::Io {
        source: std::io::Error::other(e.to_string()),
        context: format!("encoding state.bin for {}", path.display()),
    })?;

    // Leave at least one byte of padding so the trailing-zero terminator
    // is unambiguous when humans inspect the file.
    if buf.len() > FILE_SIZE - 1 {
        return Err(NmblError::StateTooLarge {
            encoded_len: buf.len(),
            max: FILE_SIZE,
        });
    }

    buf.resize(FILE_SIZE, 0);

    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(|e| NmblError::Io {
            source: e,
            context: format!("opening state.bin for write at {}", path.display()),
        })?;
    f.write_all(&buf).map_err(|e| NmblError::Io {
        source: e,
        context: format!("writing state.bin at {}", path.display()),
    })?;
    f.flush().map_err(|e| NmblError::Io {
        source: e,
        context: format!("flushing state.bin at {}", path.display()),
    })?;
    // rustix's fsync wrapper takes any AsFd; no `unsafe` required.
    rustix::fs::fsync(&f).map_err(|e| NmblError::Io {
        source: std::io::Error::from(e),
        context: format!("fsync state.bin at {}", path.display()),
    })?;

    Ok(())
}

/// Installer entry point. Ensures `dir` exists and contains a valid
/// `state.bin`. If the file is already there, it's parsed and the
/// canonical re-encoding is byte-compared against the on-disk content;
/// any divergence is reported as `StateRoundtripMismatch` so the
/// installer refuses to silently rewrite drifted state.
///
/// If `read` returns `Ok(None)` because the on-disk version is *newer*
/// than this binary supports, that's a fatal condition here: the
/// installer must not clobber a state file written by a future NMBL.
pub fn init_or_validate(dir: &Path) -> Result<State, NmblError> {
    // EEXIST on vfat is harmless — `create_dir_all` already swallows it.
    std::fs::create_dir_all(dir).map_err(|e| NmblError::Io {
        source: e,
        context: format!("creating state dir {}", dir.display()),
    })?;

    let path = dir.join("state.bin");

    if path.exists() {
        match read(&path)? {
            Some(state) => {
                // Re-encode and pad through the same code path
                // `write_padded` would use, then compare. Mismatch =
                // either schema drift on disk or a non-canonical
                // encoder; either way we refuse to overwrite.
                let mut reencoded: Vec<u8> = Vec::with_capacity(FILE_SIZE);
                ciborium::into_writer(&state, &mut reencoded).map_err(|e| NmblError::Io {
                    source: std::io::Error::other(e.to_string()),
                    context: format!("re-encoding state.bin at {}", path.display()),
                })?;
                if reencoded.len() > FILE_SIZE - 1 {
                    return Err(NmblError::StateTooLarge {
                        encoded_len: reencoded.len(),
                        max: FILE_SIZE,
                    });
                }
                reencoded.resize(FILE_SIZE, 0);

                let mut on_disk: Vec<u8> = Vec::with_capacity(FILE_SIZE);
                let cap = (FILE_SIZE * 2) as u64;
                let n = OpenOptions::new()
                    .read(true)
                    .open(&path)
                    .map_err(|e| NmblError::Io {
                        source: e,
                        context: format!("re-reading state.bin at {}", path.display()),
                    })?
                    .take(cap + 1)
                    .read_to_end(&mut on_disk)
                    .map_err(|e| NmblError::Io {
                        source: e,
                        context: format!("re-reading state.bin at {}", path.display()),
                    })?;
                if n as u64 > cap {
                    return Err(NmblError::StateRoundtripMismatch { path: path.clone() });
                }
                if on_disk != reencoded {
                    return Err(NmblError::StateRoundtripMismatch { path: path.clone() });
                }
                Ok(state)
            }
            None => {
                // `read` returned `None` either because the file
                // disappeared between `exists` and the open (race) or
                // because the on-disk version is newer than us. Either
                // way the installer must NOT clobber it.
                Err(NmblError::StateRoundtripMismatch { path })
            }
        }
    } else {
        let state = State::default();
        write_padded(&path, &state)?;
        // Round-trip verify: encode + fsync then decode the bytes we
        // just wrote, so we catch a broken codec at install time
        // rather than on the next boot.
        match read(&path)? {
            Some(reread) if reread == state => Ok(state),
            _ => Err(NmblError::StateRoundtripMismatch { path }),
        }
    }
}

/// Subcommand entry point for `--boot-succeeded`. Sets
/// `last_boot_succeeded = true` and zeros `recovery_attempt`, leaving
/// the on-disk format version untouched.
///
/// Absent or unsupported state.bin is a no-op so the booted system
/// never panics when it's running on a non-stateful image.
pub fn mark_boot_succeeded(dir: &Path) -> Result<(), NmblError> {
    let path = dir.join("state.bin");
    let mut state = match read(&path)? {
        Some(s) => s,
        None => {
            nmbl_info!(
                "nmbl state file at {} absent or unsupported; --boot-succeeded is a no-op",
                path.display()
            );
            return Ok(());
        }
    };
    state.last_boot_succeeded = true;
    state.recovery_attempt = 0;
    // state.state_format_version is deliberately NOT touched — see the
    // forward-compat contract on the struct definition.
    write_padded(&path, &state)
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

/// Run the rollback decision for the current boot.
///
/// `decide` is called ONCE per boot, BEFORE the kexec dispatch, with:
/// - `state`: the on-disk `state.bin` decoded into memory. Mutated in
///   place per the rules below; the caller writes it back to disk on
///   the non-Exhausted branches.
/// - `generations`: the result of `scan_generations`, sorted newest-first.
/// - `active_index`: the index inside `generations` of the currently
///   active Nix system profile, as returned by `active_generation_index`.
/// - `max_recovery_attempts`: operator-configured rollback budget.
///
/// `decide` does NOT touch `last_attempted_generation` or
/// `last_boot_succeeded` — that bookkeeping belongs to the caller, which
/// records the chosen generation and clears the success flag immediately
/// before kexec. Keeping those writes outside `decide` makes the
/// rotation/reset semantics independent of where the resulting boot ends
/// up going.
#[allow(
    clippy::indexing_slicing,
    reason = "ring slots are statically indexed within their fixed-size array; \
              the fallback `generations[idx]` is bounded by the `0..len()` range"
)]
pub fn decide(
    state: &mut State,
    generations: &[crate::generations::Generation],
    active_index: usize,
    max_recovery_attempts: u32,
) -> StatefulDecision {
    if state.last_boot_succeeded {
        // Rotation: bring the just-succeeded generation to the front of
        // known_good, then reset the rollback counter.
        if let Some(last) = state.last_attempted_generation {
            let n = last.get();
            // Only rotate if the gen is still on disk — a GC'd target
            // is treated as if we never attempted it. The array stays
            // a snapshot of generations actually available right now.
            if generations.iter().any(|g| g.number == n) {
                let existing = state
                    .known_good_generations
                    .iter()
                    .position(|slot| slot.map(|v| v.get()) == Some(n));
                match existing {
                    None => {
                        // Shift right by one, drop the tail, insert at [0].
                        let len = state.known_good_generations.len();
                        for i in (1..len).rev() {
                            state.known_good_generations[i] = state.known_good_generations[i - 1];
                        }
                        state.known_good_generations[0] = Some(last);
                    }
                    Some(pos) => {
                        // Only re-promote to the front when the
                        // succeeded gen is the one Nix considers active
                        // — otherwise the operator just rolled forward
                        // past a known-good and the ring already
                        // captured that boot at the right place.
                        let active_n = generations.get(active_index).map(|g| g.number);
                        if Some(n) == active_n && pos > 0 {
                            let slot = state.known_good_generations[pos];
                            for i in (1..=pos).rev() {
                                state.known_good_generations[i] =
                                    state.known_good_generations[i - 1];
                            }
                            state.known_good_generations[0] = slot;
                        }
                    }
                }
            }
        }
        state.recovery_attempt = 0;
        return StatefulDecision::HonourTui;
    }

    // First boot with a fresh state.bin: no prior attempt was recorded,
    // so there is nothing to roll back from. Honour the TUI/timeout pick
    // rather than spending a recovery slot before any failure happens.
    // Belt-and-braces with the `Default::default()` change that sets
    // `last_boot_succeeded = true`; this also covers hand-rolled or
    // version-skewed States that arrive with both fields cleared.
    if state.last_attempted_generation.is_none() {
        return StatefulDecision::HonourTui;
    }

    // Failure path. Budget check first — never mutate state if we're
    // already over budget, the caller may decide to skip the write.
    if state.recovery_attempt >= max_recovery_attempts {
        return StatefulDecision::Exhausted;
    }

    // First pick: try `known_good_generations[recovery_attempt]` if it
    // points at a generation that's still on disk.
    let r = state.recovery_attempt as usize;
    let mut picked: Option<usize> = None;
    if r < state.known_good_generations.len()
        && let Some(slot) = state.known_good_generations[r]
    {
        let n = slot.get();
        picked = generations.iter().position(|g| g.number == n);
    }

    if picked.is_none() {
        // Fallback walk: strictly OLDER than the active Nix profile.
        // `scan_generations` sorts newest-first (descending number), so
        // OLDER entries sit at HIGHER indices than `active_index`. Skip
        // anything already in known_good or the gen we tried most
        // recently (last_attempted_generation tracks the previous boot's
        // pick — preventing an immediate retry of the just-failed gen).
        // When `active_index` is already the oldest scanned generation,
        // the loop body never executes and we exhaust below.
        let last_attempt = state.last_attempted_generation.map(|v| v.get());
        picked = generations
            .iter()
            .enumerate()
            .skip(active_index + 1)
            .find_map(|(idx, g)| {
                let n = g.number;
                let in_known_good = state
                    .known_good_generations
                    .iter()
                    .any(|slot| slot.map(|v| v.get()) == Some(n));
                let is_last_attempt = last_attempt == Some(n);
                (!in_known_good && !is_last_attempt).then_some(idx)
            });
    }

    match picked {
        Some(idx) => {
            state.recovery_attempt = state.recovery_attempt.saturating_add(1);
            StatefulDecision::ForcePick(idx)
        }
        None => StatefulDecision::Exhausted,
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::field_reassign_with_default,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use tempfile::tempdir;

    #[test]
    fn roundtrip_with_all_known_good_slots_filled() {
        let mut state = State::default();
        for (i, slot) in state.known_good_generations.iter_mut().enumerate() {
            // Skip 0 → start at 1 so each slot has a distinct non-zero
            // value; NonMaxU32::new(u32::MAX) returns None.
            *slot = NonMaxU32::new((i as u32) + 1);
        }
        state.last_attempted_generation = NonMaxU32::new(42);
        state.last_boot_succeeded = true;
        state.recovery_attempt = 3;

        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("state.bin");
        write_padded(&path, &state).expect("write_padded");
        let reread = read(&path).expect("read").expect("Some(State)");
        assert_eq!(reread, state);
    }

    #[test]
    fn forward_compat_v1_decoded_by_hypothetical_v2_fills_default() {
        // Pretend a future "v2" added a `future_field`. Encode v1,
        // decode through the v2 struct — `future_field` must come back
        // as 0 (the serde default), proving newer binaries can read
        // older files.
        #[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
        struct V2 {
            state_format_version: u32,
            #[serde(default)]
            last_attempted_generation: Option<NonMaxU32>,
            #[serde(default)]
            last_boot_succeeded: bool,
            #[serde(default)]
            recovery_attempt: u32,
            #[serde(default = "super::default_known_good")]
            known_good_generations: [Option<NonMaxU32>; 20],
            #[serde(default)]
            future_field: u64,
        }

        let v1 = State::default();
        let mut buf: Vec<u8> = Vec::new();
        ciborium::into_writer(&v1, &mut buf).expect("encode v1");
        let decoded: V2 = ciborium::from_reader(&buf[..]).expect("decode v2");
        assert_eq!(decoded.future_field, 0);
        assert_eq!(decoded.state_format_version, v1.state_format_version);
    }

    #[test]
    fn forward_compat_v2_decoded_by_v1_skips_unknown_field() {
        // The OPPOSITE direction: a v2 writer emits an extra field,
        // and a v1 reader (= production `State`) silently ignores it.
        // This is the property that's only true because `State` does
        // NOT carry `#[serde(deny_unknown_fields)]`.
        #[derive(Debug, Serialize, Deserialize)]
        struct V2 {
            state_format_version: u32,
            last_attempted_generation: Option<NonMaxU32>,
            last_boot_succeeded: bool,
            recovery_attempt: u32,
            known_good_generations: [Option<NonMaxU32>; 20],
            future_field: u64,
        }

        let v2 = V2 {
            state_format_version: 1,
            last_attempted_generation: NonMaxU32::new(7),
            last_boot_succeeded: true,
            recovery_attempt: 1,
            known_good_generations: [None; 20],
            future_field: 0xdead_beef,
        };
        let mut buf: Vec<u8> = Vec::new();
        ciborium::into_writer(&v2, &mut buf).expect("encode v2");
        let decoded: State = ciborium::from_reader(&buf[..]).expect("decode v2 into v1");
        assert_eq!(decoded.state_format_version, 1);
        assert_eq!(decoded.recovery_attempt, 1);
        assert!(decoded.last_boot_succeeded);
    }

    #[test]
    fn write_padded_produces_exactly_16k_for_empty_state() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("state.bin");
        write_padded(&path, &State::default()).expect("write_padded");
        let meta = std::fs::metadata(&path).expect("metadata");
        assert_eq!(meta.len() as usize, FILE_SIZE);
    }

    #[test]
    fn write_padded_produces_exactly_16k_for_full_state() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("state.bin");
        let mut state = State::default();
        for (i, slot) in state.known_good_generations.iter_mut().enumerate() {
            *slot = NonMaxU32::new((i as u32) + 100);
        }
        state.last_attempted_generation = NonMaxU32::new(1_000);
        state.last_boot_succeeded = true;
        state.recovery_attempt = u32::MAX - 1;
        write_padded(&path, &state).expect("write_padded");
        let meta = std::fs::metadata(&path).expect("metadata");
        assert_eq!(meta.len() as usize, FILE_SIZE);
    }

    #[test]
    fn read_too_new_version_returns_none() {
        // A future v999 file. The current binary must log a warning
        // (verified end-to-end by Phase 6 VM tests; here we just pin
        // the `Ok(None)` graceful-fallback contract) and refuse to
        // touch it.
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("state.bin");
        let future = State {
            state_format_version: KNOWN_VERSION + 1000,
            ..State::default()
        };
        let mut buf: Vec<u8> = Vec::new();
        ciborium::into_writer(&future, &mut buf).expect("encode future");
        buf.resize(FILE_SIZE, 0);
        std::fs::write(&path, &buf).expect("write");

        assert!(read(&path).expect("read").is_none());
    }

    #[test]
    fn init_or_validate_on_missing_dir_creates_state() {
        let dir = tempdir().expect("tempdir");
        let nested = dir.path().join("nested").join("state-dir");
        assert!(!nested.exists());
        let state = init_or_validate(&nested).expect("init_or_validate");
        assert_eq!(state, State::default());
        assert!(nested.join("state.bin").exists());
        let meta = std::fs::metadata(nested.join("state.bin")).expect("metadata");
        assert_eq!(meta.len() as usize, FILE_SIZE);
    }

    #[test]
    fn init_or_validate_on_existing_canonical_state_succeeds() {
        // Round-trip case: a previously-written canonical state must
        // re-validate without error.
        let dir = tempdir().expect("tempdir");
        let _first = init_or_validate(dir.path()).expect("first init");
        let second = init_or_validate(dir.path()).expect("second init");
        assert_eq!(second, State::default());
    }

    #[test]
    fn mark_boot_succeeded_sets_flag_and_resets_recovery() {
        let dir = tempdir().expect("tempdir");
        // Seed with a state that has the "boot failed" shape.
        let seed = State {
            last_boot_succeeded: false,
            recovery_attempt: 4,
            last_attempted_generation: NonMaxU32::new(17),
            ..State::default()
        };
        write_padded(&dir.path().join("state.bin"), &seed).expect("seed write");

        mark_boot_succeeded(dir.path()).expect("mark_boot_succeeded");

        let after = read(&dir.path().join("state.bin"))
            .expect("read")
            .expect("Some");
        assert!(after.last_boot_succeeded);
        assert_eq!(after.recovery_attempt, 0);
        // Version preserved — must NEVER auto-bump.
        assert_eq!(after.state_format_version, KNOWN_VERSION);
        // Other fields untouched.
        assert_eq!(after.last_attempted_generation, NonMaxU32::new(17));
    }

    #[test]
    fn mark_boot_succeeded_on_missing_file_is_noop() {
        let dir = tempdir().expect("tempdir");
        // No state.bin written. Must return Ok(()) without panicking.
        mark_boot_succeeded(dir.path()).expect("noop on missing");
        assert!(!dir.path().join("state.bin").exists());
    }

    #[test]
    fn mark_boot_succeeded_on_too_new_version_is_noop() {
        // A too-new file must not be rewritten.
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("state.bin");
        let future = State {
            state_format_version: KNOWN_VERSION + 1,
            ..State::default()
        };
        let mut buf: Vec<u8> = Vec::new();
        ciborium::into_writer(&future, &mut buf).expect("encode");
        buf.resize(FILE_SIZE, 0);
        std::fs::write(&path, &buf).expect("write seed");
        let before = std::fs::read(&path).expect("read seed");

        mark_boot_succeeded(dir.path()).expect("noop on too-new");

        let after = std::fs::read(&path).expect("read after");
        assert_eq!(after, before);
    }

    // -- decide() unit tests ------------------------------------------------

    use crate::generations::Generation;
    use std::path::PathBuf;

    /// Build a synthetic [`Generation`] with just the `number` field
    /// meaningful — `decide` only looks at `.number`, the rest is filler
    /// the test never inspects.
    fn fake_gen(n: u32) -> Generation {
        Generation {
            number: n,
            profile_link: PathBuf::from(format!("/profile-{n}")),
            kernel: PathBuf::from("/kernel"),
            initrd: PathBuf::from("/initrd"),
            init_path: PathBuf::from("/init"),
            kernel_params: Vec::new(),
            label: String::new(),
        }
    }

    /// Convenience: build generations newest-first matching the order
    /// `scan_generations` produces. Pass numbers in any order; this
    /// re-sorts descending so callers can write `[10, 7, 3]` literally.
    fn gens(numbers: &[u32]) -> Vec<Generation> {
        let mut v: Vec<Generation> = numbers.iter().map(|n| fake_gen(*n)).collect();
        v.sort_by_key(|g| std::cmp::Reverse(g.number));
        v
    }

    fn nm(n: u32) -> Option<NonMaxU32> {
        NonMaxU32::new(n)
    }

    #[test]
    fn decide_success_inserts_new_into_known_good_front() {
        let mut state = State::default();
        state.last_boot_succeeded = true;
        state.last_attempted_generation = nm(42);
        state.known_good_generations[0] = nm(7);
        state.known_good_generations[1] = nm(5);
        state.recovery_attempt = 3;
        let gs = gens(&[42, 7, 5]);

        let d = decide(&mut state, &gs, 0, 3);
        assert_eq!(d, StatefulDecision::HonourTui);
        assert_eq!(state.known_good_generations[0], nm(42));
        assert_eq!(state.known_good_generations[1], nm(7));
        assert_eq!(state.known_good_generations[2], nm(5));
        // Tail drop: the previously-last slot is gone (was None anyway).
        assert_eq!(state.known_good_generations[19], None);
        assert_eq!(state.recovery_attempt, 0);
    }

    #[test]
    fn decide_success_inserts_drops_tail_when_full() {
        let mut state = State::default();
        state.last_boot_succeeded = true;
        state.last_attempted_generation = nm(100);
        for (i, slot) in state.known_good_generations.iter_mut().enumerate() {
            *slot = nm((i as u32) + 1);
        }
        // Active is gen 100 — present in generations; not in known_good.
        let mut numbers: Vec<u32> = (1..=20).collect();
        numbers.push(100);
        let gs = gens(&numbers);
        let active_index = gs.iter().position(|g| g.number == 100).expect("active");

        let _ = decide(&mut state, &gs, active_index, 3);
        assert_eq!(state.known_good_generations[0], nm(100));
        assert_eq!(state.known_good_generations[1], nm(1));
        // The original tail (slot 19 == 20) was dropped to make room.
        assert_eq!(state.known_good_generations[19], nm(19));
    }

    #[test]
    fn decide_success_promotes_when_in_known_good_and_active() {
        let mut state = State::default();
        state.last_boot_succeeded = true;
        // The succeeded gen is the active one AND already in known_good
        // at index 2 — promote it to the front, shifting [0] and [1]
        // down by one.
        state.last_attempted_generation = nm(50);
        state.known_good_generations[0] = nm(99);
        state.known_good_generations[1] = nm(80);
        state.known_good_generations[2] = nm(50);
        state.recovery_attempt = 2;
        let gs = gens(&[99, 80, 50, 10]);
        let active_index = gs.iter().position(|g| g.number == 50).expect("active");

        let d = decide(&mut state, &gs, active_index, 3);
        assert_eq!(d, StatefulDecision::HonourTui);
        assert_eq!(state.known_good_generations[0], nm(50));
        assert_eq!(state.known_good_generations[1], nm(99));
        assert_eq!(state.known_good_generations[2], nm(80));
        assert_eq!(state.recovery_attempt, 0);
    }

    #[test]
    fn decide_success_in_known_good_but_not_active_is_noop_on_array() {
        let mut state = State::default();
        state.last_boot_succeeded = true;
        state.last_attempted_generation = nm(50);
        state.known_good_generations[0] = nm(99);
        state.known_good_generations[2] = nm(50);
        state.recovery_attempt = 1;
        // Active is 99, not 50 — leave the array untouched.
        let gs = gens(&[99, 80, 50, 10]);
        let active_index = gs.iter().position(|g| g.number == 99).expect("active");
        let snapshot = state.known_good_generations;

        let d = decide(&mut state, &gs, active_index, 3);
        assert_eq!(d, StatefulDecision::HonourTui);
        assert_eq!(state.known_good_generations, snapshot);
        assert_eq!(state.recovery_attempt, 0);
    }

    #[test]
    fn decide_success_last_attempt_gc_is_noop_on_array() {
        let mut state = State::default();
        state.last_boot_succeeded = true;
        // 999 was attempted but is no longer on disk — array stays.
        state.last_attempted_generation = nm(999);
        state.known_good_generations[0] = nm(42);
        state.recovery_attempt = 1;
        let gs = gens(&[42, 7]);
        let snapshot = state.known_good_generations;

        let d = decide(&mut state, &gs, 0, 3);
        assert_eq!(d, StatefulDecision::HonourTui);
        assert_eq!(state.known_good_generations, snapshot);
        assert_eq!(state.recovery_attempt, 0);
    }

    #[test]
    fn decide_success_last_attempt_none_is_noop_on_array() {
        let mut state = State::default();
        state.last_boot_succeeded = true;
        state.last_attempted_generation = None;
        state.known_good_generations[0] = nm(42);
        state.recovery_attempt = 5;
        let gs = gens(&[42, 7]);
        let snapshot = state.known_good_generations;

        let d = decide(&mut state, &gs, 0, 3);
        assert_eq!(d, StatefulDecision::HonourTui);
        assert_eq!(state.known_good_generations, snapshot);
        assert_eq!(state.recovery_attempt, 0);
    }

    #[test]
    fn decide_failure_first_pick_from_known_good_slot_zero() {
        let mut state = State::default();
        state.last_boot_succeeded = false;
        // A prior attempt must be recorded to exercise the failure path
        // — without it the fresh-state guard short-circuits to HonourTui.
        state.last_attempted_generation = nm(100);
        state.recovery_attempt = 0;
        state.known_good_generations[0] = nm(50);
        let gs = gens(&[100, 50, 10]);
        let target = gs.iter().position(|g| g.number == 50).expect("target");

        let d = decide(&mut state, &gs, 0, 3);
        assert_eq!(d, StatefulDecision::ForcePick(target));
        assert_eq!(state.recovery_attempt, 1);
    }

    #[test]
    fn decide_failure_first_pick_missing_falls_back_to_older() {
        let mut state = State::default();
        state.last_boot_succeeded = false;
        // A prior attempt must be recorded to bypass the fresh-state
        // guard; pin it at 100 (the active gen) so the fallback walk
        // still finds 50 below it.
        state.last_attempted_generation = nm(100);
        state.recovery_attempt = 0;
        // Slot 0 is empty — must walk strictly OLDER gens. Active is
        // the newest (idx 0); fallback picks the next-older (idx 1).
        state.known_good_generations[0] = None;
        let gs = gens(&[100, 50, 10]);
        let d = decide(&mut state, &gs, 0, 3);
        assert_eq!(d, StatefulDecision::ForcePick(1));
        assert_eq!(state.recovery_attempt, 1);
    }

    #[test]
    fn decide_failure_known_good_gc_falls_back_to_older() {
        let mut state = State::default();
        state.last_boot_succeeded = false;
        // A prior attempt must be recorded to bypass the fresh-state
        // guard — pin it at the active gen so the fallback still hits.
        state.last_attempted_generation = nm(100);
        state.recovery_attempt = 0;
        // Points at gen 999 which is no longer on disk — first pick
        // misses, fallback walks older gens past `active_index`.
        state.known_good_generations[0] = nm(999);
        let gs = gens(&[100, 50, 10]);
        // Active = 100 (idx 0); fallback picks the next-older (idx 1 = 50).
        let d = decide(&mut state, &gs, 0, 3);
        assert_eq!(d, StatefulDecision::ForcePick(1));
        assert_eq!(state.recovery_attempt, 1);
    }

    #[test]
    fn decide_failure_fallback_skips_known_good_entries() {
        let mut state = State::default();
        state.last_boot_succeeded = false;
        // A prior attempt must be recorded to bypass the fresh-state guard.
        state.last_attempted_generation = nm(100);
        state.recovery_attempt = 0;
        // First pick miss — slot 0 absent.
        state.known_good_generations[0] = None;
        // But 50 IS marked known-good elsewhere in the ring — the
        // fallback walk must skip it and prefer 10 (the next older
        // entry strictly past active).
        state.known_good_generations[5] = nm(50);
        let gs = gens(&[100, 50, 10]);
        let active_index = gs.iter().position(|g| g.number == 100).expect("active");

        let d = decide(&mut state, &gs, active_index, 3);
        let target = gs.iter().position(|g| g.number == 10).expect("target");
        assert_eq!(d, StatefulDecision::ForcePick(target));
        assert_eq!(state.recovery_attempt, 1);
    }

    #[test]
    fn decide_failure_fallback_skips_last_attempted_generation() {
        let mut state = State::default();
        state.last_boot_succeeded = false;
        state.recovery_attempt = 0;
        state.known_good_generations[0] = None;
        // We just tried 50 last boot — fallback must skip it and pick
        // 10 (the next strictly-older candidate).
        state.last_attempted_generation = nm(50);
        let gs = gens(&[100, 50, 10]);
        let active_index = gs.iter().position(|g| g.number == 100).expect("active");

        let d = decide(&mut state, &gs, active_index, 3);
        let target = gs.iter().position(|g| g.number == 10).expect("target");
        assert_eq!(d, StatefulDecision::ForcePick(target));
        assert_eq!(state.recovery_attempt, 1);
    }

    #[test]
    fn decide_failure_exhausted_does_not_mutate_state() {
        let mut state = State::default();
        state.last_boot_succeeded = false;
        state.recovery_attempt = 3;
        state.last_attempted_generation = nm(42);
        state.known_good_generations[0] = nm(50);
        let gs = gens(&[100, 50, 10]);
        let snapshot = state.clone();

        let d = decide(&mut state, &gs, 0, 3);
        assert_eq!(d, StatefulDecision::Exhausted);
        assert_eq!(state, snapshot);
    }

    #[test]
    fn decide_failure_no_candidate_returns_exhausted() {
        let mut state = State::default();
        state.last_boot_succeeded = false;
        // A prior attempt must be recorded — otherwise the fresh-state
        // guard short-circuits before the budget check.
        state.last_attempted_generation = nm(100);
        state.recovery_attempt = 0;
        state.known_good_generations[0] = None;
        // active_index = 0 means no strictly older candidate; first
        // pick also misses — exhausted.
        let gs = gens(&[100]);
        let d = decide(&mut state, &gs, 0, 3);
        assert_eq!(d, StatefulDecision::Exhausted);
        // No mutation on the exhausted branch.
        assert_eq!(state.recovery_attempt, 0);
    }

    #[test]
    fn decide_failure_empty_generations_returns_exhausted() {
        let mut state = State::default();
        state.last_boot_succeeded = false;
        // A prior attempt must be recorded to bypass the fresh-state guard.
        state.last_attempted_generation = nm(50);
        state.recovery_attempt = 0;
        state.known_good_generations[0] = nm(50);
        let gs: Vec<Generation> = Vec::new();
        let d = decide(&mut state, &gs, 0, 3);
        assert_eq!(d, StatefulDecision::Exhausted);
        assert_eq!(state.recovery_attempt, 0);
    }

    #[test]
    fn decide_failure_active_index_zero_does_not_pick_active() {
        // `generations` is sorted newest-first by `scan_generations`.
        // Active at index 0 (the newest) must still leave room for the
        // fallback to pick an older entry — and it must NOT return
        // index 0 itself. This pins the "older = higher index"
        // convention against accidental sign flips in the walk.
        let mut state = State::default();
        state.last_boot_succeeded = false;
        // A prior attempt must be recorded to bypass the fresh-state guard.
        state.last_attempted_generation = nm(100);
        state.recovery_attempt = 0;
        state.known_good_generations[0] = None;
        let gs = gens(&[100, 50]);
        let d = decide(&mut state, &gs, 0, 3);
        assert_eq!(d, StatefulDecision::ForcePick(1));
        assert_ne!(d, StatefulDecision::ForcePick(0));
    }

    #[test]
    fn decide_failure_active_is_oldest_returns_exhausted() {
        // Active sits at the OLDEST slot (last index). No
        // strictly-older candidate exists, first pick missed, so the
        // result is Exhausted with no mutation.
        let mut state = State::default();
        state.last_boot_succeeded = false;
        // A prior attempt must be recorded to bypass the fresh-state guard.
        state.last_attempted_generation = nm(10);
        state.recovery_attempt = 0;
        state.known_good_generations[0] = None;
        let gs = gens(&[100, 50, 10]);
        let active_index = gs.iter().position(|g| g.number == 10).expect("active");

        let d = decide(&mut state, &gs, active_index, 3);
        assert_eq!(d, StatefulDecision::Exhausted);
        assert_eq!(state.recovery_attempt, 0);
    }

    #[test]
    fn decide_first_boot_with_fresh_state_honours_tui() {
        // Regression: a freshly-initialised state.bin
        // (last_attempted_generation = None) on a single-generation
        // install must HonourTui rather than dropping to Exhausted.
        // Pre-fix this case routed straight to the emergency screen.
        let mut state = State::default();
        // Pin the failure branch explicitly: even with
        // `last_boot_succeeded = false`, the absence of a prior attempt
        // means there is nothing to roll back from.
        state.last_boot_succeeded = false;
        state.last_attempted_generation = None;
        let gs = gens(&[42]);
        let active_index = 0;
        let max_attempts = 5;

        let decision = decide(&mut state, &gs, active_index, max_attempts);
        assert_eq!(decision, StatefulDecision::HonourTui);
        // The fresh-state guard must not spend a recovery slot.
        assert_eq!(state.recovery_attempt, 0);
    }

    #[test]
    fn decide_first_boot_default_state_honours_tui() {
        // The other half of the regression: `State::default()` now
        // initialises `last_boot_succeeded = true` (the "no failure
        // recorded yet" semantic). This pins that default so a future
        // refactor can't silently flip it back to `false` and resurrect
        // the emergency-screen loop.
        let mut state = State::default();
        let gs = gens(&[42]);
        let decision = decide(&mut state, &gs, 0, 5);
        assert_eq!(decision, StatefulDecision::HonourTui);
        assert!(state.last_boot_succeeded);
    }
}
