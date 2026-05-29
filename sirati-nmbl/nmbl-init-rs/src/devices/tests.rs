#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests can panic on assertion failure"
)]

use super::*;
use std::path::PathBuf;

fn fs_entry(device: &str, mountpoint: &str, is_root: bool) -> FilesystemEntry {
    FilesystemEntry {
        device: device.to_string(),
        mountpoint: PathBuf::from(mountpoint),
        fstype: "ext4".to_string(),
        options: String::new(),
        is_root,
    }
}

/// Counting ProgressSink for tests. Records every call and the most
/// recent phase string so we can assert both the cadence (~N ticks
/// per second of wait) and the format of the status line.
struct CountingSink {
    ticks: u32,
    last_phase: Option<String>,
}

impl CountingSink {
    fn new() -> Self {
        Self {
            ticks: 0,
            last_phase: None,
        }
    }
}

impl ProgressSink for CountingSink {
    fn tick(&mut self, phase: &str) -> TickOutcome {
        self.ticks = self.ticks.saturating_add(1);
        self.last_phase = Some(phase.to_string());
        // Production `BootReporter::tick` blocks up to ~100 ms on
        // `poll_key`, which gives the wait loop its natural
        // cadence. A bare counting sink would busy-spin and
        // generate millions of ticks per second, so simulate the
        // same cadence here. Tests asserting on tick counts can
        // then trust wallclock math.
        std::thread::sleep(POLL_INTERVAL);
        TickOutcome::Continue
    }
}

/// ProgressSink that aborts on the Nth tick. Used to drive
/// `wait_for` into the OperatorAborted path without standing up a
/// full TUI console.
struct AbortingSink {
    ticks: u32,
    abort_at: u32,
}

impl AbortingSink {
    fn at(abort_at: u32) -> Self {
        Self { ticks: 0, abort_at }
    }
}

impl ProgressSink for AbortingSink {
    fn tick(&mut self, _phase: &str) -> TickOutcome {
        self.ticks = self.ticks.saturating_add(1);
        if self.ticks >= self.abort_at {
            TickOutcome::Aborted
        } else {
            TickOutcome::Continue
        }
    }
}

#[test]
fn wait_for_missing_path_times_out() {
    let missing = Path::new("/nonexistent/path/nmbl-devices-test");
    let err = wait_for(missing, Duration::from_millis(200), "waiting for", None)
        .expect_err("missing path must time out");
    match err {
        NmblError::DeviceTimeout { device, timeout_ms } => {
            assert_eq!(device, missing.to_path_buf());
            assert_eq!(timeout_ms, 200);
        }
        other => panic!("expected DeviceTimeout, got {other:?}"),
    }
}

#[test]
fn wait_for_dev_null_returns_quickly() {
    let dev_null = Path::new("/dev/null");
    if !dev_null.exists() {
        eprintln!("skipping: /dev/null missing in this sandbox");
        return;
    }
    let start = Instant::now();
    wait_for(dev_null, Duration::from_secs(1), "waiting for", None)
        .expect("/dev/null should be ready");
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "wait_for(/dev/null) took {elapsed:?}, expected <1s",
    );
}

#[test]
fn wait_for_ticks_progress_sink_during_wait() {
    // A 500 ms timeout on a 100 ms poll cadence should fire ~5 ticks
    // (give or take one for scheduler jitter). The exact bound is
    // intentionally loose — CI VMs run hot.
    let missing = Path::new("/nonexistent/nmbl-devices-tick-test");
    let mut sink = CountingSink::new();
    let _ = wait_for(
        missing,
        Duration::from_millis(500),
        "waiting for",
        Some(&mut sink),
    )
    .expect_err("missing path must time out");
    assert!(
        sink.ticks >= 2,
        "expected at least 2 ticks during a 500 ms wait, got {}",
        sink.ticks
    );
    assert!(
        sink.ticks <= 15,
        "expected at most 15 ticks during a 500 ms wait (defensive upper bound), got {}",
        sink.ticks
    );
}

#[test]
fn wait_for_returns_operator_aborted_when_sink_aborts() {
    // First tick fires Aborted; wait_for must surface that as
    // NmblError::OperatorAborted carrying the device-context string
    // so the emergency menu can show "waiting for <device>".
    let missing = Path::new("/nonexistent/nmbl-devices-abort-test");
    let mut sink = AbortingSink::at(1);
    let err = wait_for(
        missing,
        Duration::from_secs(30),
        "phase 3b: waiting for",
        Some(&mut sink),
    )
    .expect_err("aborting sink must abort the wait");

    match err {
        NmblError::OperatorAborted { context } => {
            assert!(
                context.contains("nmbl-devices-abort-test"),
                "context must name the device being waited on: {context:?}"
            );
            assert!(
                context.starts_with("waiting for"),
                "context must lead with the action verb: {context:?}"
            );
        }
        other => panic!("expected OperatorAborted, got {other:?}"),
    }
    assert_eq!(
        sink.ticks, 1,
        "wait_for must surface the abort after exactly one tick"
    );
}

#[test]
fn wait_for_phase_string_includes_target_elapsed_and_timeout() {
    // Wait long enough for at least one whole-second tick to fire so
    // the elapsed counter increments off zero.
    let missing = Path::new("/nonexistent/nmbl-devices-phase-test");
    let mut sink = CountingSink::new();
    let _ = wait_for(
        missing,
        Duration::from_millis(1100),
        "phase 3b: waiting for",
        Some(&mut sink),
    )
    .expect_err("missing path must time out");

    let phase = sink
        .last_phase
        .as_deref()
        .expect("at least one tick must fire during a 1.1 s wait");
    assert!(
        phase.starts_with("phase 3b: waiting for"),
        "phase string must lead with the operation verb + phase context: {phase:?}"
    );
    assert!(
        phase.contains("nmbl-devices-phase-test"),
        "phase string must name the target device: {phase:?}"
    );
    assert!(
        phase.contains("/ 1s)"),
        "phase string must include timeout in seconds: {phase:?}"
    );
}

#[test]
fn format_wait_phase_renders_canonical_shape() {
    // Lock the visible format so a downstream activation-wait caller
    // can rely on the exact string the operator greps for.
    let phase = format_wait_phase(
        "phase 3b: waiting for",
        &"/dev/disk/by-uuid/abc",
        Duration::from_secs(12),
        Duration::from_secs(30),
    );
    assert_eq!(
        phase,
        "phase 3b: waiting for /dev/disk/by-uuid/abc (12s / 30s)"
    );
}

#[test]
fn resolve_mountpoint_is_root_overrides() {
    let root = PathBuf::from("/mnt/system");
    let entry = fs_entry("/dev/sda1", "/whatever", true);
    assert_eq!(resolve_mountpoint(&root, &entry), root);
}

#[test]
fn resolve_mountpoint_relative_is_joined() {
    let root = PathBuf::from("/mnt/system");
    let entry = fs_entry("/dev/sda1", "boot", false);
    assert_eq!(
        resolve_mountpoint(&root, &entry),
        PathBuf::from("/mnt/system/boot"),
    );
}

#[test]
fn resolve_mountpoint_absolute_already_under_root_kept() {
    let root = PathBuf::from("/mnt/system");
    let entry = fs_entry("/dev/sda1", "/mnt/system/boot", false);
    assert_eq!(
        resolve_mountpoint(&root, &entry),
        PathBuf::from("/mnt/system/boot"),
    );
}

#[test]
fn resolve_mountpoint_absolute_not_under_root_joined() {
    let root = PathBuf::from("/mnt/system");
    let entry = fs_entry("/dev/sda1", "/boot", false);
    assert_eq!(
        resolve_mountpoint(&root, &entry),
        PathBuf::from("/mnt/system/boot"),
    );
}
