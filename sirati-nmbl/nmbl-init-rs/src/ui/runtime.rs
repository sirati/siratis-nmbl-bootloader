//! Single-threaded async runtime wiring for the interactive TUI phase.
//!
//! NMBL runs on one OS thread (PID 1, fork-safe), so the whole TUI is
//! driven by a tokio **current-thread** [`LocalRuntime`]: every task
//! runs via `spawn_local`, no worker threads are ever spawned, and every
//! existing `fork()` site stays fork-safe. The synchronous orchestrator
//! (`main.rs`, `shell.rs`, the rescue dispatcher) crosses into the async
//! interactive phase through [`block_on_tui`].
//!
//! The reserve syscall poller (see [`crate::sys::poller`]) is
//! `spawn_local`'d onto the runtime at startup with a tokio-timer 1ms
//! pacer ([`crate::sys::poller::TokioPacer`]). It is the fallback
//! mechanism for syscalls with no async wrapper and has no consumer yet
//! (the waitpid consumer arrives in a later phase), but it is wired in
//! here so the runtime startup already matches the final shape.

use crate::error::{NmblError, Result};

/// Build the single-threaded tokio [`LocalRuntime`] that drives the
/// interactive TUI phase. Current-thread only — NMBL is one OS thread
/// (PID 1, fork-safe), so every task runs via `spawn_local` and no
/// worker threads are ever spawned. The poller driver (see
/// [`crate::sys::poller`]) is `spawn_local`'d onto this runtime at
/// startup with a tokio-timer pacer.
///
/// Returns a [`NmblError::Tui`] if the runtime cannot be constructed
/// (out of fds, etc.) so the caller can route to the emergency path
/// instead of unwinding through PID 1.
///
/// [`LocalRuntime`]: tokio::runtime::LocalRuntime
pub fn build_local_runtime() -> Result<tokio::runtime::LocalRuntime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build_local(tokio::runtime::LocalOptions::default())
        .map_err(|e| NmblError::Tui {
            source: std::io::Error::other(format!("tokio LocalRuntime build failed: {e}")),
        })
}

/// Spawn the reserve syscall poller (see [`crate::sys::poller`]) onto
/// the current `spawn_local` scope with a real tokio 1ms pacer, and
/// return its [`LocalSender`] so future syscall ops (the Phase 4
/// waitpid consumer) can enqueue work.
///
/// The poller is the fallback mechanism for syscalls with no async
/// wrapper; it has no consumer yet but is wired into the runtime here so
/// the pacing seam is live. Must be called from inside a
/// `LocalRuntime::block_on` (it uses `tokio::task::spawn_local`).
///
/// [`LocalSender`]: crate::sys::poller::LocalSender
pub fn spawn_poller() -> crate::sys::poller::LocalSender {
    let (poller, sender) = crate::sys::poller::build();
    tokio::task::spawn_local(poller.run_with(crate::sys::poller::TokioPacer));
    sender
}

/// Run an interactive TUI future to completion on a fresh single-thread
/// [`LocalRuntime`], with the reserve syscall poller `spawn_local`'d
/// first (so its tokio-timer pacing seam is live for the whole session).
///
/// This is the canonical entry the synchronous orchestrator uses to
/// cross into the async interactive phase: `block_on_tui(async { … })`.
/// The poller has no consumer yet (the waitpid consumer arrives later)
/// but is wired in here so the runtime startup matches the final shape.
///
/// Returns a [`NmblError::Tui`] if the runtime can't be built.
pub fn block_on_tui<F, T>(fut: F) -> Result<T>
where
    F: std::future::Future<Output = T>,
{
    let rt = build_local_runtime()?;
    Ok(rt.block_on(async move {
        // Wire the reserve poller onto this runtime before running the
        // session future. `spawn_local` is valid here because
        // `LocalRuntime::block_on` establishes a local task context.
        let _poller_sender = spawn_poller();
        fut.await
    }))
}
