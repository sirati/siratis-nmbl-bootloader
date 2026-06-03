//! Lock-on-rescue SEAL policy (ALWAYS-COMPILED — FIX-09).
//!
//! This module is compiled in EVERY build, independent of the
//! `secure-boot` Cargo feature, because lock-on-rescue must work on a
//! plain `luks-tpm` box too: the moment NMBL is about to drop the
//! operator into ANY interactive context (an emergency shell, a Pretty
//! Shell, a remote-attach session, a rescue handoff, or the final
//! `execve` waist) it FIRST [`seal_secrets`]:
//!
//! 1. Caps (poisons) the lock PCR so any TPM-sealed secret can no longer
//!    be unsealed without a reboot/reset (fast + irreversible).
//! 2. Closes every TPM-unsealed LUKS mapper recorded in the
//!    [`registry`], so a still-live `/dev/mapper/<name>` node cannot be
//!    read from the shell the operator is about to get.
//!
//! Only after BOTH succeed does it hand back the unforgeable
//! [`Sealed`] witness. The fork/execve shell-spawn helpers REQUIRE a
//! `Sealed` by type, so a shell literally cannot be spawned without one
//! (re-audit C-1); the `nmbl-init-must-seal` flake check enforces that
//! every spawn site is preceded by a `seal_secrets()` / `Sealed`
//! witness or a `// seal-exempt:` justification.
//!
//! `seal_secrets` is exposed in two shapes: the async [`seal_secrets`]
//! (drives the mapper close through the interactive runtime's
//! fork/exec runner) for sites already inside the [`crate::sys::poller`]
//! runtime, and [`seal_secrets_blocking`] for the synchronous terminal
//! sites (`rescue::dispatch`, `run_force_rescue`, the `dispatch_execve`
//! backstop) that run after the runtime has unwound.

pub mod guard;
pub mod refuse_screen;
pub mod registry;
pub mod relock;
pub mod sentinel;

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert on contract failures"
)]
mod tests;

pub use guard::{SealFailed, Sealed, seal_secrets, seal_secrets_blocking};
pub use refuse_screen::run_refuse_screen;
pub use registry::{MapperEntry, register_tpm_mapper};
pub use relock::{
    RelockCommand, refuse_unsigned, refuse_unsigned_blocking, relock_and_refuse,
    relock_and_refuse_blocking, relock_argv,
};
pub use sentinel::{sentinel_present, should_force_rescue, write_sentinel};
