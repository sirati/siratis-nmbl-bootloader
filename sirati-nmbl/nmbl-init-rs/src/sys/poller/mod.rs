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

mod driver;
mod pacer;
mod shared;
mod types;

#[cfg(test)]
mod tests;

pub use driver::{LocalPoller, build};
pub use pacer::{Pacer, TokioPacer, YieldPacer};
pub use shared::LocalSender;
pub use types::{SysCallPoll, SysCallState};
