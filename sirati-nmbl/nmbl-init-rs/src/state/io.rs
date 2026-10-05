use std::fs::OpenOptions;
use std::io::{ErrorKind, Read, Write};
use std::path::Path;

use crate::error::NmblError;
use crate::{nmbl_info, nmbl_warn};

use super::types::{FILE_SIZE, KNOWN_VERSION, State};

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
/// original CBOR representation and typed round-trip are validated without
/// rewriting compatible older fields. Invalid encodings are reported as
/// `StateRoundtripMismatch`; installer validation never resets boot history.
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
            Some(state) => validate_existing(path, state),
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

/// Validate the original encoding without replacing a compatible older schema.
/// Serde defaults add fields in memory, so current-State bytes need not equal
/// older on-disk bytes. Preserve the file and compare its canonical CBOR value
/// and typed round-trip independently.
fn validate_existing(path: std::path::PathBuf, state: State) -> Result<State, NmblError> {
    // Check the current typed codec separately from the original wire schema.
    // This must preserve the remembered attempt, health and recovery history.
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
    let round_tripped: State =
        ciborium::from_reader(reencoded.as_slice()).map_err(|e| NmblError::Io {
            source: std::io::Error::other(e.to_string()),
            context: format!("round-tripping state.bin at {}", path.display()),
        })?;
    if round_tripped != state {
        return Err(NmblError::StateRoundtripMismatch { path });
    }

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
    // Re-reading must not accept a different state after the initial read.
    let reread: State = ciborium::from_reader(on_disk.as_slice()).map_err(|e| NmblError::Io {
        source: std::io::Error::other(e.to_string()),
        context: format!("validating original state.bin at {}", path.display()),
    })?;
    if reread != state {
        return Err(NmblError::StateRoundtripMismatch { path });
    }
    // Value retains older missing fields and compatible unknown fields. Its
    // canonical re-encoding checks the existing padding and representation,
    // without forcing the current struct's additional default fields to disk.
    let original: ciborium::Value =
        ciborium::from_reader(on_disk.as_slice()).map_err(|e| NmblError::Io {
            source: std::io::Error::other(e.to_string()),
            context: format!("validating state.bin encoding at {}", path.display()),
        })?;
    let mut canonical_existing = Vec::with_capacity(FILE_SIZE);
    ciborium::into_writer(&original, &mut canonical_existing).map_err(|e| NmblError::Io {
        source: std::io::Error::other(e.to_string()),
        context: format!("re-encoding original state.bin at {}", path.display()),
    })?;
    if canonical_existing.len() > FILE_SIZE - 1 {
        return Err(NmblError::StateTooLarge {
            encoded_len: canonical_existing.len(),
            max: FILE_SIZE,
        });
    }
    canonical_existing.resize(FILE_SIZE, 0);
    if on_disk != canonical_existing {
        return Err(NmblError::StateRoundtripMismatch { path });
    }
    Ok(state)
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
    state.rescue_booted_generation = None;
    state.rescue_exit_retry_in_progress = false;
    state.last_boot_succeeded = true;
    state.recovery_attempt = 0;
    // state.state_format_version is deliberately NOT touched — see the
    // forward-compat contract on the struct definition.
    write_padded(&path, &state)
}
