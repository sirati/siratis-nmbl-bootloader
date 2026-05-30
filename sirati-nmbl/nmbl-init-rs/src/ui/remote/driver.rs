//! Cooperative accept + session multiplexer for the remote-TUI server.
//!
//! NMBL is one OS thread and the recovery state is borrowed (not
//! `'static`), so per-connection futures cannot be
//! `tokio::task::spawn_local`'d. Instead we hold the live session futures
//! in a [`FuturesUnordered`] (the ready-made concurrent-set driver) and,
//! on every wake, poll `accept`, the session set, and the wakeable
//! [`Shutdown`] together. A session blocked on its pty stays `Pending` and
//! never blocks `accept`, giving the same non-starvation property
//! `spawn_local` would — without the `'static` bound and without any
//! worker thread. Boxing the session futures lets a heterogeneous,
//! borrowed (non-`'static`) set live in one `FuturesUnordered`.

use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::task::Poll;

use futures_util::stream::{FuturesUnordered, Stream};
use tokio::net::UnixListener;

use crate::config::Config;
use crate::nmbl_warn;

use super::{ActionSink, Shutdown, handle_connection};

/// A live per-connection session future. Boxed + pinned so a
/// heterogeneous, borrowed (non-`'static`) set can live in one
/// [`FuturesUnordered`].
type Session<'a> = Pin<Box<dyn Future<Output = ()> + 'a>>;

/// Outcome of one combined poll of `accept` + the session set + shutdown.
enum Turn {
    /// `accept` produced a new connection to serve.
    Accepted(tokio::net::UnixStream),
    /// `accept` errored persistently (e.g. `EMFILE` fd exhaustion, which
    /// does NOT clear by re-polling). Stop accepting entirely so we never
    /// busy-spin the single recovery thread re-polling a dead listener.
    AcceptDead,
    /// Nothing accepted this turn, but the outer loop should re-evaluate
    /// (shutdown flipped or a session finished and was reaped).
    Progressed,
}

/// Accept connections and drive their sessions concurrently until
/// `shutdown` is signalled and all in-flight sessions have finished (or
/// the caller drops us, which drops every session → EOFs the clients).
///
/// Each turn: if shutdown is signalled, return at once; otherwise poll
/// `accept`, the [`FuturesUnordered`] session set, and the wakeable
/// [`Shutdown`] together via [`drive_once`]. A new connection pushes a
/// `handle_connection` future into the set; a finished session is reaped
/// by the set.
pub(super) async fn accept_loop(
    listener: &UnixListener,
    config: &Config,
    shutdown: &Shutdown,
    sink: &ActionSink,
    sender: &crate::sys::poller::LocalSender,
) {
    let mut sessions: FuturesUnordered<Session<'_>> = FuturesUnordered::new();

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

        // One combined poll of accept + the session set + shutdown.
        match drive_once(listener, shutdown, &mut sessions).await {
            Turn::Accepted(stream) => {
                // New authenticated-or-not connection: push its session.
                // `handle_connection` does the peercred/handshake itself
                // and is a no-op for rejected peers, so pushing
                // unconditionally is correct and keeps `accept`
                // non-blocking.
                sessions.push(Box::pin(handle_connection(
                    stream, config, shutdown, sink, sender,
                )));
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

/// Poll `accept`, the live session set, and shutdown together, then yield.
/// Returns a [`Turn`] describing what (if anything) is ready.
///
/// We resolve as soon as EITHER `accept` is ready (or errors) OR at least
/// one session was reaped this poll OR shutdown flips — so the outer loop
/// can react promptly. If nothing is ready we stay `Pending` (parked by
/// tokio's reactor/timer via the polled sub-futures and by `shutdown`'s
/// stashed waker).
///
/// The session set is only polled when NON-EMPTY: a `FuturesUnordered`
/// resolves `Ready(None)` the instant it is empty, which would otherwise
/// turn the outer loop into a busy-spin. When empty we simply skip it and
/// rely on `accept` / `shutdown` to wake us.
async fn drive_once(
    listener: &UnixListener,
    shutdown: &Shutdown,
    sessions: &mut FuturesUnordered<Session<'_>>,
) -> Turn {
    poll_fn(|cx| {
        // Park on shutdown so a later `signal()` wakes us even when no
        // session or `accept` is ready — without this the `Pending`
        // branch below could sleep through a shutdown request.
        shutdown.register(cx);

        // Reap finished sessions. Only poll when non-empty: an empty
        // `FuturesUnordered` yields `Ready(None)` immediately, which would
        // busy-spin the outer loop. `Ready(Some(()))` means one session
        // completed (it has already run, including any sink/shutdown
        // bookkeeping) and was reaped; treat that as progress so the outer
        // loop re-evaluates.
        let mut progressed = false;
        if !sessions.is_empty()
            && let Poll::Ready(Some(())) = Pin::new(&mut *sessions).poll_next(cx)
        {
            progressed = true;
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
