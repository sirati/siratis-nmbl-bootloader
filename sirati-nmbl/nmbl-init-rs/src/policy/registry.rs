//! The TPM-unsealed LUKS mapper registry (ALWAYS-COMPILED — FIX-09 /
//! FIX-03).
//!
//! Every `luks-tpm` activation that succeeds pushes its
//! `/dev/mapper/<name>` mapping onto this process-local registry via
//! [`register_tpm_mapper`]. [`crate::policy::seal_secrets`] drains the
//! registry and runs `cryptsetup close <name>` for each entry FIRST
//! (after the PCR cap) so no TPM-unsealed plaintext block device is left
//! live before any interactive context is reached.
//!
//! The registry is a `thread_local!{RefCell<Vec<MapperEntry>>}` (FIX-58:
//! `RefCell`, never a lock type) — the whole boot runs on the
//! single-threaded `LocalRuntime`, so there is no cross-thread access.
//!
//! ## Survival across the panic re-exec
//!
//! The panic hook `execve`s `/proc/self/exe` to resume init with a clean
//! stack — which WIPES this thread-local but leaves any open
//! device-mapper node alive in the kernel. A mapper opened before the
//! panic would therefore go un-closed by the resumed process's seal,
//! leaving readable plaintext under the post-panic emergency shell. To
//! close that hole, every registration ALSO appends the mapper to a
//! well-known tmpfs file ([`persist_path`]); the seal MERGES that file
//! back into the in-process registry before closing, and only deletes
//! the file once every listed mapper is confirmed closed.

use std::cell::RefCell;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Default on-disk registry file. `/run` is tmpfs (created in phase 1)
/// and survives the panic `execve`, so a mapper opened before a panic is
/// still listed for the resumed process's seal to close.
const DEFAULT_PERSIST_PATH: &str = "/run/nmbl/tpm-unsealed-mappers";

/// One TPM-unsealed LUKS mapper to close on seal. Carries the
/// `cryptsetup` binary the activation used so the close is fully
/// self-contained — the seal path never has to re-resolve it from the
/// config.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MapperEntry {
    /// The `cryptsetup` binary the activation forked (e.g.
    /// `/bin/cryptsetup`). Reused verbatim for the matching `close`.
    pub cryptsetup: PathBuf,
    /// The device-mapper name (the `<name>` in `/dev/mapper/<name>`),
    /// i.e. the argument passed to `cryptsetup close`.
    pub name: String,
}

thread_local! {
    /// TPM-unsealed mappers opened this boot, in open order. Drained by
    /// the seal path; never read by anything else.
    static TPM_MAPPERS: RefCell<Vec<MapperEntry>> = const { RefCell::new(Vec::new()) };

    /// On-disk registry file path. Overridable in tests so they can drive
    /// the re-exec survival path against a temp file rather than `/run`.
    static PERSIST_PATH: RefCell<PathBuf> =
        RefCell::new(PathBuf::from(DEFAULT_PERSIST_PATH));
}

/// Resolve the on-disk registry file path.
fn persist_path() -> PathBuf {
    PERSIST_PATH.with(|p| p.borrow().clone())
}

/// Record a successfully-opened TPM-unsealed LUKS mapper so the seal
/// path will close it (FIX-03). Called from the `luks-tpm` activation
/// success site. Idempotent on `name`: re-registering the same mapper
/// (e.g. a retried activation that exits 5 "already open") does not
/// duplicate the close. Also appends the mapper to the on-disk registry
/// so a panic re-exec (which wipes the thread-local but not the kernel
/// mapper) still has the entry to close.
pub fn register_tpm_mapper(entry: MapperEntry) {
    TPM_MAPPERS.with(|m| {
        let mut m = m.borrow_mut();
        if !m.iter().any(|e| e.name == entry.name) {
            persist_append(&entry);
            m.push(entry);
        }
    });
}

/// Append `entry` to the on-disk registry file (one `<cryptsetup>\t<name>`
/// line per mapper), creating the parent directory on demand. Best-effort:
/// a failure here only loses the re-exec survival guarantee for THIS
/// mapper, never the in-process close, so it is logged and swallowed
/// rather than failing the activation.
fn persist_append(entry: &MapperEntry) {
    let path = persist_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let line = format!("{}\t{}\n", entry.cryptsetup.display(), entry.name);
    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut f) => {
            let _ = f.write_all(line.as_bytes());
        }
        Err(e) => {
            crate::nmbl_warn!(
                "could not persist tpm-unsealed mapper {} to {}: {e}; \
                 a panic re-exec would not close it",
                entry.name,
                path.display()
            );
        }
    }
}

/// Parse the on-disk registry file into entries. Each non-empty line is
/// `<cryptsetup>\t<name>`; malformed lines are skipped. Missing file ⇒
/// empty (the common, no-panic case).
fn persist_load() -> Vec<MapperEntry> {
    let path = persist_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    text.lines().filter_map(parse_persist_line).collect()
}

/// Parse one `<cryptsetup>\t<name>` registry line; `None` for blanks /
/// malformed rows.
fn parse_persist_line(line: &str) -> Option<MapperEntry> {
    let (cryptsetup, name) = line.split_once('\t')?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    Some(MapperEntry {
        cryptsetup: PathBuf::from(cryptsetup),
        name: name.to_string(),
    })
}

/// Snapshot the currently-registered mappers WITHOUT clearing them,
/// MERGED (dedup on name) with any persisted on-disk entries from a prior
/// (pre-panic) process image. The seal path reads this to know what to
/// close; it clears entries only as each close confirms (see
/// [`mark_closed`]) so a partial seal that fails to close a mapper leaves
/// that mapper registered (in memory and on disk) and the seal `Err`
/// (fail-closed).
#[must_use]
pub fn snapshot() -> Vec<MapperEntry> {
    let mut merged = TPM_MAPPERS.with(|m| m.borrow().clone());
    for disk in persist_load() {
        if !merged.iter().any(|e| e.name == disk.name) {
            merged.push(disk);
        }
    }
    merged
}

/// Drop `name` from the in-memory registry AND the on-disk file once its
/// `cryptsetup close` has confirmed. Leaving still-open mappers
/// registered (in either place) is what makes a partial seal observably
/// fail.
pub fn mark_closed(name: &str) {
    TPM_MAPPERS.with(|m| m.borrow_mut().retain(|e| e.name != name));
    persist_remove(name);
}

/// Remove every line naming `name` from the on-disk registry file,
/// deleting the file entirely once it is empty. Best-effort: a write
/// failure leaves the line intact, which is the fail-closed direction
/// (the seal already gated on the close confirming).
fn persist_remove(name: &str) {
    let path = persist_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let kept: Vec<MapperEntry> = text
        .lines()
        .filter_map(parse_persist_line)
        .filter(|e| e.name != name)
        .collect();
    if kept.is_empty() {
        let _ = std::fs::remove_file(&path);
        return;
    }
    rewrite_persist(&path, &kept);
}

/// Overwrite the on-disk registry file with exactly `entries`.
fn rewrite_persist(path: &Path, entries: &[MapperEntry]) {
    let body: String = entries
        .iter()
        .map(|e| format!("{}\t{}\n", e.cryptsetup.display(), e.name))
        .collect();
    let _ = std::fs::write(path, body);
}

/// Number of mappers still awaiting close — the merged in-memory +
/// on-disk count (dedup on name). The seal succeeds only when this
/// reaches zero.
#[must_use]
pub fn pending() -> usize {
    snapshot().len()
}

/// Point the on-disk registry at `path`. Test-only — production uses
/// the fixed `/run` location.
#[cfg(test)]
pub fn set_persist_path(path: PathBuf) {
    PERSIST_PATH.with(|p| *p.borrow_mut() = path);
}

/// Clear the registry (in-memory AND on-disk). Test-only — production
/// code drains via [`mark_closed`] as each close confirms.
#[cfg(test)]
pub fn reset() {
    TPM_MAPPERS.with(|m| m.borrow_mut().clear());
    let _ = std::fs::remove_file(persist_path());
}
