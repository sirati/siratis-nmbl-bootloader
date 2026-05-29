//! A custom single-threaded poller for syscall-style operations that
//! have no ready-made async wrapper.
//!
//! This is designed for a tokio **current-thread** runtime (one OS
//! thread, `spawn_local`). tokio is not yet a dependency on this
//! branch, so this module is deliberately tokio-agnostic: it compiles
//! and is fully unit-testable against `std` alone. The two
//! integration seams that Phase 1b will wire to tokio are marked
//! explicitly (see [`LocalPoller::run`] and [`Pacer`]).
//!
//! # Model
//!
//! A [`SysCallPoll`] is a unit of polled work. The driver repeatedly
//! calls [`SysCallPoll::poll`]; when it returns [`SysCallState::Done`]
//! the op is dropped, which wakes anything waiting on it (a waiter
//! parks on a oneshot/`Drop`-signalling handle stored *inside* the op).
//!
//! Ops live in a shared queue `Rc<RefCell<VecDeque<…>>>`. The single
//! hard invariant: the `RefCell` borrow is released before any
//! `.await`. Each driver pass briefly borrows the queue, drains the
//! pending ops into a local buffer, releases the borrow, then polls
//! each op with no borrow held. Survivors (still `Pending`) are pushed
//! back afterwards under a fresh, short borrow.
//!
//! [`LocalSender`] is a cheap-to-clone handle that enqueues a new op
//! and wakes the driver. [`LocalPoller`] is the driver itself. Build
//! both from one shared state with [`build`].

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

/// Result of attempting to make progress on a [`SysCallPoll`] op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SysCallState {
    /// The op has not completed; keep it queued and poll it again.
    Pending,
    /// The op finished. The driver drops it, which wakes any waiter.
    Done,
}

/// A unit of polled, syscall-style work driven by [`LocalPoller`].
///
/// Implementors should perform one **non-blocking** attempt per
/// [`poll`](SysCallPoll::poll) call (e.g. a non-blocking syscall that
/// can return `EAGAIN`) and report whether they are finished. They
/// must never block the thread: this runs on a single-threaded
/// executor and a blocking op stalls everything.
///
/// When an op completes it is dropped by the driver; an op that backs
/// an `.await`able future typically signals completion from its
/// `Drop` (or by flipping a shared flag and calling a stored
/// [`Waker`]) so the waiting future is woken.
pub trait SysCallPoll {
    /// Attempt to make progress. Return [`SysCallState::Done`] once
    /// the work is complete (the driver then drops `self`).
    fn poll(&mut self) -> SysCallState;
}

/// State shared between the [`LocalSender`]s and the [`LocalPoller`].
struct Shared {
    /// Queue of pending ops. Borrowed only briefly, never across an
    /// `.await` (see module docs).
    queue: RefCell<VecDeque<Box<dyn SysCallPoll>>>,
    /// Set when an enqueue (or other event) should rouse a parked
    /// driver. Read-and-cleared by the driver's park future.
    woken: Cell<bool>,
    /// The driver's current [`Waker`], stored while it is parked so a
    /// sender can wake it. `None` while the driver is actively looping.
    waker: RefCell<Option<Waker>>,
}

impl Shared {
    fn new() -> Self {
        Self {
            queue: RefCell::new(VecDeque::new()),
            woken: Cell::new(false),
            waker: RefCell::new(None),
        }
    }

    /// Flag a wake and rouse the parked driver, if any.
    fn wake(&self) {
        self.woken.set(true);
        // Take the waker out before calling it so we don't hold the
        // borrow across `wake()` (which is foreign code).
        let waker = self.waker.borrow_mut().take();
        if let Some(w) = waker {
            w.wake();
        }
    }
}

/// A cheaply-clonable handle that enqueues new ops onto the shared
/// queue and wakes the [`LocalPoller`] driver.
#[derive(Clone)]
pub struct LocalSender {
    shared: Rc<Shared>,
}

impl LocalSender {
    /// Enqueue an op and wake the driver so it polls on its next pass.
    pub fn send(&self, op: Box<dyn SysCallPoll>) {
        self.shared.queue.borrow_mut().push_back(op);
        // Wake *after* releasing the queue borrow above.
        self.shared.wake();
    }

    /// Number of ops currently queued (mainly for tests/diagnostics).
    pub fn pending(&self) -> usize {
        self.shared.queue.borrow().len()
    }
}

/// The async pacing seam.
///
/// Between two driver passes that *still have work left*, the driver
/// waits a short interval (~1ms) instead of busy-spinning. In Phase 1b
/// this becomes a single `tokio::time::sleep(Duration::from_millis(1))
/// .await`. Until then it is an injectable async hook so the driver's
/// control flow is identical to the final tokio version; the default
/// [`YieldPacer`] simply yields once to the executor.
pub trait Pacer {
    /// The future produced by one pacing wait. Boxed so the trait is
    /// object-safe and the driver can hold a `dyn Pacer`.
    fn pace(&self) -> Pin<Box<dyn Future<Output = ()> + '_>>;
}

/// Default [`Pacer`]: yields to the executor exactly once.
///
/// Phase 1b replaces this with a tokio-timer-backed pacer that sleeps
/// ~1ms. A single-`yield` future is the closest tokio-free analogue
/// that neither blocks nor busy-spins.
#[derive(Debug, Default, Clone, Copy)]
pub struct YieldPacer;

impl Pacer for YieldPacer {
    fn pace(&self) -> Pin<Box<dyn Future<Output = ()> + '_>> {
        Box::pin(YieldOnce { yielded: false })
    }
}

/// A future that returns `Pending` exactly once (re-scheduling itself
/// immediately) and then `Ready`. Used by [`YieldPacer`].
struct YieldOnce {
    yielded: bool,
}

impl Future for YieldOnce {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.yielded {
            Poll::Ready(())
        } else {
            self.yielded = true;
            // Re-schedule ourselves so the executor polls us again.
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

/// The driver. Drains and polls the shared queue, parking when the
/// queue empties and pacing when work remains.
pub struct LocalPoller {
    shared: Rc<Shared>,
}

impl LocalPoller {
    /// One driver pass over the currently-queued ops.
    ///
    /// Drains the queue into a local buffer under a brief borrow,
    /// releases the borrow, polls each op with **no** borrow held,
    /// drops the ones that report [`SysCallState::Done`], and pushes
    /// the survivors back under a fresh brief borrow.
    ///
    /// Returns the number of ops still pending after the pass.
    fn run_pass(&self) -> usize {
        // Brief borrow #1: take everything currently queued. Ops that
        // a sender enqueues *during* this pass land in the queue and
        // are picked up by the next pass (and have already flagged a
        // wake), so we never miss them.
        let mut batch: VecDeque<Box<dyn SysCallPoll>> = {
            let mut q = self.shared.queue.borrow_mut();
            std::mem::take(&mut *q)
        };

        // No borrow held here while we poll arbitrary op code.
        let mut survivors: VecDeque<Box<dyn SysCallPoll>> = VecDeque::with_capacity(batch.len());
        while let Some(mut op) = batch.pop_front() {
            match op.poll() {
                SysCallState::Done => {
                    // Dropping `op` here wakes its waiter.
                    drop(op);
                }
                SysCallState::Pending => survivors.push_back(op),
            }
        }

        // Brief borrow #2: re-queue survivors *ahead* of anything a
        // sender appended mid-pass, preserving FIFO order.
        let mut q = self.shared.queue.borrow_mut();
        while let Some(op) = survivors.pop_back() {
            q.push_front(op);
        }
        q.len()
    }

    /// Park until a sender wakes us. Resolves immediately if a wake was
    /// already flagged (avoids a lost-wakeup race).
    fn park(&self) -> Park<'_> {
        Park {
            shared: &self.shared,
        }
    }

    /// Run the driver to completion with the default [`YieldPacer`].
    ///
    /// This never returns on its own (a long-lived boot-time driver);
    /// in tests it is polled a bounded number of times via the
    /// hand-rolled executor.
    pub async fn run(self) {
        self.run_with(YieldPacer).await;
    }

    /// Run the driver using a caller-supplied [`Pacer`].
    ///
    /// Loop shape (identical to the eventual tokio version):
    /// * Poll every queued op once ([`run_pass`](Self::run_pass)).
    /// * If work **remains**, pace (`pacer.pace().await`) then loop —
    ///   Phase 1b swaps the pacer for `tokio::time::sleep(1ms)`.
    /// * If the queue is **empty**, park on our waker
    ///   ([`park`](Self::park)`.await`) until a sender wakes us.
    ///
    /// There is no busy spin: every "nothing ready" path awaits.
    pub async fn run_with<P: Pacer>(self, pacer: P) {
        loop {
            let remaining = self.run_pass();
            if remaining == 0 {
                // Queue drained: sleep until a sender enqueues + wakes.
                self.park().await;
            } else {
                // Work left: brief paced wait, then re-poll.
                // Phase 1b: tokio::time::sleep(Duration::from_millis(1)).await
                pacer.pace().await;
            }
        }
    }
}

/// Future that parks the driver until [`Shared::wake`] is called.
///
/// Registers the current [`Waker`] in the shared state and returns
/// `Pending`; resolves once the `woken` flag is observed set (which a
/// sender does, then calls the stored waker).
struct Park<'a> {
    shared: &'a Shared,
}

impl Future for Park<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        // Consume any pending wake first to avoid a lost wakeup: if a
        // sender flagged a wake between our last pass and now, return
        // immediately rather than parking forever.
        if self.shared.woken.replace(false) {
            return Poll::Ready(());
        }
        // Store our waker so a future `wake()` reaches us.
        *self.shared.waker.borrow_mut() = Some(cx.waker().clone());
        // Re-check after registering, closing the register/wake race.
        if self.shared.woken.replace(false) {
            // Drop the now-stale stored waker.
            self.shared.waker.borrow_mut().take();
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

/// Build a [`LocalPoller`] driver and a [`LocalSender`] sharing one
/// queue + waker. Clone the sender freely; there is exactly one driver.
pub fn build() -> (LocalPoller, LocalSender) {
    let shared = Rc::new(Shared::new());
    (
        LocalPoller {
            shared: Rc::clone(&shared),
        },
        LocalSender { shared },
    )
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests can panic on assertion failure"
)]
mod tests {
    use std::sync::Arc;
    use std::task::Wake;

    use super::*;

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
}
