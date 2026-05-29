#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests can panic on assertion failure"
)]

use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use super::driver::build;
use super::pacer::{YieldOnce, YieldPacer};
use super::types::{SysCallPoll, SysCallState};

/// A minimal `std`-only single-future executor. Polls `fut` until
/// it is `Ready` or `max_polls` is exhausted, returning whether it
/// completed. We supply a real [`Waker`] (via [`Wake`], no unsafe)
/// so self-rescheduling futures like [`YieldOnce`] behave.
fn poll_n<F: Future>(mut fut: Pin<&mut F>, max_polls: usize) -> (Option<F::Output>, usize) {
    let waker = Waker::from(Arc::new(CountingWaker::default()));
    let mut cx = Context::from_waker(&waker);
    for n in 0..max_polls {
        if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
            return (Some(v), n + 1);
        }
    }
    (None, max_polls)
}

/// A safe [`Wake`] implementation that counts wakes. Built from an
/// `Arc` via the stable `Waker::from(Arc<W: Wake>)`, so no `unsafe`
/// `RawWaker` plumbing is needed.
#[derive(Default)]
struct CountingWaker {
    count: std::sync::atomic::AtomicUsize,
}

impl Wake for CountingWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Op that reports `Done` after exactly `remaining` polls, bumping
/// a shared counter on each poll and on drop.
struct CountdownOp {
    remaining: usize,
    polls: Rc<Cell<usize>>,
    dropped: Rc<Cell<bool>>,
}

impl SysCallPoll for CountdownOp {
    fn poll(&mut self) -> SysCallState {
        self.polls.set(self.polls.get() + 1);
        if self.remaining == 0 {
            SysCallState::Done
        } else {
            self.remaining -= 1;
            SysCallState::Pending
        }
    }
}

impl Drop for CountdownOp {
    fn drop(&mut self) {
        self.dropped.set(true);
    }
}

#[test]
fn run_pass_drives_op_to_completion_and_drops_it() {
    let (poller, sender) = build();
    let polls = Rc::new(Cell::new(0));
    let dropped = Rc::new(Cell::new(false));
    sender.send(Box::new(CountdownOp {
        remaining: 2,
        polls: Rc::clone(&polls),
        dropped: Rc::clone(&dropped),
    }));

    // Pass 1: poll -> Pending (remaining 2 -> 1), still queued.
    assert_eq!(poller.run_pass(), 1);
    assert!(!dropped.get());
    // Pass 2: Pending (1 -> 0).
    assert_eq!(poller.run_pass(), 1);
    assert!(!dropped.get());
    // Pass 3: remaining == 0 -> Done, op dropped, queue empty.
    assert_eq!(poller.run_pass(), 0);
    assert!(dropped.get());
    assert_eq!(polls.get(), 3);
    assert_eq!(sender.pending(), 0);
}

#[test]
fn empty_queue_parks_without_spinning() {
    let (poller, _sender) = build();
    // No ops queued: a pass leaves the queue empty.
    assert_eq!(poller.run_pass(), 0);
    // The park future returns Pending (parks) when no wake pending.
    let park = poller.park();
    let mut park = std::pin::pin!(park);
    let waker = Waker::from(Arc::new(CountingWaker::default()));
    let mut cx = Context::from_waker(&waker);
    assert_eq!(park.as_mut().poll(&mut cx), Poll::Pending);
    // A waker is now registered for a later wake.
    assert!(poller.shared.waker.borrow().is_some());
}

#[test]
fn sender_enqueue_wakes_parked_driver() {
    let (poller, sender) = build();
    // Park the driver first.
    let mut park = poller.park();
    let parker = Waker::from(Arc::new(CountingWaker::default()));
    let mut cx = Context::from_waker(&parker);
    {
        let p = std::pin::Pin::new(&mut park);
        assert_eq!(p.poll(&mut cx), Poll::Pending);
    }
    assert!(poller.shared.waker.borrow().is_some());

    // Enqueue: flags woken and wakes the stored waker.
    let polls = Rc::new(Cell::new(0));
    let dropped = Rc::new(Cell::new(false));
    sender.send(Box::new(CountdownOp {
        remaining: 0,
        polls: Rc::clone(&polls),
        dropped: Rc::clone(&dropped),
    }));
    assert!(poller.shared.woken.get());
    // The stored waker was taken by `wake()`.
    assert!(poller.shared.waker.borrow().is_none());

    // Re-polling the park future now resolves Ready (wake observed).
    {
        let p = std::pin::Pin::new(&mut park);
        assert_eq!(p.poll(&mut cx), Poll::Ready(()));
    }

    // And a subsequent pass drives the enqueued op to completion.
    assert_eq!(poller.run_pass(), 0);
    assert!(dropped.get());
}

#[test]
fn run_with_completes_queued_ops_then_parks() {
    let (poller, sender) = build();
    let polls = Rc::new(Cell::new(0));
    let dropped = Rc::new(Cell::new(false));
    sender.send(Box::new(CountdownOp {
        remaining: 3,
        polls: Rc::clone(&polls),
        dropped: Rc::clone(&dropped),
    }));

    let fut = poller.run_with(YieldPacer);
    let mut fut = std::pin::pin!(fut);
    // `run` never returns; poll a bounded number of times. The op
    // needs 4 passes (3 Pending + 1 Done), each Pending pass paces
    // via YieldOnce (one extra poll), then it parks forever.
    let (out, _used) = poll_n(fut.as_mut(), 64);
    assert!(out.is_none(), "driver loop must not terminate");
    assert!(dropped.get(), "op should have completed and dropped");
    assert_eq!(polls.get(), 4);
    assert_eq!(sender.pending(), 0);
}

#[test]
fn yield_once_paces_without_completing_first_poll() {
    let mut y = YieldOnce { yielded: false };
    let waker = Waker::from(Arc::new(CountingWaker::default()));
    let mut cx = Context::from_waker(&waker);
    let p = std::pin::Pin::new(&mut y);
    assert_eq!(p.poll(&mut cx), Poll::Pending);
    let p = std::pin::Pin::new(&mut y);
    assert_eq!(p.poll(&mut cx), Poll::Ready(()));
}
