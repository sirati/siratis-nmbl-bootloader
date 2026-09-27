//! Seccomp user-notification plumbing (`SECCOMP_RET_USER_NOTIF`).
//!
//! crun installs the container's seccomp filter with `SCMP_ACT_NOTIFY` for the
//! syscalls listed in the profile and, via the `run.oci.seccomp.receiver`
//! annotation, sends the resulting listener fd to our unix socket with
//! `SCM_RIGHTS`. From then on every trapped syscall of the container blocks in
//! the kernel until we answer it with `SECCOMP_IOCTL_NOTIF_SEND`: either a
//! synthetic return value/errno, or `SECCOMP_USER_NOTIF_FLAG_CONTINUE` to let
//! the kernel run it for real.
//!
//! This is the only mechanism that intercepts syscalls of a rootless,
//! capability-free process without ptrace (which would need the tracer to
//! share the container's user namespace, and Yama restricts it) and without
//! running anything privileged: the filter is installed by the unprivileged
//! runtime inside the container's own user namespace.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

// linux/seccomp.h: SECCOMP_IOCTL_NOTIF_{RECV,SEND,ID_VALID,ADDFD}.
const IOCTL_RECV: libc::c_ulong = 0xc050_2100;
const IOCTL_SEND: libc::c_ulong = 0xc018_2101;
const IOCTL_ID_VALID: libc::c_ulong = 0x4008_2102;
const IOCTL_ADDFD: libc::c_ulong = 0x4018_2103;
const FLAG_CONTINUE: u32 = 1;

/// `struct seccomp_notif` (80 bytes).
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct RawNotif {
    id: u64,
    pid: u32,
    flags: u32,
    nr: i32,
    arch: u32,
    instruction_pointer: u64,
    args: [u64; 6],
}

/// `struct seccomp_notif_resp`.
#[repr(C)]
struct RawResp {
    id: u64,
    val: i64,
    error: i32,
    flags: u32,
}

/// `struct seccomp_notif_addfd`.
#[repr(C)]
struct RawAddFd {
    id: u64,
    flags: u32,
    srcfd: u32,
    newfd: u32,
    newfd_flags: u32,
}

/// One trapped syscall.
#[derive(Debug, Clone, Copy)]
pub struct Notif {
    pub id: u64,
    pub pid: u32,
    pub nr: i64,
    pub args: [u64; 6],
}

/// The listener fd crun handed us.
pub struct Listener {
    fd: OwnedFd,
}

/// How we answer a trapped syscall.
#[derive(Debug, Clone, Copy)]
pub enum Reply {
    /// Return `val` as the syscall result.
    Value(i64),
    /// Fail with this (positive) errno.
    Errno(i32),
    /// Let the kernel execute the syscall for real.
    Continue,
    /// Install this fd (of ours) in the task and return its number.
    InstallFd(RawFd),
}

impl Listener {
    pub fn from_fd(fd: OwnedFd) -> Self {
        Self { fd }
    }

    /// Block until the next trapped syscall. `None` when the container is gone.
    pub fn recv(&self) -> Option<Notif> {
        let mut raw = RawNotif::default();
        loop {
            // SAFETY: ioctl on our listener fd with a correctly sized, owned
            // `seccomp_notif`; the kernel writes into it.
            let rc = unsafe { libc::ioctl(self.fd.as_raw_fd(), IOCTL_RECV as _, &mut raw) };
            if rc == 0 {
                return Some(Notif {
                    id: raw.id,
                    pid: raw.pid,
                    nr: i64::from(raw.nr),
                    args: raw.args,
                });
            }
            match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::EINTR) => continue,
                // ENOENT: the notifying task died before we read it.
                Some(libc::ENOENT) => continue,
                _ => return None,
            }
        }
    }

    /// Whether the notification is still live (the task has not been killed).
    /// Must be checked after reading target memory and before acting on it.
    pub fn still_valid(&self, id: u64) -> bool {
        let mut id = id;
        // SAFETY: ioctl with a pointer to a u64 cookie, as the ABI requires.
        unsafe { libc::ioctl(self.fd.as_raw_fd(), IOCTL_ID_VALID as _, &mut id) == 0 }
    }

    /// Answer a trapped syscall.
    pub fn reply(&self, id: u64, reply: Reply) {
        let resp = match reply {
            Reply::Value(v) => RawResp {
                id,
                val: v,
                error: 0,
                flags: 0,
            },
            Reply::Errno(e) => RawResp {
                id,
                val: 0,
                error: -e,
                flags: 0,
            },
            Reply::Continue => RawResp {
                id,
                val: 0,
                error: 0,
                flags: FLAG_CONTINUE,
            },
            Reply::InstallFd(fd) => {
                if self.reply_with_fd(id, fd, true).is_err() {
                    self.reply(id, Reply::Errno(libc::EMFILE));
                }
                return;
            }
        };
        // SAFETY: ioctl with a correctly laid-out `seccomp_notif_resp`.
        let _ = unsafe { libc::ioctl(self.fd.as_raw_fd(), IOCTL_SEND as _, &resp) };
    }

    /// Install `srcfd` (one of OUR fds) into the trapped task's fd table and
    /// make it the syscall's return value (`SECCOMP_ADDFD_FLAG_SEND`). Used to
    /// hand back simulated device fds (memfds) for `open("/dev/…")`.
    pub fn reply_with_fd(&self, id: u64, srcfd: RawFd, cloexec: bool) -> Result<i32, i32> {
        const ADDFD_FLAG_SEND: u32 = 2;
        let add = RawAddFd {
            id,
            flags: ADDFD_FLAG_SEND,
            srcfd: u32::try_from(srcfd).map_err(|_| libc::EBADF)?,
            newfd: 0,
            newfd_flags: if cloexec { libc::O_CLOEXEC as u32 } else { 0 },
        };
        // SAFETY: ioctl with a correctly laid-out `seccomp_notif_addfd`.
        let rc = unsafe { libc::ioctl(self.fd.as_raw_fd(), IOCTL_ADDFD as _, &add) };
        if rc < 0 {
            Err(std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EIO))
        } else {
            Ok(rc)
        }
    }
}

/// Receive the listener fd crun sends to `socket_path` (one connection, one
/// `SCM_RIGHTS` fd plus a JSON state document we ignore).
pub fn accept_listener(listener: &std::os::unix::net::UnixListener) -> std::io::Result<Listener> {
    let (conn, _) = listener.accept()?;
    let mut data = [0u8; 8192];
    let mut cmsg = [0u8; 64];
    let mut iov = libc::iovec {
        iov_base: data.as_mut_ptr().cast(),
        iov_len: data.len(),
    };
    // SAFETY: a zeroed msghdr is a valid initial state.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg.as_mut_ptr().cast();
    msg.msg_controllen = cmsg.len() as _;
    // SAFETY: recvmsg into buffers we own that outlive the call.
    let n = unsafe { libc::recvmsg(conn.as_raw_fd(), &mut msg, 0) };
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: CMSG_* walk the control buffer we just filled.
    let fd = unsafe {
        let hdr = libc::CMSG_FIRSTHDR(&msg);
        if hdr.is_null() || (*hdr).cmsg_type != libc::SCM_RIGHTS {
            return Err(std::io::Error::other(
                "no SCM_RIGHTS fd from the OCI runtime",
            ));
        }
        std::ptr::read_unaligned(libc::CMSG_DATA(hdr).cast::<RawFd>())
    };
    // SAFETY: the fd was just received and is owned by nobody else.
    Ok(Listener::from_fd(unsafe { OwnedFd::from_raw_fd(fd) }))
}
