//! Access to the trapped task: read/write its memory, grab its fds, resolve
//! paths in its root, and run privileged-in-the-container operations (bind
//! mounts) from a forked helper that joins the container's namespaces.
//!
//! The container runs in a user namespace owned by our uid, so we hold every
//! capability *in that namespace* while its processes hold none: reading
//! `/proc/<pid>/mem`, `pidfd_getfd`, and `setns` into its user+mount
//! namespaces are all permitted to us and to nobody inside.

use std::ffi::CString;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::FileExt;

/// A trapped task, addressed by pid (from the seccomp notification).
pub struct Task {
    pub pid: u32,
}

impl Task {
    pub fn new(pid: u32) -> Self {
        Self { pid }
    }

    /// Read `len` bytes of the task's memory at `addr`.
    pub fn read_mem(&self, addr: u64, len: usize) -> std::io::Result<Vec<u8>> {
        let mut f = File::open(format!("/proc/{}/mem", self.pid))?;
        f.seek(SeekFrom::Start(addr))?;
        let mut buf = vec![0u8; len];
        let mut got = 0;
        while got < len {
            let n = f.read(buf.get_mut(got..).unwrap_or_default())?;
            if n == 0 {
                break;
            }
            got += n;
        }
        buf.truncate(got);
        Ok(buf)
    }

    /// Read a NUL-terminated string (at most 4096 bytes) at `addr`.
    pub fn read_cstr(&self, addr: u64) -> std::io::Result<String> {
        if addr == 0 {
            return Ok(String::new());
        }
        let f = File::open(format!("/proc/{}/mem", self.pid))?;
        let mut out = Vec::new();
        let mut chunk = [0u8; 256];
        let mut pos = addr;
        while out.len() < 4096 {
            let n = f.read_at(&mut chunk, pos)?;
            if n == 0 {
                break;
            }
            let part = chunk.get(..n).unwrap_or_default();
            if let Some(nul) = part.iter().position(|&b| b == 0) {
                out.extend_from_slice(part.get(..nul).unwrap_or_default());
                return Ok(String::from_utf8_lossy(&out).into_owned());
            }
            out.extend_from_slice(part);
            pos += n as u64;
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    /// Write `data` into the task's memory at `addr`.
    pub fn write_mem(&self, addr: u64, data: &[u8]) -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(format!("/proc/{}/mem", self.pid))?;
        f.seek(SeekFrom::Start(addr))?;
        f.write_all(data)
    }

    /// Duplicate the task's fd `target_fd` into our process.
    pub fn get_fd(&self, target_fd: i32) -> std::io::Result<OwnedFd> {
        // SAFETY: raw pidfd_open / pidfd_getfd syscalls; results checked.
        unsafe {
            let pidfd = libc::syscall(libc::SYS_pidfd_open, self.pid, 0);
            if pidfd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let pidfd = OwnedFd::from_raw_fd(pidfd as RawFd);
            let fd = libc::syscall(libc::SYS_pidfd_getfd, pidfd.as_raw_fd(), target_fd, 0);
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(OwnedFd::from_raw_fd(fd as RawFd))
        }
    }

    /// Open `path` (as the task sees it) with `RESOLVE_IN_ROOT`, so absolute
    /// symlinks resolve inside the container rather than on the host.
    pub fn open_in_root(&self, path: &str, flags: i32, mode: u32) -> std::io::Result<OwnedFd> {
        let root = File::open(format!("/proc/{}/root", self.pid))?;
        let rel = CString::new(path.trim_start_matches('/'))
            .map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
        #[repr(C)]
        struct OpenHow {
            flags: u64,
            mode: u64,
            resolve: u64,
        }
        const RESOLVE_IN_ROOT: u64 = 0x10;
        let how = OpenHow {
            flags: (flags | libc::O_CLOEXEC) as u64,
            mode: u64::from(mode),
            resolve: RESOLVE_IN_ROOT,
        };
        // SAFETY: openat2 with a valid dirfd, NUL-terminated path and open_how.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                root.as_raw_fd(),
                rel.as_ptr(),
                &how as *const OpenHow,
                std::mem::size_of::<OpenHow>(),
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: fresh fd from openat2.
        Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
    }

    /// Read a symlink target inside the container, `None` if not a symlink.
    pub fn readlink_in_root(&self, path: &str) -> Option<String> {
        let (dir, name) = path.rsplit_once('/')?;
        let dirfd = self
            .open_in_root(
                if dir.is_empty() { "/" } else { dir },
                libc::O_PATH | libc::O_DIRECTORY,
                0,
            )
            .ok()?;
        let cname = CString::new(name).ok()?;
        let mut buf = [0u8; 4096];
        // SAFETY: readlinkat into a buffer we own.
        let n = unsafe {
            libc::readlinkat(
                dirfd.as_raw_fd(),
                cname.as_ptr(),
                buf.as_mut_ptr().cast(),
                buf.len(),
            )
        };
        if n <= 0 {
            return None;
        }
        Some(String::from_utf8_lossy(buf.get(..n as usize)?).into_owned())
    }

    /// Resolve a device path to the basename of the node it finally names,
    /// following symlinks inside the container (e.g.
    /// `/dev/disk/by-partlabel/root -> ../../vda2` gives `vda2`).
    pub fn resolve_dev_basename(&self, path: &str) -> String {
        let mut current = path.to_string();
        for _ in 0..16 {
            match self.readlink_in_root(&current) {
                Some(target) => {
                    current = if target.starts_with('/') {
                        target
                    } else {
                        let dir = current.rsplit_once('/').map_or("", |(d, _)| d);
                        format!("{dir}/{target}")
                    };
                }
                None => break,
            }
        }
        current.rsplit('/').next().unwrap_or(&current).to_string()
    }

    /// `mkdir -p` inside the container (best effort).
    pub fn mkdir_in_root(&self, path: &str) -> std::io::Result<()> {
        std::fs::create_dir_all(format!("/proc/{}/root{}", self.pid, path))
    }

    /// Create an empty regular file at `path` inside the container (used to
    /// stand in for device nodes the task tried to `mknod`).
    pub fn create_file_in_root(&self, path: &str) -> std::io::Result<()> {
        self.open_in_root(path, libc::O_CREAT | libc::O_WRONLY, 0o600)
            .map(drop)
    }

    /// Bind-mount `src` onto `dst`, both paths as the container sees them,
    /// from a forked helper that joins the task's user + mount namespaces and
    /// root. Returns the errno on failure.
    pub fn bind_mount(&self, src: &str, dst: &str) -> Result<(), i32> {
        self.in_namespaces(|| {
            let (Ok(s), Ok(d)) = (CString::new(src), CString::new(dst)) else {
                return libc::EINVAL;
            };
            // SAFETY: mount(2) with valid C strings (prepared by the caller's
            // closure before fork is impossible here, but CString::new only
            // allocates; this runs in a single-threaded forked child).
            let rc = unsafe {
                libc::mount(
                    s.as_ptr(),
                    d.as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND | libc::MS_REC,
                    std::ptr::null(),
                )
            };
            if rc == 0 { 0 } else { errno() }
        })
    }

    /// Detach-unmount `target` inside the container.
    pub fn umount(&self, target: &str) -> Result<(), i32> {
        self.in_namespaces(|| {
            let Ok(t) = CString::new(target) else {
                return libc::EINVAL;
            };
            // SAFETY: umount2 with a valid C string in the forked child.
            let rc = unsafe { libc::umount2(t.as_ptr(), libc::MNT_DETACH) };
            if rc == 0 { 0 } else { errno() }
        })
    }

    /// Run `op` in a forked child that has joined the task's user and mount
    /// namespaces and chrooted into its root. `op` returns an errno (0 = ok).
    fn in_namespaces(&self, op: impl FnOnce() -> i32) -> Result<(), i32> {
        let open = |ns: &str| File::open(format!("/proc/{}/{ns}", self.pid));
        let (Ok(userns), Ok(mntns), Ok(root)) = (open("ns/user"), open("ns/mnt"), open("root"))
        else {
            return Err(libc::ESRCH);
        };
        // SAFETY: fork; the child only calls setns/fchdir/chroot, the op's
        // syscalls, and _exit. The supervisor keeps no locks across this call
        // that the child could need.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(errno());
        }
        if pid == 0 {
            // SAFETY: raw syscalls in the forked child, then _exit.
            unsafe {
                let code = if libc::setns(userns.as_raw_fd(), libc::CLONE_NEWUSER) != 0
                    || libc::setns(mntns.as_raw_fd(), libc::CLONE_NEWNS) != 0
                    || libc::fchdir(root.as_raw_fd()) != 0
                    || libc::chroot(c".".as_ptr()) != 0
                {
                    errno()
                } else {
                    op()
                };
                libc::_exit(code & 0xff);
            }
        }
        let mut status = 0;
        // SAFETY: waitpid on our own child.
        unsafe { libc::waitpid(pid, &mut status, 0) };
        let code = if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else {
            libc::EIO
        };
        if code == 0 { Ok(()) } else { Err(code) }
    }
}

fn errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

/// Read a whole file behind an fd we own, from offset 0.
pub fn read_fd_all(fd: &OwnedFd) -> std::io::Result<Vec<u8>> {
    let f = File::from(fd.try_clone()?);
    let mut out = Vec::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut off = 0u64;
    loop {
        let n = f.read_at(&mut buf, off)?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(buf.get(..n).unwrap_or_default());
        off += n as u64;
    }
    Ok(out)
}
