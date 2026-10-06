//! Stop a running rescue system so an action committed from a remote
//! `nmbl` session inside it can take effect.
//!
//! The rescue runs as NMBL's chrooted child, but its `sshd` sessions call
//! `setsid`, so the child's process group does not cover the whole rescue.
//! NMBL is PID 1 and runs no other process at this point, so every other
//! process belongs to the rescue: signal them all (`kill(-1)`, which skips
//! PID 1), wait until they are gone, then flush and detach the rescue tree.
//! Off PID 1 (unit tests, the simbox) only the child's own group is signalled.

use std::path::Path;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::mount::MntFlags;
use nix::sys::signal::{Signal, kill, killpg};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{Pid, getpid, sync};

use crate::{nmbl_info, nmbl_warn};

/// How long the rescue gets to exit on `SIGTERM` before `SIGKILL`.
const TERM_GRACE: Duration = Duration::from_secs(5);
/// How long `SIGKILL`ed processes get to disappear.
const KILL_GRACE: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(20);

/// Terminate the rescue's processes, reap them and flush every filesystem.
pub(super) fn stop_processes(child: Pid) {
    let everyone = getpid() == Pid::from_raw(1);
    sync();
    signal_rescue(child, everyone, Signal::SIGTERM);
    if !reap_until_gone(child, everyone, TERM_GRACE) {
        nmbl_warn!("rescue did not stop on SIGTERM; killing it");
        signal_rescue(child, everyone, Signal::SIGKILL);
        if !reap_until_gone(child, everyone, KILL_GRACE) {
            nmbl_warn!("rescue processes still present after SIGKILL");
        }
    }
    // Writes from the rescue (e.g. to the persistent store) reach disk
    // before the mounts go and the action reboots or kexecs.
    sync();
}

/// Detach the rescue root with everything the rescue mounted below it, once
/// its processes are gone and the child binds are torn down.
pub(super) fn release_root(rescue_root: &Path) {
    if let Err(e) = nix::mount::umount2(rescue_root, MntFlags::MNT_DETACH) {
        nmbl_warn!("could not detach {}: {e}", rescue_root.display());
    }
    sync();
    nmbl_info!("rescue stopped");
}

fn signal_rescue(child: Pid, everyone: bool, signal: Signal) {
    let result = if everyone {
        kill(Pid::from_raw(-1), signal)
    } else {
        killpg(child, signal).or_else(|_| kill(child, signal))
    };
    match result {
        Ok(()) | Err(Errno::ESRCH) => {}
        Err(e) => nmbl_warn!("signalling the rescue with {signal}: {e}"),
    }
}

/// Reap until nothing of the rescue is left or `grace` runs out. As PID 1
/// every orphan of the rescue is reparented to NMBL, so `ECHILD` means the
/// whole tree is gone.
fn reap_until_gone(child: Pid, everyone: bool, grace: Duration) -> bool {
    let target = if everyone { Pid::from_raw(-1) } else { child };
    let deadline = Instant::now() + grace;
    loop {
        match waitpid(target, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => {}
            Ok(_) if everyone => continue,
            Ok(_) | Err(Errno::ECHILD) => return true,
            Err(Errno::EINTR) => continue,
            Err(e) => {
                nmbl_warn!("reaping the rescue: {e}");
                return false;
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL);
    }
}
