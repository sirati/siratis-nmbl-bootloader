//! [`DryRunSys`]: the side-effect-free [`SysOps`](super::SysOps) impl
//! used by `--validate-initrm`.
//!
//! It mirrors the SHAPE of [`super::RealSys`] method-for-method, but
//! replaces every side effect with a presence check against a
//! [`ClosureView`] over an extracted initramfs (or `/` at runtime). When
//! an op needs a file the closure lacks, it records a [`MissingFile`]
//! instead of performing the syscall; the `--validate-initrm` mode (next
//! phase) lists every finding and exits non-zero.
//!
//! Nothing here forks, mounts, mknods, loads a module, kexecs, or opens a
//! device. Async methods return immediately (never block on a real
//! device). The dispatch is the same generic `<S: SysOps>` seam the real
//! impl uses — `DryRunSys` is a concrete type, never `dyn SysOps`.
//!
//! Findings are recorded on `&mut self` at the op level wherever the
//! trait method takes `&mut self`. The two `&self` methods on
//! [`FsOps`] (`exists`, `read_file`) do NOT record findings: `exists`
//! is a pure predicate, and `read_file` propagates the `io::Error` so
//! the genuine caller's fallback (optional splash assets degrade; the
//! `&mut self` op that drove the read records the finding) decides.

mod closure;
mod report;
mod scenario;

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use nix::mount::MntFlags;

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::modules::ModuleSet;
use crate::sys::activation::ProcessOutcome;
use crate::sys::pty::PtyChild;
use crate::ui::console::{Console, NoopConsole};
use crate::ui::{BootReporter, ProgressSink};

use crate::sys::blkid::BLKID_BINARY;

use super::{BlockOps, ConsoleOps, ExecOps, FsOps, KexecOps, KexecTarget, ModuleOps};

pub use closure::ClosureView;
pub use report::{Findings, MissingFile};
pub use scenario::{DryRunScenario, ExecRole};

/// Side-effect-free [`SysOps`](super::SysOps) impl over an initramfs
/// closure. Records [`MissingFile`] findings for every file the boot
/// would touch that the closure lacks; performs no real side effects.
pub struct DryRunSys {
    /// Presence/read oracle over the extracted initramfs (or `/`).
    closure: ClosureView,
    /// Which boot path is being validated; scripts exec outcomes.
    scenario: DryRunScenario,
    /// Accumulated findings; listed + non-zero exit by the mode.
    findings: Findings,
    /// Device nodes the boot WOULD create (via `ensure_dev_node` /
    /// `populate_disk_symlinks`); a later `wait_for_device` on one of
    /// these passes because the boot itself produces it.
    synthetic_devices: HashSet<PathBuf>,
    /// Monotonic counter handing out fake `/dev/loopN` paths so
    /// `setup_loop` is deterministic and side-effect-free.
    loop_counter: u32,
}

impl DryRunSys {
    /// Build a dry-run over `closure`, validating `scenario`.
    #[must_use]
    pub fn new(closure: ClosureView, scenario: DryRunScenario) -> Self {
        Self {
            closure,
            scenario,
            findings: Findings::new(),
            synthetic_devices: HashSet::new(),
            loop_counter: 0,
        }
    }

    /// Borrow the recorded findings (for the mode's listing / tests).
    #[must_use]
    pub fn findings(&self) -> &Findings {
        &self.findings
    }

    /// Consume `self`, returning the findings.
    #[must_use]
    pub fn into_findings(self) -> Findings {
        self.findings
    }

    /// Borrow the closure oracle (used by the closure-probing helpers in
    /// `closure.rs`).
    pub(super) fn closure(&self) -> &ClosureView {
        &self.closure
    }

    /// Record one finding (used across the split helper modules).
    pub(super) fn record(&mut self, finding: MissingFile) {
        self.findings.push(finding);
    }

    /// `true` if `dev` is a device the boot does not have to ship in the
    /// closure: a `/dev/*` node the kernel/devtmpfs creates, or one this
    /// dry-run already seeded via `ensure_dev_node`/`populate_disk_symlinks`.
    fn device_available(&self, dev: &Path) -> bool {
        self.closure.exists(dev) || self.synthetic_devices.contains(dev) || dev.starts_with("/dev/")
    }

    /// Presence-check `binary`; record a `"<op>"` finding if absent.
    fn require_binary(&mut self, op: &'static str, binary: &Path, context: impl Into<String>) {
        if !self.closure.exists(binary) {
            self.findings
                .push(MissingFile::new(op, binary, context.into()));
        }
    }
}

impl FsOps for DryRunSys {
    fn exists(&self, path: &Path) -> bool {
        // Mirror RealSys, but also treat kernel-/dry-run-created device
        // nodes as present so a probe of e.g. /dev/mapper/root resolves.
        self.closure.exists(path) || self.device_available(path)
    }

    fn ensure_dir(&mut self, _path: &Path) -> io::Result<()> {
        // A directory is always "creatable"; the dry-run never mkdirs.
        Ok(())
    }

    fn mount(
        &mut self,
        source: Option<&Path>,
        _target: &Path,
        fstype: &str,
        _options: &str,
    ) -> Result<()> {
        // A file-backed source (loop mount, image) must be in the closure;
        // a pseudo-source (`devpts`, `proc`, …) or a device node is not a
        // shippable file. Heuristic: a source that starts with `/` and is
        // NOT a `/dev/*` node is a real file we can presence-check.
        if let Some(src) = source
            && src.is_absolute()
            && !src.starts_with("/dev/")
            && !self.closure.exists(src)
        {
            self.findings.push(MissingFile::new(
                "mount",
                src,
                format!("mount source for fstype {fstype} not in initrd"),
            ));
        }
        // fstype driver resolution is intentionally lenient: pseudo-fs are
        // built-in, and a real fstype's module was already validated via
        // load_module*. Recording it here would risk a false negative, so
        // we pass. NEVER calls mount(2).
        Ok(())
    }

    fn umount(&mut self, _target: &Path, _flags: MntFlags) -> Result<()> {
        Ok(())
    }

    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>> {
        // Propagate the error; the &mut self op that drove this read (or
        // the genuine caller's optional-read fallback) decides whether
        // the absence is a finding.
        self.closure.read_file(path)
    }
}

impl BlockOps for DryRunSys {
    async fn wait_for_device(
        &mut self,
        device: &Path,
        _timeout: Duration,
        operation: &str,
        _progress: Option<&mut dyn ProgressSink>,
    ) -> Result<()> {
        // Trivially ready — NEVER block on a real device. Pass if the
        // closure has it, the boot seeds it, or it is a kernel-created
        // /dev node; otherwise record an informational finding.
        if !self.device_available(device) {
            self.findings.push(MissingFile::new(
                "wait_for_device",
                device,
                format!("device awaited by {operation} is neither in initrd nor kernel-created"),
            ));
        }
        Ok(())
    }

    fn ensure_dev_node(&mut self, sysfs_entry: &Path, dev_path: &Path) -> Result<bool> {
        // Verify the sysfs SOURCE so the node COULD be made; never mknod.
        // Seed dev_path so later wait_for_device on it passes.
        let dev_attr = sysfs_entry.join("dev");
        if !self.closure.exists(sysfs_entry) && !self.closure.exists(&dev_attr) {
            self.findings.push(MissingFile::new(
                "ensure_dev_node",
                sysfs_entry,
                format!("sysfs source for {} missing", dev_path.display()),
            ));
        }
        self.synthetic_devices.insert(dev_path.to_path_buf());
        Ok(true)
    }

    async fn populate_disk_symlinks(&mut self) -> Result<Vec<PathBuf>> {
        // The genuine pass execs `blkid` then writes by-* symlinks. We
        // cannot enumerate real disks in a sandbox, so: verify the blkid
        // binary is shippable, then return an empty btrfs list. Any
        // by-partlabel/by-uuid path the config later awaits is seeded by
        // the device-availability `/dev/*` prefix rule, not here.
        self.require_binary(
            "run",
            Path::new(BLKID_BINARY),
            "blkid binary needed to populate /dev/disk/by-* symlinks",
        );
        Ok(Vec::new())
    }

    fn setup_loop(&mut self, file: &Path) -> Result<PathBuf> {
        // The backing file must be in the closure; never attach a loop.
        if !self.closure.exists(file) {
            self.findings.push(MissingFile::new(
                "setup_loop",
                file,
                "loop-device backing file not in initrd",
            ));
        }
        let dev = PathBuf::from(format!("/dev/loop{}", self.loop_counter));
        self.loop_counter = self.loop_counter.saturating_add(1);
        self.synthetic_devices.insert(dev.clone());
        Ok(dev)
    }

    fn btrfs_scan(&mut self, _devs: &[PathBuf]) -> Result<()> {
        // Pure ioctl over already-present devices; no file to check.
        Ok(())
    }
}

impl ModuleOps for DryRunSys {
    fn load_module_set(
        &mut self,
        config: &Config,
        _reporter: &mut BootReporter<'_, '_>,
        which: ModuleSet,
    ) -> Result<()> {
        let list = match which {
            ModuleSet::Early => &config.kernel_modules.early,
            ModuleSet::Explicit => &config.kernel_modules.explicit,
        };
        let dir = config.kernel_modules.modules_dir.clone();
        let blacklist = config.kernel_modules.blacklist.clone();
        let list = list.clone();
        self.dryrun_modules(&dir, &list, &blacklist);
        Ok(())
    }

    fn load_modules(
        &mut self,
        modules_dir: &Path,
        explicit: &[String],
        blacklist: &[String],
    ) -> Result<()> {
        self.dryrun_modules(modules_dir, explicit, blacklist);
        Ok(())
    }
}

impl ExecOps for DryRunSys {
    async fn run(
        &mut self,
        binary: &Path,
        _argv: &[String],
        _stdin_data: Option<&[u8]>,
    ) -> Result<ProcessOutcome> {
        self.require_binary("run", binary, "activation/exec binary not in initrd");
        // An execed binary here is an activation tool; its scripted
        // outcome routes the boot under ErrorToErrorScreen.
        Ok(self.scenario.scripted_outcome(ExecRole::Activation))
    }

    async fn run_with_tick(
        &mut self,
        binary: &Path,
        _argv: &[String],
        _stdin_data: Option<&[u8]>,
        _tick: &mut dyn FnMut(),
    ) -> Result<ProcessOutcome> {
        // Presence-check only: NEVER fork (so the LUKS `cryptsetup` exec is
        // not run in the sandbox) and NEVER tick (no live console). The
        // outcome is the same activation-role script `run` uses, so the
        // boot routes identically under each scenario.
        self.require_binary(
            "run_with_tick",
            binary,
            "activation/exec binary not in initrd",
        );
        Ok(self.scenario.scripted_outcome(ExecRole::Activation))
    }

    async fn run_capture(
        &mut self,
        binary: &Path,
        _argv: &[String],
    ) -> Result<(ProcessOutcome, Vec<u8>)> {
        self.require_binary("run_capture", binary, "capture binary not in initrd");
        // A captured exec is a read-only probe; never failed by a
        // scenario, and the dry-run has no real stdout to return.
        Ok((self.scenario.scripted_outcome(ExecRole::Probe), Vec::new()))
    }

    fn spawn_shell(
        &mut self,
        _sealed: crate::policy::Sealed,
        shell_path: &Path,
        _cols: u16,
        _rows: u16,
    ) -> Result<PtyChild> {
        // PtyChild owns a real master OwnedFd + child Pid; it cannot be
        // faked without an actual fork, which the dry-run must NOT do. So
        // we run the SAME preflight the genuine path does (shell presence
        // + devpts mount + ptmx) recording findings, then return the typed
        // `NmblError::DryRunShellPreflight` signal. The Phase-5
        // RawShell/PrettyShell drivers treat `matches!(err,
        // NmblError::DryRunShellPreflight)` as "shell preflight complete,
        // no fork performed", i.e. success.
        if !self.closure.exists(shell_path) {
            self.findings.push(MissingFile::new(
                "spawn_shell",
                shell_path,
                "emergency shell binary not in initrd",
            ));
        }
        // devpts mount + /dev/ptmx are the other preflight deps; ptmx is a
        // kernel/devtmpfs node so its absence in a closure is benign.
        let _ = self.mount(
            Some(Path::new("devpts")),
            Path::new("/dev/pts"),
            "devpts",
            "",
        );
        Err(NmblError::DryRunShellPreflight)
    }
}

impl KexecOps for DryRunSys {
    fn kexec_load(
        &mut self,
        target: KexecTarget,
        _verified_fds: super::VerifiedKexecFds<'_>,
        _extra_cpio: &[u8],
        _cmdline: &str,
        _flags: u32,
    ) -> Result<()> {
        // Presence-check the image(s); NO-OP the sync()+settle; NEVER
        // kexec/reboot. The deep PE-section UKI validation is a separate
        // check (validate/initrm via sys/uki) done next phase — here we
        // only presence-check the UKI path.
        match target {
            KexecTarget::MultiFile { kernel, initrd } => {
                if !self.closure.exists(&kernel) {
                    self.findings.push(MissingFile::new(
                        "kexec_load",
                        kernel,
                        "kexec kernel image not in initrd",
                    ));
                }
                if !self.closure.exists(&initrd) {
                    self.findings.push(MissingFile::new(
                        "kexec_load",
                        initrd,
                        "kexec initrd not in initrd",
                    ));
                }
            }
            KexecTarget::Uki { path } => {
                if !self.closure.exists(&path) {
                    self.findings.push(MissingFile::new(
                        "kexec_load",
                        path,
                        "kexec UKI image not in initrd",
                    ));
                }
            }
        }
        Ok(())
    }
}

impl ConsoleOps for DryRunSys {
    fn open_console(&mut self, config: &Config, _panic_recovery: bool) -> Result<Box<dyn Console>> {
        self.probe_console_files(config);
        Ok(Box::new(NoopConsole::new()))
    }
}

#[cfg(test)]
mod tests;
