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

mod decide;
mod io;
mod types;

pub use decide::decide;
pub use io::{init_or_validate, mark_boot_succeeded, read, write_padded};
pub use types::{State, StatefulDecision};

#[cfg(test)]
pub(crate) use types::{FILE_SIZE, KNOWN_VERSION, default_known_good};

#[cfg(test)]
mod tests;
