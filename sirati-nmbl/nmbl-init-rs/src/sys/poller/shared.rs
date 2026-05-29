use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::task::Waker;

use super::types::SysCallPoll;

/// State shared between the [`LocalSender`]s and the [`super::LocalPoller`].
pub(super) struct Shared {
    /// Queue of pending ops. Borrowed only briefly, never across an
    /// `.await` (see module docs).
    pub(super) queue: RefCell<VecDeque<Box<dyn SysCallPoll>>>,
    /// Set when an enqueue (or other event) should rouse a parked
    /// driver. Read-and-cleared by the driver's park future.
    pub(super) woken: Cell<bool>,
    /// The driver's current [`Waker`], stored while it is parked so a
    /// sender can wake it. `None` while the driver is actively looping.
    pub(super) waker: RefCell<Option<Waker>>,
}

impl Shared {
    pub(super) fn new() -> Self {
        Self {
            queue: RefCell::new(VecDeque::new()),
            woken: Cell::new(false),
            waker: RefCell::new(None),
        }
    }

    /// Flag a wake and rouse the parked driver, if any.
    pub(super) fn wake(&self) {
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
/// queue and wakes the [`super::LocalPoller`] driver.
#[derive(Clone)]
pub struct LocalSender {
    pub(super) shared: Rc<Shared>,
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
