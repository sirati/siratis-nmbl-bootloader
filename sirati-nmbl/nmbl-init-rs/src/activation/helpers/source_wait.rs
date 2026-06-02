//! Source-device readiness wait/poll for the activation orchestrator.

use std::future::Future;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::devices::format_wait_phase;
use crate::error::{NmblError, Result};
use crate::nmbl_warn;
use crate::ui::{ProgressSink, TickOutcome};

/// Inter-poll cadence while waiting for a source device to materialise.
/// Matches [`crate::devices::wait_for`]'s 100 ms poll loop so the two
/// readiness waits feel identical to the operator.
const SOURCE_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Block until `device` exists, re-running the by-* symlink sweep on each
/// poll so partition nodes that the kernel enumerates asynchronously (USB
/// storage, slow HBAs) get their `/dev/disk/by-*` links created the moment
/// they appear. Bounded by `timeout` (the per-device readiness budget,
/// `config.general.device_timeout_secs`).
///
/// This is the per-activation safety net the one-shot phase-2c sweep can't
/// provide: that sweep runs once, right after `usb_storage`/`uas` load,
/// before the kernel has created `/dev/sda1`, `/dev/sda2`, … — so it sees
/// the whole disk but none of its partitions and creates 0 partition
/// by-partlabel links. cryptsetup is then handed a non-existent
/// `/dev/disk/by-partlabel/disk-main-luks` and exits code 4. Re-sweeping
/// here, after the partition nodes appear, fixes that race.
///
/// Generic over the existence `probe` and the `sweep` callback so the
/// orchestrator can inject the real `Path::exists` + `populate_disk_by_-
/// symlinks`, while unit tests inject fakes and never touch `/dev`.
///
/// Fast path: if `probe(device)` is already true the function returns
/// without polling or sweeping — one existence check, no added latency.
/// On Esc (`ProgressSink::tick` → `Aborted`) it returns
/// [`NmblError::OperatorAborted`]; on deadline it returns
/// [`NmblError::DeviceTimeout`].
pub(crate) async fn wait_for_source_device<P, S, Fut>(
    device: &Path,
    timeout: Duration,
    operation: &str,
    mut progress: Option<&mut dyn ProgressSink>,
    mut probe: P,
    mut sweep: S,
) -> Result<()>
where
    P: FnMut(&Path) -> bool,
    S: FnMut() -> Fut,
    Fut: Future<Output = Result<()>>,
{
    // Fast path: device already present (the common case once the kernel
    // has settled). No sweep, no poll, no sleep — just the one stat.
    if probe(device) {
        return Ok(());
    }

    let start = Instant::now();
    let deadline = start.checked_add(timeout).unwrap_or_else(Instant::now);

    loop {
        // Re-run the by-* symlink sweep so a partition node that has just
        // appeared in /sys/class/block gets its by-partlabel/by-uuid links
        // before we re-probe. Sweep failures are non-fatal — the next
        // iteration retries — but we surface them in the log.
        if let Err(err) = sweep().await {
            nmbl_warn!(
                "activation: by-* re-sweep while waiting for {} failed (continuing): {}",
                device.display(),
                err,
            );
        }

        if probe(device) {
            return Ok(());
        }

        if Instant::now() >= deadline {
            return Err(NmblError::DeviceTimeout {
                device: device.to_path_buf(),
                timeout_ms: timeout.as_millis().min(u128::from(u64::MAX)) as u64,
            });
        }

        // Drive the spinner AND poll for Esc so a slow/absent source
        // device doesn't look frozen and the operator can still abort.
        if let Some(sink) = progress.as_deref_mut() {
            let phase = format_wait_phase(operation, &device.display(), start.elapsed(), timeout);
            if sink.tick(&phase) == TickOutcome::Aborted {
                return Err(NmblError::OperatorAborted {
                    context: format!("{operation} {}", device.display()),
                });
            }
        }
        tokio::time::sleep(SOURCE_POLL_INTERVAL).await;
    }
}
#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests can panic on assertion failure; production lints are too strict for asserts"
)]
mod tests {
    use super::*;

    // ---- wait_for_source_device ------------------------------------

    use std::cell::Cell;

    /// Build a single-thread `LocalRuntime` to drive the async helper —
    /// mirrors `devices::tests` and the production interactive runtime.
    fn block<F: std::future::Future>(fut: F) -> F::Output {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build_local(tokio::runtime::LocalOptions::default())
            .expect("test runtime");
        rt.block_on(fut)
    }

    /// `ProgressSink` that counts ticks and never aborts. Mirrors the
    /// `CountingSink` in `devices::tests` so the wait-spinner cadence is
    /// observable without a real console.
    struct CountingSink {
        ticks: u32,
    }
    impl ProgressSink for CountingSink {
        fn tick(&mut self, _phase: &str) -> TickOutcome {
            self.ticks = self.ticks.saturating_add(1);
            TickOutcome::Continue
        }
        fn render_phase(&mut self, _phase: &str) {}
    }

    /// `ProgressSink` that aborts on the first tick — exercises the
    /// Esc-abort path of the wait loop.
    struct AbortingSink;
    impl ProgressSink for AbortingSink {
        fn tick(&mut self, _phase: &str) -> TickOutcome {
            TickOutcome::Aborted
        }
        fn render_phase(&mut self, _phase: &str) {}
    }

    #[test]
    fn source_wait_present_immediately_skips_poll_and_sweep() {
        // Device is present on the first probe: the helper must return
        // without ever sweeping or ticking (zero added latency beyond the
        // single existence check).
        let sweeps = Cell::new(0u32);
        let dev = Path::new("/dev/sda2");
        let res = block(wait_for_source_device(
            dev,
            Duration::from_secs(5),
            "phase 3: waiting for source",
            None,
            |_p| true,
            || {
                sweeps.set(sweeps.get() + 1);
                async { Ok(()) }
            },
        ));
        assert!(res.is_ok(), "present-immediately must return Ok");
        assert_eq!(sweeps.get(), 0, "fast path must not sweep");
    }

    #[test]
    fn source_wait_absent_then_appearing_returns_ok() {
        // Absent for the first two probes, then the partition node (and
        // its by-* symlink) materialises after the sweep on poll #2. The
        // helper must re-sweep and succeed once the probe flips true.
        let sweeps = Cell::new(0u32);
        let dev = Path::new("/dev/disk/by-partlabel/disk-main-luks");
        // probe() returns false until 2 sweeps have run, then true —
        // models the node appearing only after re-enumeration.
        let probe = |_p: &Path| sweeps.get() >= 2;
        let mut sink = CountingSink { ticks: 0 };
        let res = block(wait_for_source_device(
            dev,
            Duration::from_secs(5),
            "phase 3: waiting for source",
            Some(&mut sink),
            probe,
            || {
                sweeps.set(sweeps.get() + 1);
                async { Ok(()) }
            },
        ));
        assert!(
            res.is_ok(),
            "device that appears within budget must return Ok"
        );
        assert!(
            sweeps.get() >= 2,
            "must have re-swept until the node appeared"
        );
    }

    #[test]
    fn source_wait_absent_past_budget_times_out() {
        // Device never appears: the helper must exhaust the (short) budget
        // and surface a DeviceTimeout naming the path, still re-sweeping on
        // each poll meanwhile.
        let sweeps = Cell::new(0u32);
        let dev = Path::new("/dev/disk/by-partlabel/never-here");
        let err = block(wait_for_source_device(
            dev,
            Duration::from_millis(250),
            "phase 3: waiting for source",
            None,
            |_p| false,
            || {
                sweeps.set(sweeps.get() + 1);
                async { Ok(()) }
            },
        ))
        .expect_err("absent-past-budget must time out");
        match err {
            NmblError::DeviceTimeout { device, timeout_ms } => {
                assert_eq!(device, dev.to_path_buf());
                assert_eq!(timeout_ms, 250);
            }
            other => panic!("expected DeviceTimeout, got {other:?}"),
        }
        assert!(
            sweeps.get() >= 1,
            "must re-sweep at least once while waiting"
        );
    }

    #[test]
    fn source_wait_surfaces_operator_abort() {
        // Esc during the wait (sink ticks Aborted) must propagate as
        // OperatorAborted, not DeviceTimeout, so the emergency menu can
        // tell the operator they cut the wait short.
        let dev = Path::new("/dev/disk/by-partlabel/slow-disk");
        let mut sink = AbortingSink;
        let err = block(wait_for_source_device(
            dev,
            Duration::from_secs(30),
            "phase 3: waiting for source",
            Some(&mut sink),
            |_p| false,
            || async { Ok(()) },
        ))
        .expect_err("Esc abort must surface an error");
        match err {
            NmblError::OperatorAborted { context } => {
                assert!(
                    context.contains("slow-disk"),
                    "abort context must name the device: {context:?}"
                );
            }
            other => panic!("expected OperatorAborted, got {other:?}"),
        }
    }

    #[test]
    fn source_wait_sweep_error_is_non_fatal() {
        // A failing sweep must not abort the wait — the next iteration
        // retries. Here the device appears (probe flips true) only after
        // the sweep has errored at least once, proving the loop continued.
        let sweeps = Cell::new(0u32);
        let dev = Path::new("/dev/disk/by-partlabel/flaky");
        let probe = |_p: &Path| sweeps.get() >= 1;
        let res = block(wait_for_source_device(
            dev,
            Duration::from_secs(5),
            "phase 3: waiting for source",
            None,
            probe,
            || {
                sweeps.set(sweeps.get() + 1);
                async {
                    Err(NmblError::Io {
                        source: std::io::Error::other("simulated sweep failure"),
                        context: "test sweep".to_string(),
                    })
                }
            },
        ));
        assert!(
            res.is_ok(),
            "sweep error must be non-fatal; device appeared next probe"
        );
    }
}
