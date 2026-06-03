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

pub mod dryrun;
pub mod real;

use std::io;
use std::os::fd::BorrowedFd;
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
    /// Open `path` read-only, returning the live handle. The secure-boot
    /// verify pipeline streams-hashes this fd and hands the SAME fd to
    /// kexec, so the dry-run impl must return a real readable fd: it opens
    /// the [`ClosureView`](crate::sys::ops::dryrun::ClosureView)-mapped path
    /// read-only so the ML-DSA verify runs against the shipped bytes
    /// (side-effect-free). An absent file is an `io::Error`.
    fn open_ro(&self, path: &Path) -> io::Result<std::fs::File>;
    /// Write `contents` to `path` (sysfs firmware-load trigger,
    /// unsealed-mapper registry, rescue sentinel). Dry-run no-ops + records.
    fn write_file(&mut self, path: &Path, contents: &[u8]) -> io::Result<()>;
    /// Remove `path` (registry / sentinel cleanup). Dry-run no-ops + records.
    fn remove_file(&mut self, path: &Path) -> io::Result<()>;
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
    /// Tick-aware variant of [`run`](ExecOps::run): while the child runs,
    /// `tick` is invoked every reap slice so a UI spinner keeps animating
    /// during a slow unlock (e.g. Argon2id LUKS). Routes the LUKS
    /// activation's spinner-reaping exec through the same seam every other
    /// activation uses, so the dry-run can presence-check the binary
    /// without forking it (and without ticking). See
    /// [`crate::sys::activation::run_with_tick`].
    async fn run_with_tick(
        &mut self,
        binary: &Path,
        argv: &[String],
        stdin_data: Option<&[u8]>,
        tick: &mut dyn FnMut(),
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
    ///
    /// Requires the [`Sealed`](crate::policy::Sealed) witness by value: the
    /// real fork/execve waist this routes to cannot be reached until
    /// `policy::seal_secrets` has capped the lock PCR and closed every
    /// TPM-unsealed mapper (re-audit C-1). The witness is threaded THROUGH the
    /// ops seam so routing the spawn through the abstraction cannot bypass the
    /// seal; a dry-run impl consumes it but never forks.
    fn spawn_shell(
        &mut self,
        sealed: crate::policy::Sealed,
        shell_path: &Path,
        cols: u16,
        rows: u16,
    ) -> Result<PtyChild>;
}

/// The PINNED, already-verified kernel + initrd source fds the secure-boot
/// verify pipeline opened. When `Some`, [`KexecOps::kexec_load`] loads the
/// image straight from these fds (FIX-02 / MED-1 / LOW-A) instead of
/// re-opening either by path, so the kernel run + the initrd unpacked are
/// byte-identical to the ones that were verified and measured.
pub type VerifiedKexecFds<'a> = Option<(BorrowedFd<'a>, BorrowedFd<'a>)>;

/// Kexec image load.
pub trait KexecOps {
    /// Load `target` into the kexec image slot, splicing `extra_cpio`
    /// (keyfiles + log fragment) after the system initrd — see
    /// [`crate::sys::kexec`]. The genuine impl also performs the
    /// pre-handoff `sync(2)` + settle so a dry-run impl can no-op it.
    ///
    /// `verified_fds` carries the secure-boot verify pipeline's pinned
    /// kernel+initrd fds (kernel, initrd) when present; the genuine impl
    /// loads from those exact fds rather than re-opening by path so the
    /// loaded image is byte-identical to the verified+measured one. A
    /// dry-run impl ignores the fds and only presence-checks the paths.
    fn kexec_load(
        &mut self,
        target: KexecTarget,
        verified_fds: VerifiedKexecFds<'_>,
        extra_cpio: &[u8],
        cmdline: &str,
        flags: u32,
    ) -> Result<()>;
}

/// Console bring-up.
pub trait ConsoleOps {
    /// Open the boot console — see [`crate::ui::console::open_console`].
    fn open_console(&mut self, config: &Config, panic_recovery: bool) -> Result<Box<dyn Console>>;
}

/// TPM 2.0 hardware operations: presence, raw transact, PCR extend, the
/// Secure-Boot efivar state, and the lock-PCR poison cap.
///
/// [`RealSys`] forwards each method to the genuine `crate::tpm::*` op (real
/// `/dev/tpmrm0` / efivarfs I/O); [`DryRunSys`](dryrun::DryRunSys) makes
/// `tpm_present` synthetic, reads the SB-state from the closure (or
/// degrades), and NO-OPS every mutating op (`tpm_transmit`, `pcr_extend`,
/// `cap_lock_pcr`) so `--validate-initrm` can never open a real TPM, extend
/// a real PCR, or poison the irreversible lock PCR (Property-6). Routing the
/// seal's cap through this seam is what guarantees the dry-run cannot poison
/// a real PCR.
pub trait TpmOps {
    /// `true` iff a TPM is present (deterministic `/sys/class/tpm/tpm0`
    /// sysfs check) — see [`crate::tpm::tpm_present`].
    fn tpm_present(&self) -> bool;
    /// Round-trip a marshaled command frame to a response frame through the
    /// resource-manager device — see [`crate::tpm::transport::TpmDevice`].
    fn tpm_transmit(&mut self, command: &[u8]) -> Result<Vec<u8>>;
    /// `TPM2_PCR_Extend` of `digest` (SHA-256 bank) into PCR `index` — see
    /// [`crate::tpm::pcr_extend`].
    fn pcr_extend(&mut self, index: u32, digest: &[u8]) -> Result<()>;
    /// Read the authoritative `SecureBoot` efivar state — see
    /// [`crate::tpm::sbstate::read_secure_boot_efivar`].
    fn read_sb_state(&self) -> crate::tpm::SbEfiState;
    /// Cap (poison) the lock PCR with the committed relock poison, returning
    /// the rich [`CapOutcome`](crate::tpm::CapOutcome) the seal policy
    /// consumes — see [`crate::tpm::cap_lock_pcr`]. The dry-run NEVER
    /// performs the irreversible extend.
    fn cap_lock_pcr(&mut self) -> crate::tpm::CapOutcome;
}

/// Super-trait bundling every focused capability. Boot code that needs the
/// full system takes `<S: SysOps>`; helpers that need one slice take the
/// focused trait. The blanket impl means any type implementing all seven
/// focused traits is a `SysOps` for free.
pub trait SysOps: FsOps + BlockOps + ModuleOps + ExecOps + KexecOps + ConsoleOps + TpmOps {}

impl<T: FsOps + BlockOps + ModuleOps + ExecOps + KexecOps + ConsoleOps + TpmOps> SysOps for T {}
