//! Cooperative accept + session multiplexer for the remote-TUI server.
//!
//! NMBL is one OS thread and the recovery state is borrowed (not
//! `'static`), so per-connection futures cannot be
//! `tokio::task::spawn_local`'d. Instead we hand-roll a single combined
//! future that, on every wake, polls `accept` AND every live session
//! once. A session blocked on its pty stays `Pending` and never blocks
//! `accept`, giving the same non-starvation property `spawn_local` would
//! — without the `'static` bound and without any worker thread.

use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::task::Poll;

use tokio::net::UnixListener;

use crate::config::Config;
use crate::nmbl_warn;

use super::{ActionSink, Shutdown, handle_connection};

/// A live per-connection session future. Boxed + pinned so a
/// heterogeneous set can live in one `Vec` and be polled by reference.
type Session<'a> = Pin<Box<dyn Future<Output = ()> + 'a>>;

/// Accept connections and drive their sessions concurrently until
/// `shutdown` is signalled and all in-flight sessions have finished (or
/// the caller drops us, which drops every session → EOFs the clients).
///
/// Each turn: if shutdown is signalled and no sessions remain, return;
/// otherwise poll `accept` and every session once via [`poll_fn`]. A new
/// connection spawns a `handle_connection` future into the live set; a
/// finished session is removed.
pub(super) async fn accept_loop(
    listener: &UnixListener,
    config: &Config,
    shutdown: &Shutdown,
    sink: &ActionSink,
) {
    let mut sessions: Vec<Session<'_>> = Vec::new();

    loop {
        // Shutdown tears down promptly: return at once, dropping any
        // in-flight sessions. Dropping each session drops its
        // `UnixStream` + pty fd → the client's blocking read EOFs and
        // it exits 0. We deliberately do NOT wait for sessions to
        // finish — the local operator (or a remote committer) already
        // picked a terminal action and the machine is about to reboot /
        // execve.
        if shutdown.is_signalled() {
            return;
        }

        // One combined poll of accept + all sessions. Returns the
        // accepted stream (if any) and whether to re-check shutdown.
        let accepted = drive_once(listener, shutdown, &mut sessions).await;

        if let Some(stream) = accepted {
            // New authenticated-or-not connection: push its session.
            // `handle_connection` does the peercred/handshake itself and
            // is a no-op for rejected peers, so pushing unconditionally
            // is correct and keeps `accept` non-blocking.
            sessions.push(Box::pin(handle_connection(stream, config, shutdown, sink)));
        }
    }
}

/// Poll `accept` and every live session exactly once (cooperatively),
/// then yield. Removes any session that completed this turn. Returns the
/// newly-accepted `UnixStream` if `accept` was ready, else `None`.
///
/// We resolve as soon as EITHER `accept` is ready OR at least one session
/// made terminal progress OR shutdown flips — so the outer loop can react
/// promptly. If nothing is ready we stay `Pending` (parked by tokio's
/// reactor/timer via the polled sub-futures).
async fn drive_once(
    listener: &UnixListener,
    shutdown: &Shutdown,
    sessions: &mut Vec<Session<'_>>,
) -> Option<tokio::net::UnixStream> {
    poll_fn(|cx| {
        // Reap finished sessions; track whether any completed this poll.
        let mut progressed = false;
        let mut i = 0;
        while let Some(session) = sessions.get_mut(i) {
            match session.as_mut().poll(cx) {
                Poll::Ready(()) => {
                    // Drop the finished session future explicitly; the
                    // `must_use` on the boxed future is satisfied by the
                    // bind-to-`_` (it has already run to completion).
                    let _done = sessions.swap_remove(i);
                    progressed = true;
                }
                Poll::Pending => i += 1,
            }
        }

        // Poll accept. A ready connection resolves the combined future.
        match listener.poll_accept(cx) {
            Poll::Ready(Ok((stream, _addr))) => return Poll::Ready(Some(stream)),
            Poll::Ready(Err(e)) => {
                nmbl_warn!("remote-tui: accept failed: {e}");
                // Treat as progress so the outer loop re-evaluates rather
                // than wedging on a persistently-erroring listener.
                return Poll::Ready(None);
            }
            Poll::Pending => {}
        }

        // Shutdown requested, or a session finished: let the outer loop
        // re-evaluate its exit condition / sink.
        if shutdown.is_signalled() || progressed {
            return Poll::Ready(None);
        }
        Poll::Pending
    })
    .await
}
