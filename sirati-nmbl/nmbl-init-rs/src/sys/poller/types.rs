/// Result of attempting to make progress on a [`SysCallPoll`] op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SysCallState {
    /// The op has not completed; keep it queued and poll it again.
    Pending,
    /// The op finished. The driver drops it, which wakes any waiter.
    Done,
}

/// A unit of polled, syscall-style work driven by [`super::LocalPoller`].
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
/// [`std::task::Waker`]) so the waiting future is woken.
pub trait SysCallPoll {
    /// Attempt to make progress. Return [`SysCallState::Done`] once
    /// the work is complete (the driver then drops `self`).
    fn poll(&mut self) -> SysCallState;
}
