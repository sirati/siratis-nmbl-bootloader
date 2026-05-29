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

/// Re-encode `state` and byte-compare against on-disk content; any
/// divergence returns `StateRoundtripMismatch`.
fn validate_existing(path: std::path::PathBuf, state: State) -> Result<State, NmblError> {
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
