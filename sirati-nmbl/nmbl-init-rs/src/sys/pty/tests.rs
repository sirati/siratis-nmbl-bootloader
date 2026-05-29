#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]

use std::os::fd::AsFd;
use std::path::PathBuf;

use super::{preflight_shell, spawn_shell};
use crate::error::NmblError;

#[test]
fn preflight_shell_rejects_missing_binary() {
    // Regression for the "Raw Shell does nothing" bug: in
    // external-rescue mode the initramfs ships no /bin/sh, so the
    // forked child execve'd nothing and silently _exit(127)'d. The
    // preflight must turn a missing/non-exec shell into an Err the
    // emergency UI can surface, not a healthy-looking fork.
    let missing = PathBuf::from("/definitely/not/here/bin/sh");
    let err = preflight_shell(&missing).expect_err("missing shell must error");
    assert!(matches!(err, NmblError::Tui { .. }), "got {err:?}");
}

#[test]
fn preflight_shell_accepts_executable() {
    // A real executable on the host passes. Skip if /bin/sh is
    // absent (extremely sandboxed CI), trying /bin/echo as a fallback.
    for cand in ["/bin/sh", "/bin/echo", "/usr/bin/env"] {
        let p = PathBuf::from(cand);
        if std::fs::metadata(&p).is_ok() {
            preflight_shell(&p).expect("executable must pass preflight");
            return;
        }
    }
}

/// Spawning `/bin/echo` (which is not a shell) is enough to verify
/// fork/execve + master-fd readback work end-to-end without
/// depending on a `/bin/sh` that varies across CI images. The child
/// writes a short line and exits; the parent reads the bytes back
/// from the master and reaps the child via `try_wait`.
#[test]
fn spawn_shell_basic_roundtrip() {
    // Skip on extremely sandboxed test envs where /bin/echo doesn't
    // exist — the test depends on a real executable to fork into.
    let echo = PathBuf::from("/bin/echo");
    if std::fs::metadata(&echo).is_err() {
        return;
    }
    let child = match spawn_shell(&echo, 80, 24) {
        Ok(c) => c,
        // Sandboxes that block fork or openpty return EPERM/ENOTTY.
        Err(_) => return,
    };

    // The PTY master is non-blocking. Drain until the child exits.
    // Cap iterations so the test cannot hang on a hostile sandbox.
    let mut buf = Vec::new();
    let mut tmp = [0u8; 256];
    let raw = child.master.as_fd();
    let mut reaped = false;
    for _ in 0..1000 {
        match rustix::io::read(raw, &mut tmp) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(tmp.get(..n).unwrap_or(&[])),
            Err(rustix::io::Errno::AGAIN) => {
                std::thread::yield_now();
            }
            Err(_) => break,
        }
        if !reaped {
            if let Ok(Some(_)) = child.try_wait() {
                reaped = true;
            }
        } else if buf.contains(&b'i') {
            // Got the 'i' from "hi" — enough evidence the pipe
            // works. Stop reading to keep the test bounded.
            break;
        }
    }
    let s = String::from_utf8_lossy(&buf);
    assert!(s.contains("hi"), "expected 'hi' in PTY output, got {s:?}");
    // Best-effort reap if try_wait above missed the exit window.
    child.terminate();
}
