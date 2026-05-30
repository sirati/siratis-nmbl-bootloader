//! [`RealSys`]: the genuine [`SysOps`](super::SysOps) impl.
//!
//! Every method is a one-line forward to the existing free function that
//! already owns the logic and its `unsafe`; this struct adds no new
//! syscalls and no new `unsafe`. The async methods need the poller's
//! [`LocalSender`] (the non-blocking `waitpid` reap channel), so `RealSys`
//! borrows one for its lifetime.
//!
//! Unused this phase — the boot core still calls the free functions
//! directly. The next phase threads `<S: SysOps>` through and this becomes
//! the production impl; until then `#[allow(dead_code)]` keeps the
//! `-D warnings` gate green.

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
#[allow(dead_code)] // wired into the boot core in the next phase
pub struct RealSys<'a> {
    sender: &'a LocalSender,
}

#[allow(dead_code)] // wired into the boot core in the next phase
impl<'a> RealSys<'a> {
    /// Build a `RealSys` borrowing the runtime's poller sender.
    pub fn new(sender: &'a LocalSender) -> Self {
        Self { sender }
    }
}

#[allow(dead_code)] // wired into the boot core in the next phase
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

#[allow(dead_code)] // wired into the boot core in the next phase
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
        crate::sys::blkid::populate_disk_by_symlinks(self.sender).await
    }

    fn setup_loop(&mut self, file: &Path) -> Result<PathBuf> {
        crate::devices::setup_loop_device(file)
    }

    fn btrfs_scan(&mut self, devs: &[PathBuf]) -> Result<()> {
        crate::sys::btrfs::scan_devices(devs)
    }
}

#[allow(dead_code)] // wired into the boot core in the next phase
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

#[allow(dead_code)] // wired into the boot core in the next phase
impl ExecOps for RealSys<'_> {
    async fn run(
        &mut self,
        binary: &Path,
        argv: &[String],
        stdin_data: Option<&[u8]>,
    ) -> Result<ProcessOutcome> {
        crate::sys::activation::run(binary, argv, stdin_data, self.sender).await
    }

    async fn run_capture(
        &mut self,
        binary: &Path,
        argv: &[String],
    ) -> Result<(ProcessOutcome, Vec<u8>)> {
        crate::sys::activation::run_capture(binary, argv, self.sender).await
    }

    fn spawn_shell(&mut self, shell_path: &Path, cols: u16, rows: u16) -> Result<PtyChild> {
        crate::sys::pty::spawn_shell(shell_path, cols, rows)
    }
}

#[allow(dead_code)] // wired into the boot core in the next phase
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

#[allow(dead_code)] // wired into the boot core in the next phase
impl ConsoleOps for RealSys<'_> {
    fn open_console(&mut self, config: &Config, panic_recovery: bool) -> Result<Box<dyn Console>> {
        crate::ui::console::open_console(config, panic_recovery)
    }
}
