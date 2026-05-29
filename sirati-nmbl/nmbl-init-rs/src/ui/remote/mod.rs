//! Remote-TUI server: drive independent recovery sessions on operator
//! ptys received over the root-only control socket.
//!
//! Gated behind the `remote-tui` feature. When NMBL enters recovery it
//! starts [`run_remote_server`] CONCURRENTLY with the local console's
//! emergency menu (see [`crate::shell::drop_to_emergency`]). A remote
//! root operator in the emergency shell runs `nmbl-init`, which detects
//! it is not PID 1, connects to the socket, and passes its controlling
//! terminal across via `SCM_RIGHTS`. PID 1 then drives a FRESH,
//! INDEPENDENT emergency menu on that pty — its own
//! [`SessionInteraction`] and its own [`TtyConsole`] — concurrently with
//! the local session and any other remote sessions, all on the single
//! `LocalRuntime` thread.
//!
//! ## Concurrency model (no `spawn_local`, no `tokio::sync`)
//!
//! NMBL is one OS thread and the recovery state (`config`, the boot
//! error) is borrowed, not `'static`, so the per-connection futures
//! cannot be `tokio::task::spawn_local`'d (that bound is `'static`).
//! Instead [`driver`] hand-rolls a cooperative multiplexer: `accept`
//! and every live session future are polled together each turn via
//! [`std::future::poll_fn`]. A session that is stuck awaiting its pty
//! simply stays `Pending` and never blocks `accept` — the same
//! starvation guarantee `spawn_local` would give, without the `'static`
//! requirement and without any worker thread (fork-safety preserved).
//!
//! ## Shutdown
//!
//! The shutdown signal is an [`Rc<Cell<bool>>`] ([`Shutdown`]) polled in
//! the accept multiplexer — NOT a `tokio::sync` primitive (banned). When
//! the local operator picks a terminal action (or a remote session
//! produces one) the recovery wiring flips the cell; the server unlinks
//! the socket and returns. In-flight sessions are dropped, which drops
//! their `UnixStream`/pty fds → the clients hit EOF and exit 0.
//!
//! ## Session end → client EOF → client exit 0
//!
//! [`serve_session`] owns the accepted `UnixStream` for the session's
//! lifetime. When the session ends (terminal choice, Ctrl+E, or a pty
//! read error meaning the client disconnected) the function returns and
//! the `UnixStream` + the [`TtyConsole`] (holding the pty `OwnedFd`)
//! drop, closing the server's ends. The client's blocking
//! `stream.read` then returns `Ok(0)` and `connect_and_serve` exits 0.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use tokio::net::UnixStream;

use crate::config::Config;
use crate::error::NmblError;
use crate::ipc::tui_socket::{
    RemoteHandle, TUI_SOCK_PATH, authenticate_and_receive, bind_listener,
};
use crate::nmbl_warn;
use crate::terminal::TerminalAction;
use crate::ui::app::{App, SessionInteraction};
use crate::ui::console::{Console, TtyConsole};
use crate::ui::{build_emergency_app, build_message, default_items, resolve_emergency_timeout};

mod driver;

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert on contract failures"
)]
mod tests;

/// Cooperative shutdown signal shared between the recovery wiring and the
/// remote accept loop. A plain `Rc<Cell<bool>>` (NOT a `tokio::sync`
/// channel — those are banned on the single-thread fork-safe runtime).
#[derive(Clone, Default)]
pub struct Shutdown(Rc<Cell<bool>>);

impl Shutdown {
    /// Create a fresh, un-signalled shutdown handle.
    #[must_use]
    pub fn new() -> Self {
        Self(Rc::new(Cell::new(false)))
    }

    /// Request shutdown. Idempotent.
    pub fn signal(&self) {
        self.0.set(true);
    }

    /// Whether shutdown has been requested.
    #[must_use]
    pub fn is_signalled(&self) -> bool {
        self.0.get()
    }
}

/// Slot a remote session writes its produced [`TerminalAction`] into.
/// Shared (`Rc<RefCell<…>>`) so the accept multiplexer can hand the same
/// sink to every session and the recovery wiring can read whichever
/// session committed first. `TerminalAction` is not `Copy`, so this is a
/// `RefCell`, not a `Cell`.
pub type ActionSink = Rc<RefCell<Option<TerminalAction>>>;

/// Own the root-only listener and serve remote recovery sessions until
/// `shutdown` is signalled.
///
/// On each accepted connection the peer is authenticated
/// ([`authenticate_and_receive`]: peercred-root gate + `SCM_RIGHTS` pty
/// fd + handshake). An authenticated peer's session is driven
/// concurrently with `accept` and every other session (see [`driver`]),
/// so one slow/stuck session can never starve `accept`. A rejected or
/// timed-out peer is dropped and the loop continues.
///
/// When a session commits a [`TerminalAction`] it is stored in `sink`
/// and `shutdown` is signalled so the whole recovery resolves to that
/// action. On return the socket path is unlinked.
pub async fn run_remote_server(config: &Config, shutdown: Shutdown, sink: ActionSink) {
    let listener = match bind_listener() {
        Ok(l) => l,
        Err(e) => {
            nmbl_warn!("remote-tui: bind_listener failed: {e}; remote attach disabled");
            return;
        }
    };
    // Unlink the socket on EVERY exit path — normal shutdown AND future
    // cancellation (the recovery wiring may `select!` this future against
    // the local menu and drop it when the local operator acts first). A
    // drop guard guarantees the stale socket never outlives the server.
    let _unlink = SocketUnlinkGuard;
    driver::accept_loop(&listener, config, &shutdown, &sink).await;
}

/// Unlinks the control socket on drop. Covers both the normal return of
/// [`run_remote_server`] and its cancellation when raced against the
/// local menu. ENOENT is fine (a stale-socket sweep may have removed it).
struct SocketUnlinkGuard;

impl Drop for SocketUnlinkGuard {
    fn drop(&mut self) {
        match rustix::fs::unlink(TUI_SOCK_PATH) {
            Ok(()) | Err(rustix::io::Errno::NOENT) => {}
            Err(e) => nmbl_warn!("remote-tui: failed to unlink {TUI_SOCK_PATH}: {e}"),
        }
    }
}

/// Authenticate one accepted connection and, on success, drive an
/// independent recovery session on the received pty. Owns `stream` for
/// the session's lifetime so dropping it on return EOFs the client.
async fn handle_connection(
    mut stream: UnixStream,
    config: &Config,
    shutdown: &Shutdown,
    sink: &ActionSink,
) {
    let handle = match authenticate_and_receive(&mut stream).await {
        Ok(Some(handle)) => handle,
        // Rejected (non-root) or timed-out peer: nothing to serve.
        Ok(None) => return,
        Err(e) => {
            nmbl_warn!("remote-tui: handshake failed: {e}");
            return;
        }
    };
    serve_session(stream, handle, config, shutdown, sink).await;
    // `stream` and the session's TtyConsole (owning the pty fd) drop
    // here → the client's blocking read hits EOF → client exits 0.
}

/// Drive one independent remote recovery session to completion.
///
/// Builds a [`TtyConsole`] on the received pty (`handle.pty`) seeded with
/// the client's winsize, a FRESH [`SessionInteraction`] (no shared
/// session state — each remote session is independent), and a fresh
/// emergency-menu [`App`]; then runs the SAME re-entrant emergency loop
/// the local console uses, via
/// [`crate::shell::dispatch_emergency_choice`]. The session ends when:
///   * the operator makes a terminal choice (Reboot, or a successful
///     Retry/Verify) — the produced [`TerminalAction`] is stored in
///     `sink` and `shutdown` is signalled;
///   * the operator presses Ctrl+E (`app.exit_session`) — the session
///     ends with no action;
///   * a render/poll error occurs (typically the client disconnected),
///     which also just ends the session.
///
/// `_stream` is held only to keep the socket open for the session's
/// lifetime; it is consumed by value so it drops on return.
async fn serve_session(
    _stream: UnixStream,
    handle: RemoteHandle,
    config: &Config,
    shutdown: &Shutdown,
    sink: &ActionSink,
) {
    let mut console = match TtyConsole::from_pty(handle.pty, handle.winsize) {
        Ok(c) => c,
        Err(e) => {
            nmbl_warn!("remote-tui: cannot build console on pty: {e}");
            return;
        }
    };

    // Each remote session is INDEPENDENT: its own interaction latch (so
    // the unattended-boot auto-reboot countdown logic is per-session) and
    // its own emergency App. We render the boot error the same way the
    // local menu does so the operator sees identical context.
    let session = SessionInteraction::new();
    let err = NmblError::Io {
        source: std::io::Error::other("remote recovery session"),
        context: "remote-tui".to_string(),
    };
    let message = build_message(&err);
    let items = default_items();
    let mut app: App<'static> = build_emergency_app(&message, &items, &session);
    let emergency_timeout = resolve_emergency_timeout(config);
    let mut error_count: u32 = 0;

    let action = run_remote_menu(
        &mut console,
        &mut app,
        config,
        &session,
        emergency_timeout,
        &mut error_count,
    )
    .await;

    if let Some(action) = action {
        shutdown.signal();
        // First committer wins: only fill the sink if still empty.
        let mut slot = sink.borrow_mut();
        if slot.is_none() {
            *slot = Some(action);
        }
    }
}

/// The per-session re-entrant emergency loop. Mirrors the local
/// [`crate::shell::drop_to_emergency`] loop but additionally honours
/// `app.exit_session` (Ctrl+E) to end the remote session. Returns
/// `Some(action)` on a terminal choice, `None` on Ctrl+E or a
/// render/poll error (client disconnected).
async fn run_remote_menu(
    console: &mut dyn Console,
    app: &mut App<'static>,
    config: &Config,
    session: &SessionInteraction,
    emergency_timeout: std::time::Duration,
    error_count: &mut u32,
) -> Option<TerminalAction> {
    loop {
        app.modal = None;
        // Use the low-level driver (not `run_emergency_screen_with_app`,
        // which swallows console errors into a `Reboot` choice). On a
        // remote pty a render/poll error means the client vanished — we
        // must NOT silently commit a machine-wide Reboot in that case.
        let choice = match crate::ui::emergency::loop_driver::drive_emergency_loop_exitable(
            app,
            emergency_timeout,
            std::time::Instant::now,
            &mut *console,
        )
        .await
        {
            Ok(c) => c,
            Err(e) => {
                nmbl_warn!("remote-tui: session ended (client likely gone): {e}");
                return None;
            }
        };

        // Ctrl+E asks to leave this remote session with no action.
        if app.exit_session {
            return None;
        }

        if let Some(action) = crate::shell::dispatch_emergency_choice(
            choice,
            &mut *console,
            app,
            error_count,
            config,
            session,
        )
        .await
        {
            return Some(action);
        }
    }
}
