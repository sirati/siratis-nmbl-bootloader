//! System-operation trait family (`SysOps`).
//!
//! These focused traits abstract every side-effecting syscall the boot
//! procedure performs (mount, block-device wait, module load, fork/exec,
//! kexec, console open) behind a generic `<S: SysOps>` seam. The genuine
//! [`RealSys`] impl in [`real`] forwards each method one-to-one to the
//! existing free function that already owns the logic; a later phase
//! threads `<S: SysOps>` through the boot core and adds a dry-run impl for
//! `--validate-initrm`.
//!
//! Dispatch is always generic (`<S: SysOps>`), never `dyn SysOps`, so the
//! async methods can be native `async fn` in trait with no `async-trait`
//! crate and no boxing. [`FsOps`] is deliberately kept object-safe (no
//! async / generic methods) because later phases hand `&mut dyn FsOps` to
//! the splash and pty init helpers.
//!
//! The boot spine threads `<S: SysOps>` through and routes module loads,
//! mounts, block ops, and activation exec through `RealSys`. Kexec, shell
//! spawn, and console bring-up are routed in later phases.

pub mod real;

use std::io;
use std::path::{Path, PathBuf};

use nix::mount::MntFlags;

use crate::config::Config;
use crate::error::Result;
use crate::modules::ModuleSet;
use crate::sys::activation::ProcessOutcome;
use crate::sys::pty::PtyChild;
use crate::ui::console::Console;
use crate::ui::{BootReporter, ProgressSink};

pub use real::RealSys;

/// Which image shape [`KexecOps::kexec_load`] should load.
///
/// `MultiFile` is the classic separate-files kernel + initrd pair (the
/// path NMBL takes today); `Uki` is a single bundled UKI image. Carrying
/// the discriminant in the call lets the dry-run impl record the intended
/// target without a second method per shape.
pub enum KexecTarget {
    /// Separate kernel image and initrd file.
    MultiFile {
        /// Kernel image path.
        kernel: PathBuf,
        /// Initrd file path.
        initrd: PathBuf,
    },
    /// A single bundled UKI image.
    Uki {
        /// UKI image path.
        path: PathBuf,
    },
}

/// Filesystem syscalls: existence, directory creation, mount/umount, read.
///
/// Object-safe by construction (all-sync, no generics) so later phases can
/// pass `&mut dyn FsOps` into the splash / pty init helpers.
pub trait FsOps {
    /// `true` if `path` exists (a stat error collapses to `false`).
    fn exists(&self, path: &Path) -> bool;
    /// Create `path` and any missing parents.
    fn ensure_dir(&mut self, path: &Path) -> io::Result<()>;
    /// Mount a filesystem — see [`crate::sys::mount::mount_fs`].
    fn mount(
        &mut self,
        source: Option<&Path>,
        target: &Path,
        fstype: &str,
        options: &str,
    ) -> Result<()>;
    /// Unmount `target` with `flags` — see [`crate::sys::mount::umount`].
    fn umount(&mut self, target: &Path, flags: MntFlags) -> Result<()>;
    /// Read the whole file at `path`.
    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>>;
}

/// Block-device operations: device-readiness wait, node creation, by-*
/// symlink population, loop setup, btrfs scan.
///
/// The `async fn`s are sound without `Send` bounds: every consumer is the
/// single-threaded `LocalRuntime` and dispatch is always generic
/// (`<S: SysOps>`), never `dyn`. The auto-trait-bound warning the lint
/// raises does not apply.
#[allow(async_fn_in_trait)]
pub trait BlockOps {
    /// Poll until `device` appears — see [`crate::devices::wait_for`].
    async fn wait_for_device(
        &mut self,
        device: &Path,
        timeout: std::time::Duration,
        operation: &str,
        progress: Option<&mut dyn ProgressSink>,
    ) -> Result<()>;
    /// Ensure a `/dev/<name>` node exists for the sysfs block entry — see
    /// [`crate::sys::blkid::ensure_dev_node`].
    fn ensure_dev_node(&mut self, sysfs_entry: &Path, dev_path: &Path) -> Result<bool>;
    /// Populate `/dev/disk/by-*` symlinks and return btrfs members — see
    /// [`crate::sys::blkid::populate_disk_by_symlinks`].
    async fn populate_disk_symlinks(&mut self) -> Result<Vec<PathBuf>>;
    /// Attach a loop device over `file` — see
    /// [`crate::devices::setup_loop_device`].
    fn setup_loop(&mut self, file: &Path) -> Result<PathBuf>;
    /// Issue `BTRFS_IOC_SCAN_DEV` over `devs` — see
    /// [`crate::sys::btrfs::scan_devices`].
    fn btrfs_scan(&mut self, devs: &[PathBuf]) -> Result<()>;
}

/// Kernel-module loading.
pub trait ModuleOps {
    /// Load a named module set — see [`crate::modules::load_module_set`].
    fn load_module_set(
        &mut self,
        config: &Config,
        reporter: &mut BootReporter<'_, '_>,
        which: ModuleSet,
    ) -> Result<()>;
    /// Reporter-free loader — see [`crate::modules::load_modules`].
    fn load_modules(
        &mut self,
        modules_dir: &Path,
        explicit: &[String],
        blacklist: &[String],
    ) -> Result<()>;
}

/// Fork/exec operations: run, run-and-capture, fork a shell on a PTY.
///
/// `async fn`s carry no `Send` bound for the same single-threaded reason
/// as [`BlockOps`].
#[allow(async_fn_in_trait)]
pub trait ExecOps {
    /// Fork+exec `binary`, inherit stdio — see
    /// [`crate::sys::activation::run`].
    async fn run(
        &mut self,
        binary: &Path,
        argv: &[String],
        stdin_data: Option<&[u8]>,
    ) -> Result<ProcessOutcome>;
    /// Fork+exec `binary` and capture stdout — see
    /// [`crate::sys::activation::run_capture`].
    async fn run_capture(
        &mut self,
        binary: &Path,
        argv: &[String],
    ) -> Result<(ProcessOutcome, Vec<u8>)>;
    /// Fork a shell on a fresh PTY pair — see
    /// [`crate::sys::pty::spawn_shell`].
    fn spawn_shell(&mut self, shell_path: &Path, cols: u16, rows: u16) -> Result<PtyChild>;
}

/// Kexec image load.
pub trait KexecOps {
    /// Load `target` into the kexec image slot — see
    /// [`crate::sys::kexec`].
    fn kexec_load(&mut self, target: KexecTarget, cmdline: &str, flags: u32) -> Result<()>;
}

/// Console bring-up.
pub trait ConsoleOps {
    /// Open the boot console — see [`crate::ui::console::open_console`].
    fn open_console(&mut self, config: &Config, panic_recovery: bool) -> Result<Box<dyn Console>>;
}

/// Super-trait bundling every focused capability. Boot code that needs the
/// full system takes `<S: SysOps>`; helpers that need one slice take the
/// focused trait. The blanket impl means any type implementing all six
/// focused traits is a `SysOps` for free.
pub trait SysOps: FsOps + BlockOps + ModuleOps + ExecOps + KexecOps + ConsoleOps {}

impl<T: FsOps + BlockOps + ModuleOps + ExecOps + KexecOps + ConsoleOps> SysOps for T {}
