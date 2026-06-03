//! Source-device readiness wait for the activation orchestrator.
//!
//! Routes the existence check and re-sweep through the `ops` seam so the
//! genuine boot drives `RealSys` while `--validate-initrm` drives the
//! side-effect-free `DryRunSys`.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::devices::format_wait_phase;
use crate::error::{NmblError, Result};
use crate::nmbl_warn;
use crate::sys::ops::{BlockOps, FsOps};
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
/// Routes the existence check and re-sweep through `ops` so the genuine
/// boot drives `RealSys` (`Path::exists` + `populate_disk_by_symlinks`)
/// while `--validate-initrm` drives `DryRunSys` (trivially-ready presence
/// check, no fork/mknod). The two `ops` calls run sequentially inside the
/// loop, so a shared `&` probe and a `&mut` sweep never co-capture.
///
/// Fast path: if `ops.exists(device)` is already true the function returns
/// without polling or sweeping — one existence check, no added latency.
/// On Esc (`ProgressSink::tick` → `Aborted`) it returns
/// [`NmblError::OperatorAborted`]; on deadline it returns
/// [`NmblError::DeviceTimeout`].
pub(super) async fn wait_for_source_device<S: BlockOps + FsOps>(
    ops: &mut S,
    device: &Path,
    timeout: Duration,
    operation: &str,
    mut progress: Option<&mut dyn ProgressSink>,
) -> Result<()> {
    // Fast path: device already present (the common case once the kernel
    // has settled). No sweep, no poll, no sleep — just the one stat.
    if ops.exists(device) {
        return Ok(());
    }

    let start = Instant::now();
    let deadline = start.checked_add(timeout).unwrap_or_else(Instant::now);

    loop {
        // Re-run the by-* symlink sweep so a partition node that has just
        // appeared in /sys/class/block gets its by-partlabel/by-uuid links
        // before we re-probe. Sweep failures are non-fatal — the next
        // iteration retries — but we surface them in the log. The btrfs
        // member list is discarded here (phase-3b re-collects it).
        if let Err(err) = ops.populate_disk_symlinks().await {
            nmbl_warn!(
                "activation: by-* re-sweep while waiting for {} failed (continuing): {}",
                device.display(),
                err,
            );
        }

        if ops.exists(device) {
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
    use std::cell::Cell;
    use std::io;
    use std::path::PathBuf;

    use nix::mount::MntFlags;

    use super::*;

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

    /// Minimal `BlockOps + FsOps` test double driving the new ops-based
    /// `wait_for_source_device`. `exists()` flips true once
    /// `populate_disk_symlinks` has been called `appear_after_sweeps`
    /// times; `sweep_errs` makes every sweep return `Err` to exercise the
    /// non-fatal path. Only the two methods the helper calls
    /// (`exists` / `populate_disk_symlinks`) carry behaviour; the rest are
    /// panicking stubs.
    struct ScriptedOps {
        sweeps: Cell<u32>,
        /// `None` = never appears; `Some(n)` = present after `n` sweeps.
        appear_after_sweeps: Option<u32>,
        sweep_errs: bool,
    }

    impl ScriptedOps {
        fn new(appear_after_sweeps: Option<u32>, sweep_errs: bool) -> Self {
            Self {
                sweeps: Cell::new(0),
                appear_after_sweeps,
                sweep_errs,
            }
        }
        fn sweeps(&self) -> u32 {
            self.sweeps.get()
        }
    }

    impl FsOps for ScriptedOps {
        fn exists(&self, _path: &Path) -> bool {
            match self.appear_after_sweeps {
                Some(n) => self.sweeps.get() >= n,
                None => false,
            }
        }
        fn ensure_dir(&mut self, _path: &Path) -> io::Result<()> {
            panic!("wait_for_source_device never calls ensure_dir")
        }
        fn mount(
            &mut self,
            _source: Option<&Path>,
            _target: &Path,
            _fstype: &str,
            _options: &str,
        ) -> Result<()> {
            panic!("wait_for_source_device never calls mount")
        }
        fn umount(&mut self, _target: &Path, _flags: MntFlags) -> Result<()> {
            panic!("wait_for_source_device never calls umount")
        }
        fn read_file(&self, _path: &Path) -> io::Result<Vec<u8>> {
            panic!("wait_for_source_device never calls read_file")
        }
        fn open_ro(&self, _path: &Path) -> io::Result<std::fs::File> {
            panic!("wait_for_source_device never calls open_ro")
        }
        fn write_file(&mut self, _path: &Path, _contents: &[u8]) -> io::Result<()> {
            panic!("wait_for_source_device never calls write_file")
        }
        fn remove_file(&mut self, _path: &Path) -> io::Result<()> {
            panic!("wait_for_source_device never calls remove_file")
        }
        fn canonicalize(&self, _path: &Path) -> io::Result<PathBuf> {
            panic!("wait_for_source_device never calls canonicalize")
        }
    }

    impl BlockOps for ScriptedOps {
        async fn wait_for_device(
            &mut self,
            _device: &Path,
            _timeout: Duration,
            _operation: &str,
            _progress: Option<&mut dyn ProgressSink>,
        ) -> Result<()> {
            panic!("wait_for_source_device never calls wait_for_device")
        }
        fn ensure_dev_node(&mut self, _sysfs_entry: &Path, _dev_path: &Path) -> Result<bool> {
            panic!("wait_for_source_device never calls ensure_dev_node")
        }
        async fn populate_disk_symlinks(&mut self) -> Result<Vec<PathBuf>> {
            self.sweeps.set(self.sweeps.get() + 1);
            if self.sweep_errs {
                return Err(NmblError::Io {
                    source: io::Error::other("simulated sweep failure"),
                    context: "test sweep".to_string(),
                });
            }
            Ok(Vec::new())
        }
        fn setup_loop(&mut self, _file: &Path) -> Result<PathBuf> {
            panic!("wait_for_source_device never calls setup_loop")
        }
        fn btrfs_scan(&mut self, _devs: &[PathBuf]) -> Result<()> {
            panic!("wait_for_source_device never calls btrfs_scan")
        }
    }

    #[test]
    fn source_wait_present_immediately_skips_poll_and_sweep() {
        // Device is present on the first probe (appears after 0 sweeps):
        // the helper must return without ever sweeping or ticking (zero
        // added latency beyond the single existence check).
        let mut ops = ScriptedOps::new(Some(0), false);
        let dev = Path::new("/dev/sda2");
        let res = block(wait_for_source_device(
            &mut ops,
            dev,
            Duration::from_secs(5),
            "phase 3: waiting for source",
            None,
        ));
        assert!(res.is_ok(), "present-immediately must return Ok");
        assert_eq!(ops.sweeps(), 0, "fast path must not sweep");
    }

    #[test]
    fn source_wait_absent_then_appearing_returns_ok() {
        // Absent for the first probes, then the partition node (and its
        // by-* symlink) materialises after two sweeps. The helper must
        // re-sweep and succeed once the probe flips true.
        let mut ops = ScriptedOps::new(Some(2), false);
        let dev = Path::new("/dev/disk/by-partlabel/disk-main-luks");
        let mut sink = CountingSink { ticks: 0 };
        let res = block(wait_for_source_device(
            &mut ops,
            dev,
            Duration::from_secs(5),
            "phase 3: waiting for source",
            Some(&mut sink),
        ));
        assert!(
            res.is_ok(),
            "device that appears within budget must return Ok"
        );
        assert!(
            ops.sweeps() >= 2,
            "must have re-swept until the node appeared"
        );
    }

    #[test]
    fn source_wait_absent_past_budget_times_out() {
        // Device never appears: the helper must exhaust the (short) budget
        // and surface a DeviceTimeout naming the path, still re-sweeping on
        // each poll meanwhile.
        let mut ops = ScriptedOps::new(None, false);
        let dev = Path::new("/dev/disk/by-partlabel/never-here");
        let err = block(wait_for_source_device(
            &mut ops,
            dev,
            Duration::from_millis(250),
            "phase 3: waiting for source",
            None,
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
            ops.sweeps() >= 1,
            "must re-sweep at least once while waiting"
        );
    }

    #[test]
    fn source_wait_surfaces_operator_abort() {
        // Esc during the wait (sink ticks Aborted) must propagate as
        // OperatorAborted, not DeviceTimeout, so the emergency menu can
        // tell the operator they cut the wait short.
        let mut ops = ScriptedOps::new(None, false);
        let dev = Path::new("/dev/disk/by-partlabel/slow-disk");
        let mut sink = AbortingSink;
        let err = block(wait_for_source_device(
            &mut ops,
            dev,
            Duration::from_secs(30),
            "phase 3: waiting for source",
            Some(&mut sink),
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
        // retries. Here the device appears (probe flips true) after one
        // sweep, which also errored, proving the loop continued past the
        // failed sweep.
        let mut ops = ScriptedOps::new(Some(1), true);
        let dev = Path::new("/dev/disk/by-partlabel/flaky");
        let res = block(wait_for_source_device(
            &mut ops,
            dev,
            Duration::from_secs(5),
            "phase 3: waiting for source",
            None,
        ));
        assert!(
            res.is_ok(),
            "sweep error must be non-fatal; device appeared next probe"
        );
        assert!(ops.sweeps() >= 1, "must have attempted at least one sweep");
    }
}
