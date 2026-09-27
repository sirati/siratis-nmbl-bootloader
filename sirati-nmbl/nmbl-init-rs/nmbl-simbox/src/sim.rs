//! The syscall simulator: answers every trapped syscall of the container from
//! the scenario, and records what NMBL handed to kexec.
//!
//! Faithful (the real behaviour is reproduced from the scenario):
//! * `mount` of a block device or LUKS mapper: bind-mounts the scenario tree
//!   for that device, after resolving `/dev/disk/by-*` symlinks inside the
//!   container; fstype must match the device's blkid `TYPE`, else `EINVAL`;
//!   unknown devices give `ENOENT`. `MS_BIND` mounts are performed for real
//!   inside the container's namespaces. `umount2` detaches for real.
//! * `mknod`/`mknodat`: creates the node as an empty regular file (a device
//!   stand-in NMBL's readiness check accepts); `EEXIST` if present.
//! * `uname`: the host's utsname with the scenario's kernel release, so module
//!   lookups hit the initramfs' own `/lib/modules/<release>`.
//! * `init_module`/`finit_module`: the module image is read and its `name=`
//!   recorded; a second load of the same module gives `EEXIST`.
//! * `kexec_file_load`: kernel and initrd are read through the task's own fds
//!   (`pidfd_getfd`), the cmdline from its memory; nothing is loaded.
//! * `reboot`: `LINUX_REBOOT_CMD_KEXEC` ends the run with the captured handoff;
//!   restart/halt/power-off end it with that outcome; CAD toggles succeed.
//!
//! Stubbed: `proc`/`sysfs`/`devtmpfs`/`tmpfs`/`devpts` mounts succeed without
//! effect (podman already provides them and `/sys` is the scenario's tree);
//! loop devices and `kexec_load` return `ENODEV`/`ENOSYS`.

use std::collections::BTreeSet;
use std::path::PathBuf;

use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::drm::Card;
use crate::scenario::Scenario;
use crate::seccomp::{Notif, Reply};
use crate::task::{Task, read_fd_all};

// x86_64 syscall numbers.
const SYS_IOCTL: i64 = 16;
const SYS_UNAME: i64 = 63;
const SYS_MKNOD: i64 = 133;
const SYS_MOUNT: i64 = 165;
const SYS_UMOUNT2: i64 = 166;
const SYS_REBOOT: i64 = 169;
const SYS_INIT_MODULE: i64 = 175;
const SYS_KEXEC_LOAD: i64 = 246;
const SYS_MKNODAT: i64 = 259;
const SYS_FINIT_MODULE: i64 = 313;
const SYS_KEXEC_FILE_LOAD: i64 = 320;
const SYS_NEWFSTATAT: i64 = 262;
const SYS_STATX: i64 = 332;
const SYS_STAT: i64 = 4;
const SYS_OPENAT: i64 = 257;
const SYS_OPEN: i64 = 2;
const SYS_LSTAT: i64 = 6;

const MS_REMOUNT: u64 = 32;
const MS_BIND: u64 = 4096;
const KEXEC_FILE_NO_INITRAMFS: u64 = 4;

pub const SYSCALLS: &[&str] = &[
    "mount",
    "umount2",
    "mknod",
    "mknodat",
    "uname",
    "init_module",
    "finit_module",
    "kexec_load",
    "kexec_file_load",
    "reboot",
    "stat",
    "lstat",
    "newfstatat",
    "statx",
];

/// The kernel/initrd/cmdline NMBL passed to `kexec_file_load`.
#[derive(Debug, Clone, Default)]
pub struct KexecLoad {
    pub kernel: Vec<u8>,
    pub kernel_path: Option<String>,
    pub initrd: Option<Vec<u8>>,
    pub cmdline: String,
}

/// How the simulated machine ended.
#[derive(Debug, Clone)]
pub enum Outcome {
    Kexec(Box<KexecLoad>),
    Reboot,
    Halt,
    PowerOff,
}

/// Extra syscalls trapped in graphical mode (DRM card + VT input).
pub const GRAPHICAL_SYSCALLS: &[&str] = &["open", "openat", "ioctl"];

/// The simulated display: a DRM card and the console pty the splash reads
/// keys from (NMBL opens /dev/tty1 for splash input).
pub struct Display {
    pub card: Card,
    pub pty_slave: std::path::PathBuf,
    /// Frame counter the X11 viewer polls.
    pub frames: Arc<AtomicU64>,
    /// Our fds kept alive while installed in the task.
    held: Vec<OwnedFd>,
}

impl Display {
    pub fn new(card: Card, pty_slave: std::path::PathBuf, frames: Arc<AtomicU64>) -> Self {
        Self {
            card,
            pty_slave,
            frames,
            held: Vec::new(),
        }
    }
}

/// Simulator state for one run.
pub struct Sim {
    pub display: Option<Display>,
    scenario: Scenario,
    /// `/.simbox/trees/<name>` inside the container, per block device / mapper.
    release: String,
    loaded_modules: BTreeSet<String>,
    kexec: Option<KexecLoad>,
    dev_prepared: bool,
    /// Human-readable trace of every simulated call, for `--trace`.
    pub trace: Vec<String>,
}

impl Sim {
    pub fn new(scenario: Scenario, release: String) -> Self {
        Self {
            display: None,
            scenario,
            release,
            loaded_modules: BTreeSet::new(),
            kexec: None,
            dev_prepared: false,
            trace: Vec::new(),
        }
    }

    /// Answer one notification. Returns `Some(outcome)` when the machine ended.
    pub fn handle(&mut self, n: &Notif) -> (Reply, Option<Outcome>) {
        let task = Task::new(n.pid);
        let a = n.args;
        match n.nr {
            SYS_MOUNT => (self.mount(&task, a), None),
            SYS_UMOUNT2 => {
                let target = task.read_cstr(a[0]).unwrap_or_default();
                let _ = task.umount(&target);
                self.log(format!("umount2({target}) -> 0"));
                (Reply::Value(0), None)
            }
            SYS_MKNOD => (self.mknod(&task, a[0]), None),
            SYS_MKNODAT => (self.mknod(&task, a[1]), None),
            SYS_UNAME => (self.uname(&task, a[0]), None),
            SYS_INIT_MODULE => {
                let image = task
                    .read_mem(a[0], usize::try_from(a[1]).unwrap_or(0))
                    .unwrap_or_default();
                (self.load_module(&image), None)
            }
            SYS_FINIT_MODULE => {
                let image = i32::try_from(a[0])
                    .ok()
                    .and_then(|fd| task.get_fd(fd).ok())
                    .and_then(|fd| read_fd_all(&fd).ok())
                    .unwrap_or_default();
                (self.load_module(&image), None)
            }
            SYS_KEXEC_LOAD => (Reply::Errno(libc::ENOSYS), None),
            SYS_KEXEC_FILE_LOAD => (self.kexec_file_load(&task, a), None),
            SYS_REBOOT => self.reboot(a[2]),
            SYS_STATX => (self.statx(&task, a), None),
            SYS_NEWFSTATAT => (self.fstatat(&task, a[1], a[2], a[3]), None),
            SYS_STAT => (self.fstatat(&task, a[0], a[1], 0), None),
            SYS_LSTAT => (
                self.fstatat(&task, a[0], a[1], libc::AT_SYMLINK_NOFOLLOW as u64),
                None,
            ),
            SYS_IOCTL => (self.ioctl(&task, a), None),
            SYS_OPENAT => (self.openat(&task, a[1]), None),
            SYS_OPEN => (self.openat(&task, a[0]), None),
            _ => (Reply::Continue, None),
        }
    }

    fn openat(&mut self, task: &Task, path_ptr: u64) -> Reply {
        let Some(d) = self.display.as_mut() else {
            return Reply::Continue;
        };
        let path = task.read_cstr(path_ptr).unwrap_or_default();
        let fd = if path.starts_with("/dev/dri/card") {
            d.card.memfd.try_clone().ok()
        } else if path == "/dev/tty1" || path == "/dev/tty0" {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&d.pty_slave)
                .ok()
                .map(OwnedFd::from)
        } else {
            return Reply::Continue;
        };
        let Some(fd) = fd else {
            return Reply::Errno(libc::ENOENT);
        };
        let raw = fd.as_raw_fd();
        d.held.push(fd);
        self.trace
            .push(format!("openat({path}) -> simulated device"));
        Reply::InstallFd(raw)
    }

    fn ioctl(&mut self, task: &Task, a: [u64; 6]) -> Reply {
        let Some(d) = self.display.as_mut() else {
            return Reply::Continue;
        };
        let fd = i32::try_from(a[0]).unwrap_or(-1);
        // Only the card fd is ours; identify it by its /proc link (a memfd
        // named "nmbl-simbox-drm").
        let link = std::fs::read_link(format!("/proc/{}/fd/{fd}", task.pid)).ok();
        let is_card = link
            .as_ref()
            .is_some_and(|p| p.to_string_lossy().contains("nmbl-simbox-drm"));
        if !is_card {
            // The splash input "VT" is our pty: answer the VT/KD ioctls a real
            // VT accepts (VT_ACTIVATE/WAITACTIVE 0x5606/7, KDSETMODE/KDGETMODE
            // 0x4B3A/B, KDSKBMODE/KDGKBMODE 0x4B45/44, KDGKBLED 0x4B64) so
            // NMBL's splash input bring-up behaves as on hardware.
            let req = a[1] & 0xffff_ffff;
            let on_vt = link.as_ref().is_some_and(|p| *p == d.pty_slave);
            if on_vt && matches!(req, 0x5606 | 0x5607 | 0x4B3A | 0x4B45 | 0x4B51) {
                return Reply::Value(0);
            }
            if on_vt && matches!(req, 0x4B3B | 0x4B44 | 0x4B64) {
                // KD_TEXT / K_XLATE / no LEDs: all encode as 0.
                return match task.write_mem(a[2], &[0u8; 4]) {
                    Ok(()) => Reply::Value(0),
                    Err(_) => Reply::Errno(libc::EFAULT),
                };
            }
            return Reply::Continue;
        }
        let before = d.card.frames;
        let r = d
            .card
            .ioctl(task, a[1] & 0xffff_ffff, a[2])
            .unwrap_or(Reply::Errno(libc::ENOTTY));
        if d.card.frames != before {
            d.frames.store(d.card.frames, Ordering::Release);
        }
        r
    }

    fn log(&mut self, line: String) {
        self.trace.push(line);
    }

    fn mount(&mut self, task: &Task, a: [u64; 6]) -> Reply {
        let source = task.read_cstr(a[0]).unwrap_or_default();
        let target = task.read_cstr(a[1]).unwrap_or_default();
        let fstype = task.read_cstr(a[2]).unwrap_or_default();
        let flags = a[3];
        let reply = if flags & MS_REMOUNT != 0 {
            Reply::Value(0)
        } else if flags & MS_BIND != 0 {
            match task.bind_mount(&source, &target) {
                Ok(()) => Reply::Value(0),
                Err(e) => Reply::Errno(e),
            }
        } else if matches!(
            fstype.as_str(),
            "proc" | "sysfs" | "devtmpfs" | "tmpfs" | "devpts" | "ramfs" | "efivarfs"
        ) {
            if fstype == "devtmpfs" && !self.dev_prepared {
                // Directories real devtmpfs/udev would provide.
                let _ = task.mkdir_in_root("/dev/mapper");
                self.dev_prepared = true;
            }
            Reply::Value(0)
        } else {
            self.mount_device(task, &source, &target, &fstype)
        };
        self.log(format!(
            "mount({source}, {target}, {fstype}, {flags:#x}) -> {reply:?}"
        ));
        reply
    }

    fn mount_device(&self, task: &Task, source: &str, target: &str, fstype: &str) -> Reply {
        let node = task.resolve_dev_basename(source);
        // A LUKS mapper opened by the simulated cryptsetup.
        if let Some(l) = self.scenario.luks.iter().find(|l| l.name == node) {
            let _ = l;
            return bind_tree(task, &format!("/.simbox/mapper/{node}"), target);
        }
        let Some(dev) = self.scenario.block.iter().find(|b| b.name == node) else {
            return Reply::Errno(libc::ENOENT);
        };
        if dev.tree.is_none() {
            return Reply::Errno(libc::EINVAL);
        }
        if let Some(t) = dev.blkid.get("TYPE")
            && t != fstype
        {
            return Reply::Errno(libc::EINVAL);
        }
        bind_tree(task, &format!("/.simbox/trees/{node}"), target)
    }

    /// The simulated block device node `path` names, if any. Device nodes
    /// exist in the container as empty regular files (mknod is unprivileged
    /// there); stat must still report them as block devices with the right
    /// major:minor, since NMBL tells device nodes from loop-image files by type.
    fn device_node(&self, path: &str) -> Option<(u32, u32)> {
        let name = path.strip_prefix("/dev/")?;
        if let Some(b) = self.scenario.block.iter().find(|b| b.name == name) {
            return Some((b.major, b.minor));
        }
        let mapper = name.strip_prefix("mapper/")?;
        self.scenario
            .luks
            .iter()
            .position(|l| l.name == mapper)
            .map(|i| (253, u32::try_from(i).unwrap_or(0)))
    }

    /// Canonical absolute path the task means, following symlinks inside the
    /// container (only for paths under /dev, the only ones we rewrite).
    fn dev_target(task: &Task, dirfd: i64, path: &str, follow: bool) -> Option<String> {
        if !path.starts_with("/dev/")
            || dirfd != i64::from(libc::AT_FDCWD) && !path.starts_with('/')
        {
            return None;
        }
        if !follow {
            return Some(path.to_string());
        }
        let base = task.resolve_dev_basename(path);
        // resolve_dev_basename returns the final component; rebuild under /dev
        // (by-* links point at /dev/<node> or /dev/mapper/<node>).
        if task
            .open_in_root(&format!("/dev/mapper/{base}"), libc::O_PATH, 0)
            .is_ok()
            && path.contains("mapper")
        {
            return Some(format!("/dev/mapper/{base}"));
        }
        Some(format!("/dev/{base}"))
    }

    fn statx(&mut self, task: &Task, a: [u64; 6]) -> Reply {
        let path = task.read_cstr(a[1]).unwrap_or_default();
        let follow = a[2] & libc::AT_SYMLINK_NOFOLLOW as u64 == 0;
        let Some(dev) = Self::dev_target(task, a[0] as i32 as i64, &path, follow)
            .and_then(|p| self.device_node(&p))
        else {
            return Reply::Continue;
        };
        if task.open_in_root(&path, libc::O_PATH, 0).is_err() {
            return Reply::Errno(libc::ENOENT);
        }
        // struct statx: stx_mask@0 u32, stx_blksize@4, stx_attributes@8 u64,
        // stx_nlink@16, stx_uid@20, stx_gid@24, stx_mode@28 u16,
        // stx_rdev_major@128 u32, stx_rdev_minor@132 u32.
        let mut buf = [0u8; 256];
        let mask: u32 = 0x7ff; // STATX_BASIC_STATS
        buf[0..4].copy_from_slice(&mask.to_ne_bytes());
        buf[4..8].copy_from_slice(&4096u32.to_ne_bytes());
        buf[16..20].copy_from_slice(&1u32.to_ne_bytes());
        let mode: u16 = (libc::S_IFBLK | 0o600) as u16;
        buf[28..30].copy_from_slice(&mode.to_ne_bytes());
        buf[128..132].copy_from_slice(&dev.0.to_ne_bytes());
        buf[132..136].copy_from_slice(&dev.1.to_ne_bytes());
        let reply = match task.write_mem(a[4], &buf) {
            Ok(()) => Reply::Value(0),
            Err(_) => Reply::Errno(libc::EFAULT),
        };
        self.log(format!("statx({path}) -> block {}:{}", dev.0, dev.1));
        reply
    }

    fn fstatat(&mut self, task: &Task, path_ptr: u64, statbuf: u64, flags: u64) -> Reply {
        let path = task.read_cstr(path_ptr).unwrap_or_default();
        let follow = flags & libc::AT_SYMLINK_NOFOLLOW as u64 == 0;
        let Some(dev) = Self::dev_target(task, i64::from(libc::AT_FDCWD), &path, follow)
            .and_then(|p| self.device_node(&p))
        else {
            return Reply::Continue;
        };
        if task.open_in_root(&path, libc::O_PATH, 0).is_err() {
            return Reply::Errno(libc::ENOENT);
        }
        // SAFETY: a zeroed stat is valid plain data.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        st.st_mode = libc::S_IFBLK | 0o600;
        st.st_nlink = 1;
        st.st_rdev = libc::makedev(dev.0, dev.1);
        st.st_blksize = 4096;
        // SAFETY: view the POD struct as bytes to copy it out.
        let bytes = unsafe {
            std::slice::from_raw_parts(
                (&st as *const libc::stat).cast::<u8>(),
                std::mem::size_of::<libc::stat>(),
            )
        };
        let reply = match task.write_mem(statbuf, bytes) {
            Ok(()) => Reply::Value(0),
            Err(_) => Reply::Errno(libc::EFAULT),
        };
        self.log(format!("stat({path}) -> block {}:{}", dev.0, dev.1));
        reply
    }

    fn mknod(&mut self, task: &Task, path_ptr: u64) -> Reply {
        let path = task.read_cstr(path_ptr).unwrap_or_default();
        let reply = if task.open_in_root(&path, libc::O_PATH, 0).is_ok() {
            Reply::Errno(libc::EEXIST)
        } else {
            match task.create_file_in_root(&path) {
                Ok(()) => Reply::Value(0),
                Err(e) => Reply::Errno(e.raw_os_error().unwrap_or(libc::EIO)),
            }
        };
        self.log(format!("mknod({path}) -> {reply:?}"));
        reply
    }

    fn uname(&self, task: &Task, buf: u64) -> Reply {
        // SAFETY: a zeroed utsname is valid; uname fills it.
        let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
        // SAFETY: uname into our own struct.
        if unsafe { libc::uname(&mut uts) } != 0 {
            return Reply::Errno(libc::EFAULT);
        }
        let mut release = [0 as libc::c_char; 65];
        for (dst, src) in release.iter_mut().zip(self.release.bytes().take(64)) {
            *dst = src as libc::c_char;
        }
        uts.release = release;
        uts.nodename = [0; 65];
        for (dst, src) in uts.nodename.iter_mut().zip(b"nmbl-simbox".iter()) {
            *dst = *src as libc::c_char;
        }
        // SAFETY: utsname is plain old data; view it as bytes to copy out.
        let bytes = unsafe {
            std::slice::from_raw_parts(
                (&uts as *const libc::utsname).cast::<u8>(),
                std::mem::size_of::<libc::utsname>(),
            )
        };
        match task.write_mem(buf, bytes) {
            Ok(()) => Reply::Value(0),
            Err(_) => Reply::Errno(libc::EFAULT),
        }
    }

    fn load_module(&mut self, image: &[u8]) -> Reply {
        let name = module_name(image).unwrap_or_else(|| "?".to_string());
        let reply = if self.loaded_modules.insert(name.clone()) {
            Reply::Value(0)
        } else {
            Reply::Errno(libc::EEXIST)
        };
        self.log(format!("init_module({name}) -> {reply:?}"));
        reply
    }

    fn kexec_file_load(&mut self, task: &Task, a: [u64; 6]) -> Reply {
        let kernel_fd = i32::try_from(a[0]).unwrap_or(-1);
        let initrd_fd = i32::try_from(a[1] as i64).unwrap_or(-1);
        let flags = a[4];
        let Ok(kfd) = task.get_fd(kernel_fd) else {
            return Reply::Errno(libc::EBADF);
        };
        // The task's own view of the path it opened.
        let kernel_path = std::fs::read_link(format!("/proc/{}/fd/{kernel_fd}", task.pid))
            .ok()
            .map(|p| p.to_string_lossy().into_owned());
        let kernel = read_fd_all(&kfd).unwrap_or_default();
        let initrd = if flags & KEXEC_FILE_NO_INITRAMFS != 0 || initrd_fd < 0 {
            None
        } else {
            task.get_fd(initrd_fd)
                .ok()
                .and_then(|fd| read_fd_all(&fd).ok())
        };
        let len = usize::try_from(a[2]).unwrap_or(0);
        let cmdline = task
            .read_mem(a[3], len)
            .map(|b| {
                String::from_utf8_lossy(b.split(|&c| c == 0).next().unwrap_or_default())
                    .into_owned()
            })
            .unwrap_or_default();
        self.log(format!(
            "kexec_file_load(kernel {} B, initrd {} B, cmdline {cmdline:?}) -> 0",
            kernel.len(),
            initrd.as_ref().map_or(0, Vec::len)
        ));
        self.kexec = Some(KexecLoad {
            kernel,
            kernel_path,
            initrd,
            cmdline,
        });
        Reply::Value(0)
    }

    fn reboot(&mut self, cmd: u64) -> (Reply, Option<Outcome>) {
        const KEXEC: u64 = 0x4558_4543;
        const RESTART: u64 = 0x0123_4567;
        const HALT: u64 = 0xCDEF_0123;
        const POWER_OFF: u64 = 0x4321_FEDC;
        let cmd = cmd & 0xffff_ffff;
        match cmd {
            KEXEC => match self.kexec.take() {
                Some(k) => (Reply::Value(0), Some(Outcome::Kexec(Box::new(k)))),
                None => (Reply::Errno(libc::EINVAL), None),
            },
            RESTART => (Reply::Value(0), Some(Outcome::Reboot)),
            HALT => (Reply::Value(0), Some(Outcome::Halt)),
            POWER_OFF => (Reply::Value(0), Some(Outcome::PowerOff)),
            _ => (Reply::Value(0), None),
        }
    }
}

fn bind_tree(task: &Task, src: &str, target: &str) -> Reply {
    match task.bind_mount(src, target) {
        Ok(()) => Reply::Value(0),
        Err(e) => Reply::Errno(e),
    }
}

/// Extract `name=` from a kernel module's `.modinfo`, possibly compressed —
/// NMBL decompresses before `init_module`, so the image is a plain ELF.
pub fn module_name(image: &[u8]) -> Option<String> {
    let needle = b"\0name=";
    let pos = image.windows(needle.len()).position(|w| w == needle)?;
    let rest = image.get(pos + needle.len()..)?;
    let end = rest.iter().position(|&b| b == 0)?;
    Some(String::from_utf8_lossy(rest.get(..end)?).into_owned())
}

/// Scenario trees the container sees under `/.simbox/…`, as `(host, container)`.
pub fn tree_mounts(s: &Scenario) -> Vec<(PathBuf, String)> {
    let mut v = Vec::new();
    for b in &s.block {
        if let Some(t) = &b.tree {
            v.push((s.path(t), format!("/.simbox/trees/{}", b.name)));
        }
    }
    for l in &s.luks {
        v.push((
            s.path(&l.mapper_tree),
            format!("/.simbox/mapper/{}", l.name),
        ));
    }
    v
}

#[cfg(test)]
#[allow(clippy::panic, reason = "tests assert")]
mod tests {
    use super::*;

    #[test]
    fn module_name_from_modinfo() {
        let mut img = b"\x7fELF....".to_vec();
        img.extend_from_slice(b"\0license=GPL\0name=dm_crypt\0vermagic=6.12\0");
        assert_eq!(module_name(&img).as_deref(), Some("dm_crypt"));
        assert_eq!(module_name(b"garbage"), None);
    }
}
