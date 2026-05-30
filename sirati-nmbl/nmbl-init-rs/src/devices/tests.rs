#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests can panic on assertion failure"
)]

use super::*;
use crate::ui::TickOutcome;
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

/// Counting ProgressSink for tests. Records every `render_phase` call
/// and the most recent phase string so we can assert both the cadence
/// (~N renders per second of wait, driven by `poll_abort`'s timeout) and
/// the format of the status line. `abort` injects an Esc: when set,
/// `poll_abort` resolves `true` so `wait_for` returns the abort outcome.
struct CountingSink {
    ticks: u32,
    last_phase: Option<String>,
    abort: bool,
}

impl CountingSink {
    fn new() -> Self {
        Self {
            ticks: 0,
            last_phase: None,
            abort: false,
        }
    }

    /// A sink that reports an operator Esc on the first abort poll.
    fn aborting() -> Self {
        Self {
            ticks: 0,
            last_phase: None,
            abort: true,
        }
    }
}

impl ProgressSink for CountingSink {
    fn tick(&mut self, _phase: &str) -> TickOutcome {
        // The async `wait_for` no longer calls `tick` (no blocking input
        // poll during a device wait); it renders via `render_phase` and
        // races `poll_abort`.
        TickOutcome::Continue
    }

    fn render_phase(&mut self, phase: &str) {
        self.ticks = self.ticks.saturating_add(1);
        self.last_phase = Some(phase.to_string());
    }

    fn poll_abort(
        &mut self,
        timeout: Duration,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + '_>> {
        let abort = self.abort;
        Box::pin(async move {
            if abort {
                // Inject the Esc immediately so the wait aborts without
                // burning a cadence slice.
                return true;
            }
            // No abort: provide the inter-poll cadence the production
            // backend's `poll_event` timeout would, so the render-count
            // assertions still hold.
            tokio::time::sleep(timeout).await;
            false
        })
    }
}

/// Build a single-thread `LocalRuntime` to drive the async `wait_for`
/// in tests — mirrors the production interactive runtime.
fn block<F: std::future::Future>(fut: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build_local(tokio::runtime::LocalOptions::default())
        .expect("test runtime");
    rt.block_on(fut)
}

#[test]
fn wait_for_missing_path_times_out() {
    let missing = Path::new("/nonexistent/path/nmbl-devices-test");
    let err = block(wait_for(
        missing,
        Duration::from_millis(200),
        "waiting for",
        None,
    ))
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
    block(wait_for(
        dev_null,
        Duration::from_secs(1),
        "waiting for",
        None,
    ))
    .expect("/dev/null should be ready");
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "wait_for(/dev/null) took {elapsed:?}, expected <1s",
    );
}

#[test]
fn wait_for_ticks_progress_sink_during_wait() {
    // A 500 ms timeout on a 100 ms poll cadence should fire ~5 renders
    // (give or take one for scheduler jitter). The exact bound is
    // intentionally loose — CI VMs run hot.
    let missing = Path::new("/nonexistent/nmbl-devices-tick-test");
    let mut sink = CountingSink::new();
    let _ = block(wait_for(
        missing,
        Duration::from_millis(500),
        "waiting for",
        Some(&mut sink),
    ))
    .expect_err("missing path must time out");
    assert!(
        sink.ticks >= 2,
        "expected at least 2 renders during a 500 ms wait, got {}",
        sink.ticks
    );
    assert!(
        sink.ticks <= 15,
        "expected at most 15 renders during a 500 ms wait (defensive upper bound), got {}",
        sink.ticks
    );
}

#[test]
fn wait_for_esc_aborts_the_wait() {
    // An injected Esc (via the aborting sink) must short-circuit a still-
    // missing device with `OperatorAborted` carrying the device context —
    // not run the full timeout out to `DeviceTimeout`.
    let missing = Path::new("/nonexistent/nmbl-devices-abort-test");
    let mut sink = CountingSink::aborting();
    let start = Instant::now();
    let err = block(wait_for(
        missing,
        Duration::from_secs(30),
        "phase 3b: waiting for",
        Some(&mut sink),
    ))
    .expect_err("an injected Esc must abort the wait");
    match err {
        NmblError::OperatorAborted { context } => {
            assert!(
                context.contains("nmbl-devices-abort-test"),
                "abort context must name the device being waited on: {context:?}"
            );
        }
        other => panic!("expected OperatorAborted, got {other:?}"),
    }
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "Esc abort must return promptly, not after the 30s timeout",
    );
}

#[test]
fn wait_for_phase_string_includes_target_elapsed_and_timeout() {
    // Wait long enough for at least one whole-second render to fire so
    // the elapsed counter increments off zero.
    let missing = Path::new("/nonexistent/nmbl-devices-phase-test");
    let mut sink = CountingSink::new();
    let _ = block(wait_for(
        missing,
        Duration::from_millis(1100),
        "phase 3b: waiting for",
        Some(&mut sink),
    ))
    .expect_err("missing path must time out");

    let phase = sink
        .last_phase
        .as_deref()
        .expect("at least one render must fire during a 1.1 s wait");
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

#[test]
fn loop_backed_when_options_carry_loop() {
    // The `loop` mount pseudo-option marks an entry loop-backed even
    // when the resolved device path does not exist (it is created by the
    // image builder; at config time only the option is authoritative).
    let mut entry = fs_entry("/.nix-image/nix.sqfs", "/nix", false);
    entry.fstype = "squashfs".to_string();
    entry.options = "loop,ro".to_string();
    assert!(entry_is_loop_backed(
        &entry,
        Path::new("/nonexistent/nmbl-loop-test.sqfs"),
    ));
}

#[test]
fn loop_backed_detected_for_regular_file_without_option() {
    // A regular-file device with no explicit `loop` option is still
    // loop-backed: the resolved path stats as a regular file.
    let dir = tempfile::tempdir().expect("tempdir");
    let img = dir.path().join("image.sqfs");
    std::fs::write(&img, b"squashfs-placeholder").expect("write image");
    let mut entry = fs_entry(&img.display().to_string(), "/nix", false);
    entry.fstype = "squashfs".to_string();
    assert!(entry_is_loop_backed(&entry, &img));
}

#[test]
fn not_loop_backed_for_block_device_node() {
    // A real block/char device node (e.g. /dev/null is char) with no
    // `loop` option must NOT be treated as loop-backed.
    let dev_null = Path::new("/dev/null");
    if !dev_null.exists() {
        eprintln!("skipping: /dev/null missing in this sandbox");
        return;
    }
    let entry = fs_entry("/dev/null", "/x", false);
    assert!(!entry_is_loop_backed(&entry, dev_null));
}
