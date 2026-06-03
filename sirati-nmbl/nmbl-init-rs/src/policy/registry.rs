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

use std::cell::RefCell;
use std::path::PathBuf;

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
}

/// Record a successfully-opened TPM-unsealed LUKS mapper so the seal
/// path will close it (FIX-03). Called from the `luks-tpm` activation
/// success site. Idempotent on `name`: re-registering the same mapper
/// (e.g. a retried activation that exits 5 "already open") does not
/// duplicate the close.
pub fn register_tpm_mapper(entry: MapperEntry) {
    TPM_MAPPERS.with(|m| {
        let mut m = m.borrow_mut();
        if !m.iter().any(|e| e.name == entry.name) {
            m.push(entry);
        }
    });
}

/// Snapshot the currently-registered mappers WITHOUT clearing them.
/// The seal path reads this to know what to close; it clears entries
/// only as each close confirms (see [`mark_closed`]) so a partial seal
/// that fails to close a mapper leaves that mapper registered and the
/// seal `Err` (fail-closed).
#[must_use]
pub fn snapshot() -> Vec<MapperEntry> {
    TPM_MAPPERS.with(|m| m.borrow().clone())
}

/// Drop `name` from the registry once its `cryptsetup close` has
/// confirmed. Leaving still-open mappers registered is what makes a
/// partial seal observably fail.
pub fn mark_closed(name: &str) {
    TPM_MAPPERS.with(|m| m.borrow_mut().retain(|e| e.name != name));
}

/// Number of mappers still awaiting close. The seal succeeds only when
/// this reaches zero.
#[must_use]
pub fn pending() -> usize {
    TPM_MAPPERS.with(|m| m.borrow().len())
}

/// Clear the registry. Test-only — production code drains via
/// [`mark_closed`] as each close confirms.
#[cfg(test)]
pub fn reset() {
    TPM_MAPPERS.with(|m| m.borrow_mut().clear());
}
