use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

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

/// Production [`Pacer`]: sleeps ~1ms between driver passes via the
/// tokio timer. This is the real pacing the module docs reserved the
/// seam for — a 1ms `tokio::time::sleep` keeps the driver responsive
/// without busy-spinning. Wired in by [`crate::ui::spawn_poller`].
#[derive(Debug, Default, Clone, Copy)]
pub struct TokioPacer;

impl Pacer for TokioPacer {
    fn pace(&self) -> Pin<Box<dyn Future<Output = ()> + '_>> {
        Box::pin(tokio::time::sleep(std::time::Duration::from_millis(1)))
    }
}

/// Default [`Pacer`]: yields to the executor exactly once.
///
/// Kept as the tokio-free analogue used by the module's own unit tests
/// (which drive the driver under a hand-rolled `std`-only executor with
/// no tokio timer). Production uses [`TokioPacer`].
#[derive(Debug, Default, Clone, Copy)]
pub struct YieldPacer;

impl Pacer for YieldPacer {
    fn pace(&self) -> Pin<Box<dyn Future<Output = ()> + '_>> {
        Box::pin(YieldOnce { yielded: false })
    }
}

/// A future that returns `Pending` exactly once (re-scheduling itself
/// immediately) and then `Ready`. Used by [`YieldPacer`].
pub(super) struct YieldOnce {
    pub(super) yielded: bool,
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
