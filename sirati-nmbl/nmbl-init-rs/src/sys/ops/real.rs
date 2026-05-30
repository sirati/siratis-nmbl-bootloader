//! [`RealSys`]: the genuine [`SysOps`](super::SysOps) impl.
//!
//! Every method is a one-line forward to the existing free function that
//! already owns the logic and its `unsafe`; this struct adds no new
//! syscalls and no new `unsafe`. The async methods need the poller's
//! [`LocalSender`] (the non-blocking `waitpid` reap channel), so `RealSys`
//! borrows one for its lifetime.
//!
//! The boot spine threads `&mut RealSys` through and dispatches its
//! side-effecting calls through these forwards. The pre-runtime phase 1
//! (pseudo-fs mount) has no poller yet, so [`RealSys::sync_only`] builds
//! a sender-less instance whose sync `FsOps` methods work and whose async
//! methods are unreachable on that path (documented invariant).

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use nix::mount::MntFlags;

use crate::config::Config;
use crate::error::Result;
use crate::modules::ModuleSet;
use crate::sys::activation::ProcessOutcome;
use crate::sys::poller::LocalSender;
use crate::sys::pty::PtyChild;
use crate::ui::console::Console;
use crate::ui::{BootReporter, ProgressSink};

use super::{BlockOps, ConsoleOps, ExecOps, FsOps, KexecOps, KexecTarget, ModuleOps};

/// Genuine system-operations impl. Borrows the poller's [`LocalSender`] so
/// the async fork/exec and blkid forwarders can drive non-blocking
/// `waitpid` reaps.
///
/// The sender is optional: the pre-runtime phase 1 (pseudo-fs mount) runs
/// before the poller exists, but only uses the sync `FsOps` methods. A
/// [`RealSys::sync_only`] instance carries `None`; calling an async method
/// on it panics by the documented invariant that the pre-runtime path
/// never reaches one.
pub struct RealSys<'a> {
    sender: Option<&'a LocalSender>,
}

impl<'a> RealSys<'a> {
    /// Build a `RealSys` borrowing the runtime's poller sender. Use for
    /// every in-runtime boot phase (async fork/exec and blkid reaps).
    pub fn new(sender: &'a LocalSender) -> Self {
        Self {
            sender: Some(sender),
        }
    }

    /// Build a sender-less `RealSys` for the pre-runtime phase 1, whose
    /// only side effects are the sync `FsOps` mounts. The async methods
    /// must never be called on this instance — they `expect` the sender.
    pub fn sync_only() -> Self {
        Self { sender: None }
    }

    /// Resolve the poller sender for an async forward, panicking with a
    /// clear message if this is a `sync_only` instance. The invariant
    /// (pre-runtime path never calls an async op) makes this unreachable.
    fn sender(&self) -> &'a LocalSender {
        self.sender
            .expect("RealSys async op requires a poller sender (built via sync_only)")
    }
}

impl FsOps for RealSys<'_> {
    fn exists(&self, path: &Path) -> bool {
        path.try_exists().unwrap_or(false)
    }

    fn ensure_dir(&mut self, path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }

    fn mount(
        &mut self,
        source: Option<&Path>,
        target: &Path,
        fstype: &str,
        options: &str,
    ) -> Result<()> {
        crate::sys::mount::mount_fs(source, target, fstype, options)
    }

    fn umount(&mut self, target: &Path, flags: MntFlags) -> Result<()> {
        crate::sys::mount::umount(target, flags)
    }

    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }
}

impl BlockOps for RealSys<'_> {
    async fn wait_for_device(
        &mut self,
        device: &Path,
        timeout: Duration,
        operation: &str,
        progress: Option<&mut dyn ProgressSink>,
    ) -> Result<()> {
        crate::devices::wait_for(device, timeout, operation, progress).await
    }

    fn ensure_dev_node(&mut self, sysfs_entry: &Path, dev_path: &Path) -> Result<bool> {
        crate::sys::blkid::ensure_dev_node(sysfs_entry, dev_path)
    }

    async fn populate_disk_symlinks(&mut self) -> Result<Vec<PathBuf>> {
        crate::sys::blkid::populate_disk_by_symlinks(self.sender()).await
    }

    fn setup_loop(&mut self, file: &Path) -> Result<PathBuf> {
        crate::devices::setup_loop_device(file)
    }

    fn btrfs_scan(&mut self, devs: &[PathBuf]) -> Result<()> {
        crate::sys::btrfs::scan_devices(devs)
    }
}

impl ModuleOps for RealSys<'_> {
    fn load_module_set(
        &mut self,
        config: &Config,
        reporter: &mut BootReporter<'_, '_>,
        which: ModuleSet,
    ) -> Result<()> {
        crate::modules::load_module_set(config, reporter, which)
    }

    fn load_modules(
        &mut self,
        modules_dir: &Path,
        explicit: &[String],
        blacklist: &[String],
    ) -> Result<()> {
        crate::modules::load_modules(modules_dir, explicit, blacklist)
    }
}

impl ExecOps for RealSys<'_> {
    async fn run(
        &mut self,
        binary: &Path,
        argv: &[String],
        stdin_data: Option<&[u8]>,
    ) -> Result<ProcessOutcome> {
        crate::sys::activation::run(binary, argv, stdin_data, self.sender()).await
    }

    async fn run_capture(
        &mut self,
        binary: &Path,
        argv: &[String],
    ) -> Result<(ProcessOutcome, Vec<u8>)> {
        crate::sys::activation::run_capture(binary, argv, self.sender()).await
    }

    fn spawn_shell(&mut self, shell_path: &Path, cols: u16, rows: u16) -> Result<PtyChild> {
        crate::sys::pty::spawn_shell(shell_path, cols, rows)
    }
}

impl KexecOps for RealSys<'_> {
    fn kexec_load(&mut self, target: KexecTarget, cmdline: &str, flags: u32) -> Result<()> {
        match target {
            KexecTarget::MultiFile { kernel, initrd } => {
                // Thin forward: load kernel + initrd with an empty extra
                // cpio fragment. The keyfile/log injection that
                // `boot::kexec_into` layers on top is built by the boot
                // core, which passes the fragment here in the next phase.
                crate::sys::kexec::load_with_extra_initrd_cpio(
                    &kernel,
                    &initrd,
                    &[],
                    cmdline,
                    flags,
                )
            }
            KexecTarget::Uki { path } => crate::sys::kexec::load(&path, None, cmdline, flags),
        }
    }
}

impl ConsoleOps for RealSys<'_> {
    fn open_console(&mut self, config: &Config, panic_recovery: bool) -> Result<Box<dyn Console>> {
        crate::ui::console::open_console(config, panic_recovery)
    }
}
