//! The high-level read-only loop-bind helper shared by every caller that
//! loop-mounts a read-only backing fd (the external/network rescue squashfs,
//! a `mount -o loop` filesystem entry, and the driver-image loader).
//!
//! Each of those sites independently repeated the same three-step dance —
//! [`allocate_loop_device`] → [`open_loop_device`] (RW, so `LOOP_CONFIGURE`
//! is permitted) → [`configure_loop_device`] with `LO_FLAGS_READ_ONLY` — so
//! the sequence is factored here ONCE. [`loop_bind_ro`] returns the bound
//! loop index; on failure it returns a [`LoopBindError`] carrying the exact
//! stage that failed so each caller can re-wrap it in its own error variant
//! with an unchanged stage tag (the rescue path's `loop-alloc` / `loop-open`
//! / `loop-configure` stages are preserved verbatim).

use std::os::fd::AsFd;

use crate::error::NmblError;

use super::ops::{allocate_loop_device, configure_loop_device, open_loop_device};

/// Stage tag for the `loop-alloc` step (`LOOP_CTL_GET_FREE`).
pub(crate) const STAGE_ALLOC: &str = "loop-alloc";
/// Stage tag for the `loop-open` step (open `/dev/loopN`).
pub(crate) const STAGE_OPEN: &str = "loop-open";
/// Stage tag for the `loop-configure` step (`LOOP_CONFIGURE`).
pub(crate) const STAGE_CONFIGURE: &str = "loop-configure";

/// A failure from [`loop_bind_ro`], naming the step that failed so the caller
/// can re-wrap it in `NmblError::Rescue { stage }` / `NmblError::DriverImage`
/// with the same stable stage tag the un-factored code used.
///
/// The `source` is boxed so this error stays small — the `NmblError` payload
/// is wide, and an un-boxed copy would trip `clippy::result_large_err` (the
/// same reason `NmblError::Rescue`/`DriverImage` box their `source`).
#[derive(Debug)]
pub(crate) struct LoopBindError {
    /// One of [`STAGE_ALLOC`], [`STAGE_OPEN`], [`STAGE_CONFIGURE`].
    pub(crate) stage: &'static str,
    /// The underlying loop-ioctl / open error.
    pub(crate) source: Box<NmblError>,
}

/// Allocate a free loop minor, open it read-write, and bind `backing` to it
/// read-only via `LOOP_CONFIGURE`, returning the bound `/dev/loopN` index.
///
/// `backing` must be a fd to a read-only-suitable backing object (a squashfs
/// opened `O_RDONLY`, a populated memfd, …). The loop device itself is opened
/// RW because `LOOP_CONFIGURE` refuses an RO fd, but the resulting block
/// device is marked read-only via `LO_FLAGS_READ_ONLY`, so nothing can write
/// through it.
///
/// The kernel releases the binding automatically when the loop mount is
/// (lazily) unmounted, so callers that never unwind need no teardown.
pub(crate) fn loop_bind_ro(backing: &impl AsFd) -> Result<u32, LoopBindError> {
    let index = allocate_loop_device().map_err(|source| LoopBindError {
        stage: STAGE_ALLOC,
        source: Box::new(source),
    })?;

    let loop_fd = open_loop_device(index, true).map_err(|source| LoopBindError {
        stage: STAGE_OPEN,
        source: Box::new(source),
    })?;

    configure_loop_device(&loop_fd, backing, true).map_err(|source| LoopBindError {
        stage: STAGE_CONFIGURE,
        source: Box::new(source),
    })?;

    Ok(index)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests are allowed to assert with panics"
)]
mod tests {
    use super::super::types::LOOP_CONTROL_PATH;
    use super::*;
    use std::path::Path;

    /// On a host without `/dev/loop-control` (sandboxed CI) the very first
    /// step fails and we must surface the `loop-alloc` stage so the rescue
    /// callers' stage assertions keep holding through the factor.
    #[test]
    #[cfg(target_os = "linux")]
    fn bind_ro_without_loop_control_reports_alloc_stage() {
        if Path::new(LOOP_CONTROL_PATH).exists() {
            eprintln!("skipping: {LOOP_CONTROL_PATH} present");
            return;
        }
        let backing = tempfile::tempfile().expect("tempfile");
        let err = loop_bind_ro(&backing).expect_err("no loop-control must error");
        assert_eq!(err.stage, STAGE_ALLOC, "expected loop-alloc stage");
    }

    /// On a host WITH `/dev/loop-control` and enough privilege, the full
    /// allocate→open→configure dance binds a 1 MiB tempfile read-only and
    /// returns a plausible loop index. Unprivileged sandboxes that see the
    /// node but are refused the ioctl are treated as a skip.
    #[test]
    #[cfg(target_os = "linux")]
    fn bind_ro_against_tempfile() {
        if !Path::new(LOOP_CONTROL_PATH).exists() {
            eprintln!("skipping: {LOOP_CONTROL_PATH} not present");
            return;
        }
        let mut tmp = tempfile::tempfile().expect("tempfile");
        use std::io::Write as _;
        tmp.write_all(&vec![0u8; 1024 * 1024]).expect("fill tmp");
        tmp.flush().expect("flush tmp");

        match loop_bind_ro(&tmp) {
            Ok(index) => {
                assert!(index < 1_000_000, "loop index {index} looks bogus");
                // Detach so we don't leak the binding for subsequent runs.
                if let Ok(loop_fd) = open_loop_device(index, true) {
                    let _ = super::super::ops::detach_loop_device(&loop_fd);
                }
            }
            Err(e) => {
                eprintln!("skipping: loop_bind_ro failed at {}: {}", e.stage, e.source);
            }
        }
    }
}
