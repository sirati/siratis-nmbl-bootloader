//! Panic hook with `execve`-into-recovery flow (PLAN.md §10).
//!
//! Why re-exec instead of just unwinding: a panicking PID 1 has unknown
//! state (terminal raw-mode, partial mounts, leaked console fd).
//! `execve(2)` resets the process — clean stack, default termios after
//! the kernel re-opens the controlling tty, fresh fd table — and
//! preserves the PID, so we land in a known state without the kernel
//! panicking. `main` notices the `--errored=<path>` argv and routes
//! straight to [`crate::shell::drop_to_emergency`].
//!
//! On any failure (write fails, execve fails) the hook does its best
//! to print to stderr and then `_exit(1)`s; that becomes a kernel
//! panic for PID 1, which is the documented worst case.

use std::ffi::CString;
use std::fs::File;
use std::io::Write as _;
use std::panic::PanicHookInfo;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

use nix::unistd::execve;

/// Fallback location for the panic report used when
/// [`install_panic_hook`] hasn't been called yet (e.g. an early panic
/// before main installs the hook with the configured directory). `/run`
/// is tmpfs — created in phase 1 — so a re-exec for panic recovery has
/// somewhere to write even when the system filesystem isn't mounted.
pub const DEFAULT_PANIC_REPORT_DIR: &str = "/run";

/// Process-wide override for the panic report directory. Populated by
/// [`install_panic_hook`] from the runtime config so the panic hook —
/// which is a `'static + Send + Sync` closure with no captured state —
/// can still honour the operator's choice. An `RwLock` (rather than a
/// `OnceLock`) so a second `install_panic_hook` call genuinely replaces
/// the first value: bootstrap mode installs once against the recovery
/// default before Phase 0.5, then again with the real config after
/// `run_bootstrap_phase` returns.
static PANIC_REPORT_DIR: RwLock<Option<PathBuf>> = RwLock::new(None);

/// Path to the on-disk self for `execve(2)`. The kernel resolves this
/// to the binary that is currently mapped into the panicking process,
/// which is always the right thing to re-exec.
const SELF_EXE: &str = "/proc/self/exe";

/// The argv0 we hand the re-exec'd process. Matches the package name
/// so `ps` output stays consistent across the re-exec.
const ARGV0: &str = "nmbl-init";

/// Install the process-wide panic hook. Must run early in startup,
/// before any other work, so a bug in early phases (config load,
/// mount, …) is still caught by the recovery path.
///
/// `report_dir` is the directory where the structured panic report
/// will be written. This is recorded in a process-wide `RwLock` so
/// the `'static` panic-hook closure can pick it up without capturing
/// non-`'static` config state.
///
/// **Idempotent / replace semantics.** Each call replaces the stored
/// directory. Bootstrap mode relies on this: `main` installs once with
/// the recovery default before `run_bootstrap_phase` (so an early panic
/// still produces a report), then re-installs after Phase 0.5 returns
/// the operator's real `Config`, so the remainder of the boot honours
/// `general.panic_report_dir` from `/boot/.../config.toml`. A poisoned
/// lock is treated as a no-op — the previous directory keeps applying,
/// which is the safer behaviour for a hook that may run from the same
/// thread that poisoned the lock.
pub fn install_panic_hook(report_dir: &Path) {
    if let Ok(mut w) = PANIC_REPORT_DIR.write() {
        *w = Some(report_dir.to_path_buf());
    }
    std::panic::set_hook(Box::new(|info: &PanicHookInfo<'_>| {
        let report = build_report(info);
        let pid = std::process::id();
        let report_path = report_path_for(pid);

        // Always print to stderr first; the file write is best-effort.
        eprintln!("[nmbl] PANIC HOOK ENGAGED");
        eprintln!("{report}");

        let written_path = match write_report(&report_path, &report) {
            Ok(()) => Some(report_path),
            Err(err) => {
                eprintln!(
                    "[nmbl] failed to write panic report to {}: {err}",
                    report_path.display()
                );
                None
            }
        };

        try_reexec(written_path);

        // execve failed — print one last line and _exit. If we are PID
        // 1 the kernel will panic, which is the documented worst case.
        eprintln!("[nmbl] re-exec into recovery failed; halting process");
        // SAFETY: libc::_exit is async-signal-safe and unconditionally
        // terminates the process; no safe wrapper exists (rustix #844).
        unsafe { libc::_exit(1) };
    }));
}

/// Format the structured panic report. Pure function so the hook
/// itself stays short and the report shape is testable.
fn build_report(info: &PanicHookInfo<'_>) -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let location = match info.location() {
        Some(loc) => format!("{}:{}:{}", loc.file(), loc.line(), loc.column()),
        None => "<unknown>".to_string(),
    };
    let payload = panic_payload_str(info);
    format!("nmbl-init panic at unix-seconds={ts}\n  location: {location}\n  payload:  {payload}\n",)
}

/// Extract the panic payload as a `&str`. `set_hook`'s payload is
/// `&dyn Any` — by convention it's `&'static str` (from `panic!("...")`)
/// or `String` (from `panic!("{x}")` formatting). Anything else
/// collapses to a synthetic placeholder.
fn panic_payload_str(info: &PanicHookInfo<'_>) -> String {
    if let Some(s) = info.payload().downcast_ref::<&'static str>() {
        return (*s).to_string();
    }
    if let Some(s) = info.payload().downcast_ref::<String>() {
        return s.clone();
    }
    "<non-string panic payload>".to_string()
}

/// Per-PID report path under `dir`. Encoded into the file name so
/// concurrent crashes (extremely unlikely for PID 1) don't clobber
/// each other.
fn report_path_in(dir: &Path, pid: u32) -> PathBuf {
    dir.join(format!("nmbl-panic-{pid}.txt"))
}

/// Per-PID report path. Reads the configured directory from the
/// [`PANIC_REPORT_DIR`] `RwLock`, falling back to
/// [`DEFAULT_PANIC_REPORT_DIR`] when the hook hasn't been installed
/// yet or the lock is poisoned.
fn report_path_for(pid: u32) -> PathBuf {
    let dir = PANIC_REPORT_DIR
        .read()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_else(|| PathBuf::from(DEFAULT_PANIC_REPORT_DIR));
    report_path_in(&dir, pid)
}

/// Write the report to disk. Best-effort: the caller logs the failure
/// and continues to the re-exec step.
fn write_report(path: &std::path::Path, report: &str) -> std::io::Result<()> {
    let mut f = File::create(path)?;
    f.write_all(report.as_bytes())?;
    f.flush()
}

/// Attempt the re-exec. Argv is `[ARGV0, "--errored=<path>"]` when a
/// report was persisted, else `[ARGV0, "--errored=<missing>"]` so the
/// recovery side still knows we crashed even if the report write
/// failed. On execve success the function does not return; on failure
/// it returns and the hook proceeds to `_exit`.
fn try_reexec(report_path: Option<PathBuf>) {
    let path_str = match report_path {
        Some(p) => p.display().to_string(),
        None => "<missing>".to_string(),
    };
    let arg = format!("--errored={path_str}");

    let exe_c = match CString::new(SELF_EXE) {
        Ok(c) => c,
        Err(_) => return,
    };
    let argv0_c = match CString::new(ARGV0) {
        Ok(c) => c,
        Err(_) => return,
    };
    let arg_c = match CString::new(arg) {
        Ok(c) => c,
        Err(_) => return,
    };

    let argv: [&CString; 2] = [&argv0_c, &arg_c];
    let env: [&CString; 0] = [];
    // execve safety: panic-recovery re-exec of our own binary to resume init with a clean stack.
    let _ = execve(&exe_c, &argv, &env);
    // Fall through — execve only returns on failure.
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;

    #[test]
    fn report_path_uses_pid() {
        // `report_path_for` reads the process-wide OnceLock — which
        // tests must not poison — so check the deterministic helper
        // instead. Confirms the default-fallback shape.
        let p = report_path_in(Path::new(DEFAULT_PANIC_REPORT_DIR), 42);
        assert_eq!(p, PathBuf::from("/run/nmbl-panic-42.txt"));
    }

    #[test]
    fn report_path_honours_configured_dir() {
        // Operator overrides the panic dir to e.g. /var/run/nmbl via
        // boot.nmbl.panicReportDir; the filename pattern stays.
        let dir = PathBuf::from("/var/run/nmbl");
        let p = report_path_in(&dir, 1);
        assert_eq!(p, PathBuf::from("/var/run/nmbl/nmbl-panic-1.txt"));
    }

    #[test]
    fn install_panic_hook_replaces_stored_dir() {
        // Bootstrap mode installs twice: first with the recovery default
        // before run_bootstrap_phase, then with the operator's real
        // panic_report_dir after Phase 0.5. The second call must
        // overwrite the first — `report_path_for` must see the latest
        // value, not the first one.
        //
        // Touches global state (PANIC_REPORT_DIR and the panic hook), so
        // it cannot run in parallel with other tests that also poke
        // those; cargo test serialises by module via `--test-threads`
        // when needed. We restore the original hook on exit.
        let prev_hook = std::panic::take_hook();
        install_panic_hook(Path::new("/tmp/first"));
        assert_eq!(
            report_path_for(7),
            PathBuf::from("/tmp/first/nmbl-panic-7.txt"),
        );
        install_panic_hook(Path::new("/tmp/second"));
        assert_eq!(
            report_path_for(7),
            PathBuf::from("/tmp/second/nmbl-panic-7.txt"),
        );
        std::panic::set_hook(prev_hook);
    }

    #[test]
    fn build_report_contains_location_when_caught() {
        // `catch_unwind` lets us grab a real PanicHookInfo by way of
        // a custom hook — the hook captures `info.location()` and the
        // payload string into a shared cell.
        use std::panic::{AssertUnwindSafe, catch_unwind, set_hook, take_hook};
        use std::sync::Mutex;
        use std::sync::OnceLock;

        static CAPTURED: OnceLock<Mutex<Option<String>>> = OnceLock::new();
        let cell = CAPTURED.get_or_init(|| Mutex::new(None));
        {
            // Reset for re-runs in the same test binary.
            if let Ok(mut g) = cell.lock() {
                *g = None;
            }
        }

        let prev = take_hook();
        set_hook(Box::new(|info| {
            let report = build_report(info);
            if let Some(cell) = CAPTURED.get()
                && let Ok(mut g) = cell.lock()
            {
                *g = Some(report);
            }
        }));

        let _ = catch_unwind(AssertUnwindSafe(|| {
            panic!("synthetic panic for test");
        }));
        set_hook(prev);

        let got = cell
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .expect("hook should have captured a report");
        assert!(got.contains("synthetic panic for test"), "{got}");
        assert!(got.contains("location:"), "{got}");
        assert!(got.contains("unix-seconds="), "{got}");
    }
}
