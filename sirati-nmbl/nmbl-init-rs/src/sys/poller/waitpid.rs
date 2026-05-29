//! Non-blocking `waitpid(WNOHANG)` op for the [`super::LocalPoller`].
//!
//! This is the poller's first real consumer: the chrooted external
//! rescue child runs while NMBL stays PID 1, and PID 1 must reap it
//! without blocking the single-threaded `LocalRuntime`. A blocking
//! `waitpid(pid, None)` would freeze the whole event loop (including
//! the concurrent remote-attach server); instead we register a
//! [`WaitpidOp`] that performs one `waitpid(pid, WNOHANG)` per driver
//! pass and reports [`SysCallState::Pending`] until the child exits.
//!
//! The async adapter ([`reap_child`]) avoids `tokio::sync` entirely:
//! the op and the awaiting future share an `Rc<RefCell<…>>` slot for
//! the result plus a stored [`Waker`]. When the op observes the child
//! has exited it stores the [`WaitStatus`], wakes the waiter, and
//! returns `Done`; the driver then drops the op. Single-threaded, so
//! `Rc`/`RefCell` are sufficient — no `Arc`/`Mutex`/`tokio::sync`.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;

use super::shared::LocalSender;
use super::types::{SysCallPoll, SysCallState};

/// Shared completion slot between a [`WaitpidOp`] and the future that
/// awaits it. The op writes `status` and wakes `waker` on completion;
/// the future reads `status` to resolve.
struct ReapShared {
    /// Set once the op has finished (terminal status observed OR
    /// `waitpid` errored). Distinct from `status` so a clean reap with
    /// no collectable status (e.g. `ECHILD`) is still seen as "done".
    done: Cell<bool>,
    /// The terminal status once the child has been reaped. `None` when
    /// `waitpid` could not collect it (e.g. `ECHILD`).
    status: RefCell<Option<WaitStatus>>,
    /// Waker for the awaiting future, stored while it is parked.
    waker: RefCell<Option<Waker>>,
}

/// A polled `waitpid(pid, WNOHANG)`. One non-blocking attempt per
/// [`poll`](SysCallPoll::poll); reports [`SysCallState::Done`] once the
/// child has terminated (`Exited`/`Signaled`) or `waitpid` errors out
/// (e.g. `ECHILD` — already reaped). `StillAlive` keeps it `Pending`.
struct WaitpidOp {
    pid: Pid,
    shared: Rc<ReapShared>,
}

impl SysCallPoll for WaitpidOp {
    fn poll(&mut self) -> SysCallState {
        match waitpid(self.pid, Some(WaitPidFlag::WNOHANG)) {
            // Child still running: try again next pass.
            Ok(WaitStatus::StillAlive) => SysCallState::Pending,
            // Stopped/continued (only delivered with WUNTRACED/WCONTINUED,
            // which we do not pass) — treat defensively as not-yet-done.
            Ok(WaitStatus::Stopped(..)) | Ok(WaitStatus::Continued(..)) => SysCallState::Pending,
            // PtraceEvent/PtraceSyscall are likewise non-terminal.
            #[cfg(any(target_os = "linux", target_os = "android"))]
            Ok(WaitStatus::PtraceEvent(..)) | Ok(WaitStatus::PtraceSyscall(..)) => {
                SysCallState::Pending
            }
            // Terminal: record the status and wake the waiter.
            Ok(status @ (WaitStatus::Exited(..) | WaitStatus::Signaled(..))) => {
                self.complete(Some(status))
            }
            // ECHILD (no such child / already reaped) or any other error:
            // we can never make progress, so finish with no status. The
            // awaiter resolves to `None` and the rescue flow proceeds.
            Err(_) => self.complete(None),
        }
    }
}

impl WaitpidOp {
    /// Store the terminal status, flag completion, wake the awaiting
    /// future, and report [`SysCallState::Done`] so the driver drops the
    /// op.
    fn complete(&self, status: Option<WaitStatus>) -> SysCallState {
        *self.shared.status.borrow_mut() = status;
        self.shared.done.set(true);
        // Take the waker before calling it so we never hold the borrow
        // across foreign wake code.
        let waker = self.shared.waker.borrow_mut().take();
        if let Some(w) = waker {
            w.wake();
        }
        SysCallState::Done
    }
}

/// Future that resolves once the [`WaitpidOp`] reaping `pid` reports
/// the child has terminated. Resolves to `Some(status)` on a clean
/// reap, or `None` if `waitpid` could never collect it (e.g. `ECHILD`).
pub struct ReapFuture {
    shared: Rc<ReapShared>,
    /// `true` once the op has been enqueued on the poller. The op is
    /// submitted lazily on first poll so the future carries the
    /// [`LocalSender`] only until then.
    submitted: bool,
    pid: Pid,
    sender: LocalSender,
}

impl Future for ReapFuture {
    type Output = Option<WaitStatus>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        // Fast path: the op already finished (status may be None on a
        // clean reap that could not be collected, e.g. ECHILD).
        if this.shared.done.get() {
            return Poll::Ready(this.shared.status.borrow_mut().take());
        }
        // Register our waker so the op can rouse us on completion. Done
        // before submitting (and re-stored on every poll) so a wake that
        // fires between submit and the next poll is never lost.
        *this.shared.waker.borrow_mut() = Some(cx.waker().clone());
        if !this.submitted {
            this.sender.send(Box::new(WaitpidOp {
                pid: this.pid,
                shared: Rc::clone(&this.shared),
            }));
            this.submitted = true;
        }
        // Re-check after registering, closing the register/complete race.
        if this.shared.done.get() {
            this.shared.waker.borrow_mut().take();
            Poll::Ready(this.shared.status.borrow_mut().take())
        } else {
            Poll::Pending
        }
    }
}

/// Reap `pid` asynchronously via the poller, returning a future that
/// resolves to its [`WaitStatus`] (or `None` if it could not be
/// collected). The op is enqueued on `sender` on the future's first
/// poll and performs one non-blocking `waitpid(WNOHANG)` per driver
/// pass thereafter — the single-threaded event loop keeps running
/// (notably the concurrent remote-attach server) while the child lives.
pub fn reap_child(pid: Pid, sender: LocalSender) -> ReapFuture {
    ReapFuture {
        shared: Rc::new(ReapShared {
            done: Cell::new(false),
            status: RefCell::new(None),
            waker: RefCell::new(None),
        }),
        submitted: false,
        pid,
        sender,
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;
    use crate::sys::poller::build;
    use nix::sys::wait::WaitStatus;
    use nix::unistd::{ForkResult, fork};
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};

    #[derive(Default)]
    struct NoopWaker;
    impl Wake for NoopWaker {
        fn wake(self: Arc<Self>) {}
        fn wake_by_ref(self: &Arc<Self>) {}
    }

    /// Drive `fut` and the poller together under a hand-rolled std-only
    /// executor: each iteration polls the future, then runs one driver
    /// pass, until the future is ready or `max` iterations elapse.
    fn drive<F: Future>(
        mut fut: Pin<&mut F>,
        poller: &crate::sys::poller::LocalPoller,
        max: usize,
    ) -> Option<F::Output> {
        let waker = Waker::from(Arc::new(NoopWaker));
        let mut cx = Context::from_waker(&waker);
        for _ in 0..max {
            if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
                return Some(v);
            }
            poller.run_pass();
        }
        None
    }

    #[test]
    fn reap_child_collects_exit_status() {
        // Fork a child that immediately exits with a known code; reap it
        // through the poller op and assert the status round-trips. This
        // is the unprivileged-testable core of the rescue child reaper.
        // SAFETY: single-threaded test; the child does nothing but
        // _exit, performing no allocation or Rust I/O.
        let pid = match unsafe { fork() }.expect("fork") {
            ForkResult::Child => {
                // SAFETY: post-fork child; _exit is the only correct
                // termination primitive (async-signal-safe).
                unsafe { libc::_exit(7) };
            }
            ForkResult::Parent { child } => child,
        };

        let (poller, sender) = build();
        let fut = reap_child(pid, sender);
        let mut fut = std::pin::pin!(fut);
        let out = drive(fut.as_mut(), &poller, 10_000).expect("child must be reaped");
        match out {
            Some(WaitStatus::Exited(p, code)) => {
                assert_eq!(p, pid);
                assert_eq!(code, 7);
            }
            other => panic!("expected Exited(_, 7), got {other:?}"),
        }
    }

    #[test]
    fn reap_child_reports_signal_death() {
        // SAFETY: single-threaded test; child only raises SIGKILL on
        // itself via async-signal-safe libc calls.
        let pid = match unsafe { fork() }.expect("fork") {
            ForkResult::Child => {
                // SAFETY: post-fork child; async-signal-safe kill + _exit.
                unsafe {
                    libc::kill(libc::getpid(), libc::SIGKILL);
                    libc::_exit(0);
                }
            }
            ForkResult::Parent { child } => child,
        };

        let (poller, sender) = build();
        let fut = reap_child(pid, sender);
        let mut fut = std::pin::pin!(fut);
        let out = drive(fut.as_mut(), &poller, 10_000).expect("child must be reaped");
        match out {
            Some(WaitStatus::Signaled(p, sig, _)) => {
                assert_eq!(p, pid);
                assert_eq!(sig, nix::sys::signal::Signal::SIGKILL);
            }
            other => panic!("expected Signaled(_, SIGKILL, _), got {other:?}"),
        }
    }

    #[test]
    fn reap_unknown_pid_resolves_none() {
        // A pid we never forked yields ECHILD; the future must resolve
        // to None rather than hang.
        let (poller, sender) = build();
        // A very high pid that is overwhelmingly unlikely to be our child.
        let pid = Pid::from_raw(0x7fff_fffe);
        let fut = reap_child(pid, sender);
        let mut fut = std::pin::pin!(fut);
        let out = drive(fut.as_mut(), &poller, 16).expect("must resolve");
        assert!(
            out.is_none(),
            "ECHILD reap must resolve to None, got {out:?}"
        );
    }
}
