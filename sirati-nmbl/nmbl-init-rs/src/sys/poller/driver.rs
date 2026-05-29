use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use super::pacer::{Pacer, YieldPacer};
use super::shared::{LocalSender, Shared};
use super::types::{SysCallPoll, SysCallState};

/// The driver. Drains and polls the shared queue, parking when the
/// queue empties and pacing when work remains.
pub struct LocalPoller {
    pub(super) shared: Rc<Shared>,
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
    pub(super) fn run_pass(&self) -> usize {
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
    pub(super) fn park(&self) -> Park<'_> {
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
pub(super) struct Park<'a> {
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
