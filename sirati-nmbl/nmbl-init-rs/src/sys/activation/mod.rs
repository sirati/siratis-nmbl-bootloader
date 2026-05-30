//! Pure-mechanism execve runner for activation tools.
//!
//! This module is one of exactly three sites in the crate that are
//! allowed to replace the current process with another binary (the
//! other two being `src/shell.rs` and `src/panic.rs`). It exists so
//! the activation orchestrator can launch LVM/mdraid/cryptsetup/zpool
//! with a configured argv and — for the `luks-password` kind — a
//! passphrase piped to the child's stdin.
//!
//! It is deliberately stripped of policy: the caller decides which
//! binary to run, what arguments to pass, whether to feed bytes on
//! stdin, and whether a non-zero exit code is fatal. The runner only
//! reports *how* the child terminated.
//!
//! We use `nix`'s primitives directly rather than `std::process::Command`
//! because (a) we need fine-grained control of the stdin pipe and (b)
//! the CI grep enforces that `Command::` only ever appears in the
//! emergency-shell and panic-recovery paths.

use std::time::Duration;

mod helpers;
mod runner;

#[cfg(all(test, unix))]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests can panic on assertion failure; production lints are too strict for asserts"
)]
mod tests;

/// Poll cadence for the tick-aware wait helper.
///
/// 150 ms is brisk enough that the operator sees the spinner move
/// while a passphrase is being verified (cryptsetup --key-file=- runs
/// in well under a second on modern hardware, but Argon2id key
/// derivation can take ~1-3 s on a Raspberry Pi class CPU), and slow
/// enough that the WNOHANG polling overhead stays negligible.
const TICK_INTERVAL: Duration = Duration::from_millis(150);

/// How a child process terminated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessOutcome {
    /// `WEXITSTATUS` for normal exits, `128 + signal` for signalled
    /// exits (mirroring the shell convention so callers can log a
    /// single number).
    pub exit_code: i32,
    /// `true` if the child exited via `_exit`/`exit`, `false` if it
    /// was killed by a signal.
    pub normal_exit: bool,
}

/// Conventional shell exit code for "exec failed / command not found".
/// We surface this when the post-fork `execve(2)` returns an error so
/// the caller can distinguish a missing binary from a broken one.
const EXEC_FAILED_EXIT_CODE: i32 = 127;

pub use runner::{run, run_capture, run_capture_blocking, run_with_tick};
