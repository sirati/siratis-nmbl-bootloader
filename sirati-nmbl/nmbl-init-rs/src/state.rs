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
        Self {
            state_format_version: KNOWN_VERSION,
            last_attempted_generation: None,
            last_boot_succeeded: false,
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

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
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
}
