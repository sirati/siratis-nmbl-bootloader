use std::path::{Path, PathBuf};

use super::{EXEC_FAILED_EXIT_CODE, ProcessOutcome, run, run_capture_blocking};
use crate::error::Result;

/// Drive an async runner future on a single-thread `LocalRuntime` with
/// the reserve poller spawned — exactly the production interactive
/// shape. The closure receives the poller's `LocalSender` so it can hand
/// it to `run`/`run_capture`, whose reaps go through the poller's
/// non-blocking `waitpid(WNOHANG)` op.
fn with_runner<F, Fut>(build: F) -> Result<(ProcessOutcome, Vec<u8>)>
where
    F: FnOnce(crate::sys::poller::LocalSender) -> Fut,
    Fut: std::future::Future<Output = Result<(ProcessOutcome, Vec<u8>)>>,
{
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build_local(tokio::runtime::LocalOptions::default())
        .expect("test runtime");
    rt.block_on(async move {
        let (poller, sender) = crate::sys::poller::build();
        tokio::task::spawn_local(poller.run_with(crate::sys::poller::TokioPacer));
        build(sender).await
    })
}

/// Async `run` helper that discards the (empty) capture buffer.
fn run_via_poller(
    binary: &Path,
    argv: &[String],
    stdin_data: Option<&[u8]>,
) -> Result<ProcessOutcome> {
    let binary = binary.to_path_buf();
    let argv = argv.to_vec();
    let stdin_owned = stdin_data.map(<[u8]>::to_vec);
    with_runner(move |sender| async move {
        let outcome = run(&binary, &argv, stdin_owned.as_deref(), &sender).await?;
        Ok((outcome, Vec::new()))
    })
    .map(|(outcome, _)| outcome)
}

/// Locate a binary on disk. We can't rely on `/bin/<x>` existing
/// inside a `nix develop` shell (which often has an almost-empty
/// `/bin`), so we resolve via `PATH` ourselves — keeping the test
/// dependency surface to just the standard library.
fn which(name: &str) -> Option<PathBuf> {
    let path_env = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_env) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[test]
fn true_exits_zero() {
    let Some(bin) = which("true") else {
        eprintln!("skipping: `true` not found on PATH");
        return;
    };
    let out = run_via_poller(&bin, &[], None).expect("run /usr/bin/true");
    assert!(out.normal_exit, "true should exit normally");
    assert_eq!(out.exit_code, 0);
}

#[test]
fn false_exits_one() {
    let Some(bin) = which("false") else {
        eprintln!("skipping: `false` not found on PATH");
        return;
    };
    let out = run_via_poller(&bin, &[], None).expect("run /usr/bin/false");
    assert!(out.normal_exit, "false should exit normally");
    assert_eq!(out.exit_code, 1);
}

#[test]
fn cat_consumes_piped_stdin() {
    let Some(bin) = which("cat") else {
        eprintln!("skipping: `cat` not found on PATH");
        return;
    };
    let out = run_via_poller(&bin, &[], Some(b"hello\n")).expect("run cat with stdin");
    assert!(out.normal_exit, "cat should exit normally");
    assert_eq!(out.exit_code, 0);
}

#[test]
fn missing_binary_yields_127() {
    let bogus = PathBuf::from("/nonexistent/path/xyz-nmbl-activation-test");
    let out = run_via_poller(&bogus, &[], None).expect("run should report, not error");
    assert!(out.normal_exit, "missing-binary path uses _exit(127)");
    assert_eq!(
        out.exit_code, EXEC_FAILED_EXIT_CODE,
        "execve failure must surface as 127"
    );
}

#[test]
fn capture_echo_returns_stdout_bytes() {
    let Some(bin) = which("echo") else {
        eprintln!("skipping: `echo` not found on PATH");
        return;
    };
    let (outcome, captured) = run_capture_blocking(&bin, &["hello".to_string()]).expect("run echo");
    assert!(outcome.normal_exit);
    assert_eq!(outcome.exit_code, 0);
    // `echo` appends a newline; we should see it.
    assert_eq!(captured, b"hello\n");
}

#[test]
fn capture_missing_binary_yields_127_and_empty_buffer() {
    let bogus = PathBuf::from("/nonexistent/path/xyz-nmbl-capture-test");
    let (outcome, captured) = run_capture_blocking(&bogus, &[]).expect("run_capture should report");
    assert!(outcome.normal_exit, "missing-binary path uses _exit(127)");
    assert_eq!(outcome.exit_code, EXEC_FAILED_EXIT_CODE);
    assert!(
        captured.is_empty(),
        "no stdout should have been produced by a missing binary",
    );
}

#[test]
fn capture_handles_payload_larger_than_pipe_buffer() {
    // A payload bigger than the 4 KiB pipe read buffer exercises
    // the multi-iteration path in `read_all`. We use `printf` to
    // emit a deterministic string of known size.
    let Some(bin) = which("printf") else {
        eprintln!("skipping: `printf` not found on PATH");
        return;
    };
    // 10 000 'x' characters, no trailing newline.
    let pattern = "x".repeat(10_000);
    let (outcome, captured) =
        run_capture_blocking(&bin, &["%s".to_string(), pattern.clone()]).expect("run printf");
    assert!(outcome.normal_exit);
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(captured.len(), pattern.len());
    assert!(captured.iter().all(|b| *b == b'x'));
}
