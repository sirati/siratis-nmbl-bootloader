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

use std::ffi::CString;
use std::path::Path;

use nix::sys::wait::WaitStatus;
use nix::unistd::{ForkResult, Pid, fork};

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::sys::mount::{make_shared, mount_fs, umount};
use crate::sys::poller::{LocalSender, reap_child};
use crate::{nmbl_info, nmbl_warn};

/// Where the writable rescue overlay is staged (mirrors
/// `disk::RESCUE_MOUNT`). The chroot target.
const RESCUE_ROOT: &str = "/rescue";
/// PID 1's own mountpoint that mirrors the child's `/rescue/mnt` via a
/// shared subtree, so PID 1 observes whatever the child mounts there.
const PID1_MNT: &str = "/mnt";
/// The chroot's `/mnt` (becomes `/mnt` after chroot); made a shared
/// subtree peer of [`PID1_MNT`].
const CHILD_MNT: &str = "/rescue/mnt";
/// NMBL's own root, bind-mounted into the chroot. Becomes `/nmbl-root`
/// after chroot, exposing the TUI socket at
/// `/nmbl-root/nmbl-run/tui.sock` (matches the rescue-sfs contract).
const CHILD_NMBL_ROOT: &str = "/rescue/nmbl-root";

/// Conventional exit code surfaced when the post-fork `execve(2)` (or a
/// pre-exec syscall) fails in the child. Matches `sys::pty`.
const EXEC_FAILED_EXIT_CODE: i32 = 127;

/// One bind/shared-subtree step in the pre-fork mount plan. A pure
/// description so the sequence is unit-testable without privileges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MountStep {
    /// `mkdir -p path` (idempotent).
    MkDir(&'static str),
    /// `mount --bind src dst` (`MS_BIND`).
    Bind {
        src: &'static str,
        dst: &'static str,
    },
    /// `mount --rbind src dst` (`MS_BIND | MS_REC`).
    RBind {
        src: &'static str,
        dst: &'static str,
    },
    /// `mount --make-shared target` (`MS_SHARED`, no fstype/source).
    MakeShared(&'static str),
}

/// The pre-fork mount plan, in execution order. Pure so it can be
/// asserted on in tests:
///
/// ```text
/// mkdir -p /mnt /rescue/nmbl-root /rescue/mnt
/// bind        /rescue/mnt -> /rescue/mnt   (self-bind so it is a mount)
/// make-shared /rescue/mnt                  (MS_SHARED)
/// rbind       /rescue/mnt -> /mnt          (PID 1 sees child mounts)
/// rbind       /           -> /rescue/nmbl-root (expose NMBL root + socket)
/// ```
pub(crate) fn mount_plan() -> Vec<MountStep> {
    vec![
        MountStep::MkDir(PID1_MNT),
        MountStep::MkDir(CHILD_NMBL_ROOT),
        MountStep::MkDir(CHILD_MNT),
        MountStep::Bind {
            src: CHILD_MNT,
            dst: CHILD_MNT,
        },
        MountStep::MakeShared(CHILD_MNT),
        MountStep::RBind {
            src: CHILD_MNT,
            dst: PID1_MNT,
        },
        MountStep::RBind {
            src: "/",
            dst: CHILD_NMBL_ROOT,
        },
    ]
}

/// The teardown plan applied after the child exits, in order. Lazy
/// `MNT_DETACH` is acceptable for the recursive binds (the task spec):
/// unmount PID 1's `/mnt` first (the propagation target), then the
/// child's `/rescue/mnt`, then NMBL's bind at `/rescue/nmbl-root`. Pure
/// for the same reason as [`mount_plan`].
pub(crate) fn umount_plan() -> Vec<&'static str> {
    vec![PID1_MNT, CHILD_MNT, CHILD_NMBL_ROOT]
}

/// Execute [`mount_plan`] with safe nix/std wrappers (parent side, PID 1
/// — runs before `fork`). Any failure aborts with a wrapped
/// [`NmblError::Rescue`] so the recovery flow can surface it.
fn apply_mount_plan() -> Result<()> {
    let wrap = |source: NmblError| NmblError::Rescue {
        stage: "rescue-child-mount",
        source: Box::new(source),
    };
    for step in mount_plan() {
        match step {
            MountStep::MkDir(p) => ensure_dir(Path::new(p)).map_err(wrap)?,
            MountStep::Bind { src, dst } => {
                mount_fs(Some(Path::new(src)), Path::new(dst), "none", "bind").map_err(wrap)?;
            }
            MountStep::RBind { src, dst } => {
                mount_fs(Some(Path::new(src)), Path::new(dst), "none", "rbind").map_err(wrap)?;
            }
            MountStep::MakeShared(p) => make_shared(Path::new(p)).map_err(wrap)?,
        }
    }
    Ok(())
}

/// Tear down the binds set up by [`apply_mount_plan`]. Best-effort: a
/// failed unmount is logged, never propagated — the recovery flow must
/// proceed regardless.
fn teardown_mounts() {
    use nix::mount::MntFlags;
    for target in umount_plan() {
        match umount(Path::new(target), MntFlags::MNT_DETACH) {
            Ok(()) => nmbl_info!("rescue child: detached {target}"),
            Err(e) => nmbl_warn!("rescue child: could not detach {target}: {e}"),
        }
    }
}

/// Create `path` (and parents) idempotently. Mirrors `disk::ensure_dir`.
fn ensure_dir(path: &Path) -> Result<()> {
    match std::fs::create_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(NmblError::Io {
            source: e,
            context: format!("creating {}", path.display()),
        }),
    }
}

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
    let envp: [*const libc::c_char; 4] = [
        exec.env_term.as_ptr(),
        exec.env_path.as_ptr(),
        exec.env_sock.as_ptr(),
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

/// Run the external rescue squashfs as a chrooted child while NMBL stays
/// PID 1, reaping it asynchronously (concurrently with the remote-attach
/// server). Returns once the child has exited and the binds are torn
/// down; the caller resumes the recovery flow.
///
/// `rescue_dir` is the writable overlay from `disk::prepare_disk_rescue`
/// (always `/rescue`); `entrypoint` is `config.rescue.entrypoint`.
pub async fn run_external_rescue_child(
    config: &Config,
    rescue_dir: &Path,
    entrypoint: &Path,
    sender: LocalSender,
) -> Result<()> {
    debug_assert_eq!(rescue_dir, Path::new(RESCUE_ROOT));
    // Build the exec strings + set up the binds in the PARENT, before
    // fork (fork-safety: all allocation happens here).
    let exec = ChildExec::build(entrypoint)?;
    // If the mount plan fails partway, tear down whatever it already set
    // up before propagating: the network path can loop back and retry,
    // and re-running bind/make-shared/rbind over surviving mounts would
    // stack duplicates. teardown_mounts is idempotent (lazy MNT_DETACH).
    if let Err(e) = apply_mount_plan() {
        teardown_mounts();
        return Err(e);
    }

    let pid = match fork_rescue_child(&exec) {
        Ok(pid) => pid,
        Err(e) => {
            teardown_mounts();
            return Err(e);
        }
    };
    nmbl_info!("rescue child: forked pid {pid}, reaping while serving remote attach");

    let status = reap_with_server(config, pid, sender).await;
    match status {
        Some(WaitStatus::Exited(_, code)) => {
            nmbl_info!("rescue child: exited with code {code}");
        }
        Some(WaitStatus::Signaled(_, sig, _)) => {
            nmbl_warn!("rescue child: killed by signal {sig}");
        }
        other => nmbl_warn!("rescue child: reaped with status {other:?}"),
    }
    teardown_mounts();
    Ok(())
}

/// Reap `pid` concurrently with the remote-attach server. The reap arm
/// is terminal; the server runs only as long as the child lives and is
/// dropped (its `SocketUnlinkGuard` unlinks the socket) once the child
/// exits. Without `remote-tui` there is no server — just reap.
#[cfg(feature = "remote-tui")]
async fn reap_with_server(config: &Config, pid: Pid, sender: LocalSender) -> Option<WaitStatus> {
    use crate::ui::remote::{ActionSink, Shutdown, run_remote_server};

    let shutdown = Shutdown::new();
    let sink: ActionSink = std::rc::Rc::new(std::cell::RefCell::new(None));
    let server = run_remote_server(config, shutdown.clone(), sink);
    // Clone the sender so the rare "server returned first" branch can
    // still reap the child (LocalSender is a cheap Rc handle).
    let reap = reap_child(pid, sender.clone());

    tokio::select! {
        biased;
        // Child exited: tell the server to unlink + stop, then return.
        status = reap => {
            shutdown.signal();
            status
        }
        // The server only returns if a remote session committed an
        // action (or bind failed); rescue ownership belongs to the
        // child, so ignore any committed action and keep reaping the
        // child to completion on a fresh reap future.
        () = server => reap_child(pid, sender).await,
    }
}

/// Reap without the remote server (feature off).
#[cfg(not(feature = "remote-tui"))]
async fn reap_with_server(_config: &Config, pid: Pid, sender: LocalSender) -> Option<WaitStatus> {
    reap_child(pid, sender).await
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
#[path = "child_tests.rs"]
mod tests;
