//! Chrooted external-rescue child runner (Phase 4b).
//!
//! Runs the EXTERNAL full-system squashfs path. Rather than detaching
//! the initramfs root and replacing PID 1, NMBL stays PID 1 on the
//! initramfs rootfs and runs the rescue system as a **chrooted child**:
//!
//! 1. Pre-fork (PID 1, safe nix wrappers): create the chroot's
//!    `/nmbl-root` + `/mnt` and PID 1's own `/mnt`, set up a shared
//!    subtree so the child's `/mnt` mounts propagate back to PID 1, and
//!    bind NMBL's root into the chroot at `/nmbl-root` (so the root-only
//!    TUI socket is reachable at `/nmbl-root/nmbl-run/tui.sock`).
//! 2. `fork()`; the child (async-signal-safe only — mirrors
//!    `sys::pty`) `chroot`s into the rescue overlay, opens
//!    `/dev/console` onto stdio, and `execve`s the rescue entrypoint.
//! 3. PID 1 reaps the child via the poller's non-blocking
//!    `waitpid(WNOHANG)` op, CONCURRENTLY with the remote-attach server
//!    so an operator can attach over the socket while the rescue child
//!    runs. On child exit the bind mounts are torn down (lazy
//!    `MNT_DETACH`) and control returns to the recovery flow.
//! 4. When a remote `nmbl` session inside the rescue commits an action
//!    (boot a generation, reboot, ...), the rescue is stopped, synced and
//!    unmounted, and that action is performed instead.

use std::ffi::CString;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;

use nix::sys::wait::WaitStatus;
use nix::unistd::{ForkResult, Pid, fork};

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::sys::poller::{LocalSender, reap_child};
use crate::terminal::TerminalAction;
use crate::{nmbl_info, nmbl_warn};

/// Where the writable rescue overlay is staged (mirrors
/// `disk::RESCUE_MOUNT`). The chroot target.
const RESCUE_ROOT: &str = "/rescue";
#[path = "child_mounts.rs"]
mod mounts;
#[path = "child_ready_paths.rs"]
mod ready_paths;
#[path = "child_stop.rs"]
mod stop;
#[cfg(test)]
use mounts::{MountStep, child_boot_target, mount_plan, preserved_roots, umount_plan};
use mounts::{apply_mount_plan, teardown_mounts};
pub(crate) use mounts::reveal_installed_mounts;

/// Conventional exit code surfaced when the post-fork `execve(2)` (or a
/// pre-exec syscall) fails in the child. Matches `sys::pty`.
const EXEC_FAILED_EXIT_CODE: i32 = 127;

/// The CStrings the chrooted child needs, all allocated in the PARENT
/// before `fork` (the child path is async-signal-safe and must not
/// allocate). `argv0` is the entrypoint basename; the env is the
/// minimal `TERM` + `PATH` + `NMBL_TUI_SOCK` set the rescue-sfs
/// contract expects.
pub(crate) struct ChildExec {
    path_c: CString,
    argv0_c: CString,
    env_term: CString,
    env_path: CString,
    env_sock: CString,
    env_ready: Option<CString>,
    ready_read_fd: Option<i32>,
    ready_write_fd: Option<i32>,
}

impl ChildExec {
    /// Build the exec strings for `entrypoint` (inside the chroot, e.g.
    /// `/init`). Returns the basename-as-argv0 and the env triple. Pure
    /// apart from the CString allocations, so the argv/env shape is
    /// directly unit-testable.
    pub(crate) fn build(entrypoint: &Path) -> Result<Self> {
        let nul = |what: &str| NmblError::Rescue {
            stage: "rescue-child-exec",
            source: Box::new(NmblError::ConfigInvalid {
                reason: format!("{what} contains interior NUL"),
                context: format!("preparing chrooted execve of {}", entrypoint.display()),
            }),
        };
        let entry_bytes = entrypoint.as_os_str().as_encoded_bytes();
        let path_c = CString::new(entry_bytes).map_err(|_| nul("rescue entrypoint path"))?;
        let argv0_bytes: Vec<u8> = entrypoint
            .file_name()
            .map(|n| n.as_encoded_bytes().to_vec())
            .unwrap_or_else(|| entry_bytes.to_vec());
        let argv0_c = CString::new(argv0_bytes).map_err(|_| nul("rescue argv0"))?;
        let env_term = CString::new("TERM=linux").map_err(|_| nul("TERM env"))?;
        let env_path =
            CString::new("PATH=/bin:/sbin:/usr/bin:/usr/sbin").map_err(|_| nul("PATH env"))?;
        // The chroot-relative socket path the rescue /init re-exports and
        // the nmbl-tui shim honours (rescue-sfs.nix contract).
        let env_sock = CString::new("NMBL_TUI_SOCK=/nmbl-root/nmbl-run/tui.sock")
            .map_err(|_| nul("NMBL_TUI_SOCK env"))?;
        Ok(Self {
            path_c,
            argv0_c,
            env_term,
            env_path,
            env_sock,
            env_ready: None,
            ready_read_fd: None,
            ready_write_fd: None,
        })
    }
}

/// Post-fork child path: chroot into the rescue overlay, become a
/// session leader, wire `/dev/console` onto stdio, and `execve` the
/// rescue entrypoint. Restricted to async-signal-safe primitives —
/// mirrors `sys::pty::spawn::child_exec_on_pty`.
///
/// # Safety
/// Must only be called from the child branch of `fork()`. No allocation,
/// no Rust I/O, no destructors. All `CString`s were built in the parent.
unsafe fn child_chroot_exec(exec: &ChildExec) -> ! {
    if let Some(fd) = exec.ready_write_fd {
        // SAFETY: only the owned pipe writer may cross this child's exec.
        // fcntl is async-signal-safe; never clear flags on a requester fd.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, 0) } < 0 {
            unsafe { libc::_exit(EXEC_FAILED_EXIT_CODE) };
        }
    }

    if let Some(fd) = exec.ready_read_fd {
        // SAFETY: the child must not retain the parent's readiness reader.
        let _ = unsafe { libc::close(fd) };
    }
    // chroot into the writable rescue overlay, then anchor cwd at the
    // new root. setsid() detaches from PID 1's session so the rescue
    // /init owns a fresh session for its console.
    // SAFETY: libc::chroot/chdir/setsid are async-signal-safe. On any
    // failure we _exit(127) — the parent observes the non-zero status.
    if unsafe { libc::chroot(c"/rescue".as_ptr()) } != 0 {
        // SAFETY: post-fork child; _exit is the only correct primitive.
        unsafe { libc::_exit(EXEC_FAILED_EXIT_CODE) };
    }
    if unsafe { libc::chdir(c"/".as_ptr()) } != 0 {
        unsafe { libc::_exit(EXEC_FAILED_EXIT_CODE) };
    }
    // setsid failure is non-fatal: the entrypoint still runs, it merely
    // lacks a fresh session.
    let _ = unsafe { libc::setsid() };

    // Open /dev/console (now the chroot's) and dup it onto 0/1/2 so the
    // rescue system's stdio reaches the operator's primary console.
    // SAFETY: libc::open is async-signal-safe; O_RDWR for read+write.
    let console_fd = unsafe { libc::open(c"/dev/console".as_ptr(), libc::O_RDWR) };
    if console_fd >= 0 {
        for target in [0, 1, 2] {
            // SAFETY: dup2 is async-signal-safe; atomically replaces fd.
            if unsafe { libc::dup2(console_fd, target) } < 0 {
                unsafe { libc::_exit(EXEC_FAILED_EXIT_CODE) };
            }
        }
        if console_fd > 2 {
            // SAFETY: close on a valid fd; async-signal-safe.
            let _ = unsafe { libc::close(console_fd) };
        }
    }
    // A missing /dev/console is non-fatal here: the rescue /init mounts
    // its own devtmpfs and reopens the console itself; stranding the
    // operator by _exit'ing would be worse than inheriting PID 1's fds.

    let argv: [*const libc::c_char; 2] = [exec.argv0_c.as_ptr(), std::ptr::null()];
    let envp: [*const libc::c_char; 5] = [
        exec.env_term.as_ptr(),
        exec.env_path.as_ptr(),
        exec.env_sock.as_ptr(),
        exec.env_ready
            .as_ref()
            .map_or(std::ptr::null(), |v| v.as_ptr()),
        std::ptr::null(),
    ];

    // SAFETY: libc::execve is async-signal-safe. On success it does not
    // return; on failure errno is set and we _exit(127).
    // execve safety: we are a forked child process, not PID 1; our job is to replace ourselves with the chrooted rescue entrypoint while NMBL stays PID 1 outside.
    let _ = unsafe { libc::execve(exec.path_c.as_ptr(), argv.as_ptr(), envp.as_ptr()) };

    // SAFETY: Unavoidable. Post-fork child must use _exit (the documented
    // exception, same as sys::pty / sys::activation).
    unsafe { libc::_exit(EXEC_FAILED_EXIT_CODE) }
}

/// `fork(2)` the chrooted rescue child and return its Pid. All
/// allocation (the `ChildExec` CStrings) happened before this call; the
/// child branch is async-signal-safe only.
fn fork_rescue_child(exec: &ChildExec) -> Result<Pid> {
    // SAFETY: `nix::unistd::fork` is `unsafe` by design (no safe wrapper
    // exists). The child branch (`child_chroot_exec`) is restricted to
    // async-signal-safe primitives — no allocation, no Rust I/O, no
    // destructors. All CStrings were built in the parent above. This
    // mirrors `sys::pty::spawn::spawn_shell` and is one of the
    // documented exceptions to the project's "minimize unsafe" rule.
    let fork_result = unsafe { fork() }.map_err(|e| NmblError::Rescue {
        stage: "rescue-child-fork",
        source: Box::new(NmblError::Io {
            source: std::io::Error::other(format!("fork() for chrooted rescue child: {e}")),
            context: "forking chrooted rescue child".to_string(),
        }),
    })?;
    match fork_result {
        ForkResult::Parent { child } => Ok(child),
        ForkResult::Child => {
            // === CHILD === async-signal-safe only past this point.
            // SAFETY: child branch of fork(); child_chroot_exec is
            // restricted to async-signal-safe calls and does not return.
            unsafe { child_chroot_exec(exec) }
        }
    }
}

/// How a rescue child run ended.
enum RescueEnd {
    /// The rescue exited by itself.
    Exited(Option<WaitStatus>),
    /// A remote `nmbl` session inside the rescue committed an action.
    #[cfg_attr(not(feature = "remote-tui"), allow(dead_code))]
    Committed(TerminalAction),
}

/// Run the external rescue squashfs as a chrooted child while NMBL stays
/// PID 1, reaping it asynchronously (concurrently with the remote-attach
/// server). Returns `None` once the child has exited and the binds are
/// torn down; the caller resumes the recovery flow. When a remote `nmbl`
/// session commits an action (boot a generation, reboot, ...) the rescue
/// is stopped and unmounted first and the action is returned.
///
/// `rescue_dir` is the writable overlay from `disk::prepare_disk_rescue`
/// (always `/rescue`); `entrypoint` is `config.rescue.entrypoint`.
pub async fn run_external_rescue_child(
    config: &Config,
    rescue_dir: &Path,
    entrypoint: &Path,
    sender: LocalSender,
) -> Result<Option<TerminalAction>> {
    debug_assert_eq!(rescue_dir, Path::new(RESCUE_ROOT));
    // Build the exec strings + set up the binds in the PARENT, before
    // fork (fork-safety: all allocation happens here).
    let (ready_read, ready_write) = nix::unistd::pipe2(
        nix::fcntl::OFlag::O_NONBLOCK | nix::fcntl::OFlag::O_CLOEXEC,
    )
    .map_err(|source| NmblError::Io {
        source: source.into(),
        context: "rescue readiness pipe".into(),
    })?;
    let mut exec = ChildExec::build(entrypoint)?;
    exec.env_ready = Some(
        CString::new(format!("NMBL_RESCUE_READY_FD={}", ready_write.as_raw_fd())).map_err(
            |_| NmblError::ConfigInvalid {
                reason: "invalid readiness descriptor".into(),
                context: "rescue readiness pipe".into(),
            },
        )?,
    );
    exec.ready_read_fd = Some(ready_read.as_raw_fd());
    exec.ready_write_fd = Some(ready_write.as_raw_fd());
    // Pin persistent directories before the shared rescue /mnt covers their
    // runtime paths. Keep owned CLOEXEC descriptors alive through readiness.
    let ready_paths = match ready_paths::ReadyPaths::capture(config) {
        Ok(paths) => Some(paths),
        Err(e) => {
            nmbl_warn!("could not pin rescue boot state: {e}");
            None
        }
    };
    // If the mount plan fails partway, tear down whatever it already set
    // up before propagating: the network path can loop back and retry,
    // and re-running bind/make-shared/rbind over surviving mounts would
    // stack duplicates. teardown_mounts is idempotent (lazy MNT_DETACH).
    if let Err(e) = apply_mount_plan(config) {
        teardown_mounts(config);
        return Err(e);
    }

    let pid = match fork_rescue_child(&exec) {
        Ok(pid) => pid,
        Err(e) => {
            teardown_mounts(config);
            return Err(e);
        }
    };
    nmbl_info!("rescue child: forked pid {pid}, reaping while serving remote attach");

    drop(ready_write);
    let reap = reap_with_server(config, pid, sender);
    tokio::pin!(reap);
    let end = tokio::select! {
        // Prefer an already delivered console-ready signal over a concurrent
        // child exit; readiness is an actual event, not a launch intention.
        biased;
        ready = wait_console_ready(ready_read) => {
            match ready {
                Ok(true) => record_console_ready(ready_paths.as_ref()),
                Ok(false) => nmbl_warn!("rescue child closed readiness pipe before console became ready"),
                Err(e) => nmbl_warn!("rescue readiness failed: {e}"),
            }
            reap.await
        }
        end = &mut reap => end,
    };
    let committed = match end {
        RescueEnd::Exited(Some(WaitStatus::Exited(_, code))) => {
            nmbl_info!("rescue child: exited with code {code}");
            None
        }
        RescueEnd::Exited(Some(WaitStatus::Signaled(_, sig, _))) => {
            nmbl_warn!("rescue child: killed by signal {sig}");
            None
        }
        RescueEnd::Exited(other) => {
            nmbl_warn!("rescue child: reaped with status {other:?}");
            None
        }
        RescueEnd::Committed(action) => {
            nmbl_info!("rescue child: remote session committed an action; stopping the rescue");
            stop::stop_processes(pid);
            Some(action)
        }
    };
    teardown_mounts(config);
    if committed.is_some() {
        stop::release_root(Path::new(RESCUE_ROOT));
    }
    Ok(committed)
}

/// The only accepted message is one typed byte from the owned rescue child.
async fn wait_console_ready(pipe: OwnedFd) -> std::io::Result<bool> {
    let fd = tokio::io::unix::AsyncFd::new(pipe)?;
    loop {
        let mut ready = fd.readable().await?;
        match ready.try_io(|inner| {
            let mut byte = [0u8; 1];
            nix::unistd::read(inner.get_ref().as_raw_fd(), &mut byte)
                .map(|length| (length, byte[0]))
                .map_err(std::io::Error::from)
        }) {
            Err(_) => continue,
            Ok(result) => {
                return match result? {
                    (0, _) => Ok(false),
                    (1, b'R') => Ok(true),
                    _ => Err(std::io::Error::other("invalid rescue readiness message")),
                };
            }
        }
    }
}

fn record_console_ready(paths: Option<&ready_paths::ReadyPaths>) {
    let Some(paths) = paths else {
        nmbl_warn!("rescue ready, but persistent paths were not pinned; keeping rescue request");
        return;
    };
    if let Err(e) = paths.record() {
        nmbl_warn!("rescue ready, but could not persist rescue exit: {e}");
        return;
    }
    nmbl_info!("rescue console ready; next boot leaves this rescue request");
}

/// Reap `pid` concurrently with the remote-attach server. The server runs
/// only as long as the child lives and is dropped (its `SocketUnlinkGuard`
/// unlinks the socket) once the child exits. A remote `nmbl` session that
/// commits an action ends the run with that action; the caller then stops
/// the rescue and performs it. Without `remote-tui` there is no server —
/// just reap.
#[cfg(feature = "remote-tui")]
async fn reap_with_server(config: &Config, pid: Pid, sender: LocalSender) -> RescueEnd {
    use crate::ui::remote::{ActionSink, Shutdown, run_remote_server};

    let shutdown = Shutdown::new();
    let sink: ActionSink = std::rc::Rc::new(std::cell::RefCell::new(None));
    let server = run_remote_server(config, shutdown.clone(), sink.clone(), &sender);
    // Clone the sender so the "server returned without an action" branch
    // can still reap the child (LocalSender is a cheap Rc handle).
    let reap = reap_child(pid, sender.clone());

    tokio::select! {
        biased;
        // Child exited: tell the server to unlink + stop, then return.
        status = reap => {
            shutdown.signal();
            RescueEnd::Exited(status)
        }
        // The server returns once a remote session committed an action,
        // or when it could not bind; only then keep reaping the child.
        () = server => {
            let committed = sink.borrow_mut().take();
            match committed {
                Some(action) => RescueEnd::Committed(action),
                None => RescueEnd::Exited(reap_child(pid, sender).await),
            }
        }
    }
}

/// Reap without the remote server (feature off).
#[cfg(not(feature = "remote-tui"))]
async fn reap_with_server(_config: &Config, pid: Pid, sender: LocalSender) -> RescueEnd {
    RescueEnd::Exited(reap_child(pid, sender).await)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
#[path = "child_tests.rs"]
mod tests;
