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

/// Outcome of one combined poll of `accept` + all live sessions.
enum Turn {
    /// `accept` produced a new connection to serve.
    Accepted(tokio::net::UnixStream),
    /// `accept` errored persistently (e.g. `EMFILE` fd exhaustion, which
    /// does NOT clear by re-polling). Stop accepting entirely so we never
    /// busy-spin the single recovery thread re-polling a dead listener.
    AcceptDead,
    /// Nothing accepted this turn, but the outer loop should re-evaluate
    /// (shutdown flipped or a session finished).
    Progressed,
}

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

        // One combined poll of accept + all sessions.
        match drive_once(listener, shutdown, &mut sessions).await {
            Turn::Accepted(stream) => {
                // New authenticated-or-not connection: push its session.
                // `handle_connection` does the peercred/handshake itself
                // and is a no-op for rejected peers, so pushing
                // unconditionally is correct and keeps `accept`
                // non-blocking.
                sessions.push(Box::pin(handle_connection(stream, config, shutdown, sink)));
            }
            // The listener is wedged (e.g. fd exhaustion). Re-polling it
            // would spin at 100% CPU and flood the log, so stop accepting
            // for the rest of this recovery — mirroring the
            // `bind_listener`-failure fallback. Returning drops any
            // in-flight sessions (clients EOF), exactly like shutdown.
            Turn::AcceptDead => return,
            // Shutdown flipped or a session finished: loop re-evaluates.
            Turn::Progressed => {}
        }
    }
}

/// Poll `accept` and every live session exactly once (cooperatively),
/// then yield. Removes any session that completed this turn. Returns a
/// [`Turn`] describing what (if anything) is ready.
///
/// We resolve as soon as EITHER `accept` is ready (or errors) OR at least
/// one session made terminal progress OR shutdown flips — so the outer
/// loop can react promptly. If nothing is ready we stay `Pending` (parked
/// by tokio's reactor/timer via the polled sub-futures).
async fn drive_once(
    listener: &UnixListener,
    shutdown: &Shutdown,
    sessions: &mut Vec<Session<'_>>,
) -> Turn {
    poll_fn(|cx| {
        // Park on shutdown so a later `signal()` wakes us even when no
        // session or `accept` is ready — without this the `Pending`
        // branch below could sleep through a shutdown request.
        shutdown.register(cx);

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
            Poll::Ready(Ok((stream, _addr))) => return Poll::Ready(Turn::Accepted(stream)),
            Poll::Ready(Err(e)) => {
                // A `poll_accept` error here is typically persistent (fd
                // exhaustion / `EMFILE` does not clear on re-poll). Log
                // ONCE and tell the outer loop to stop accepting, instead
                // of re-polling immediately and spinning the thread.
                nmbl_warn!("remote-tui: accept failed: {e}; remote attach disabled");
                return Poll::Ready(Turn::AcceptDead);
            }
            Poll::Pending => {}
        }

        // Shutdown requested, or a session finished: let the outer loop
        // re-evaluate its exit condition / sink.
        if shutdown.is_signalled() || progressed {
            return Poll::Ready(Turn::Progressed);
        }
        Poll::Pending
    })
    .await
}
