//! Optional QEMU firmware/security devices: an swtpm-backed TPM 2.0 and a
//! Secure-Boot-enforcing OVMF pflash.
//!
//! Both are ADDITIVE: a [`QemuConfig`](super::qemu::QemuConfig) with neither
//! configured produces a byte-identical QEMU invocation to the historical
//! TPM-less, non-Secure-Boot path. The TPM is driven by a per-run `swtpm`
//! sidecar whose lifetime (and state-directory cleanup) is owned by
//! [`SwtpmSidecar`], so every run starts on clean PCRs and no emulated TPM
//! ever outlives its VM.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::fs;
use tokio::process::{Child, Command};
use tracing::{debug, warn};

/// Emulated TPM device model presented to the guest.
///
/// QEMU exposes the swtpm-backed device through one of two front-ends; the
/// guest kernel drives whichever the firmware/OS expects. CRB is the modern
/// default for UEFI guests; TIS is the older interface some setups need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TpmKind {
    /// `-device tpm-tis,tpmdev=tpm0` — the legacy TIS interface.
    Tis,
    /// `-device tpm-crb,tpmdev=tpm0` — the Command-Response-Buffer interface.
    Crb,
}

impl TpmKind {
    /// The `-device` model string QEMU expects for this front-end.
    fn device_model(self) -> &'static str {
        match self {
            TpmKind::Tis => "tpm-tis",
            TpmKind::Crb => "tpm-crb",
        }
    }
}

/// Configuration for an emulated TPM 2.0, backed by a per-run `swtpm` sidecar.
///
/// When present on a [`QemuConfig`](super::qemu::QemuConfig),
/// [`QemuConfig::start`](super::qemu::QemuConfig::start) launches a
/// `swtpm socket` process with a freshly-created state directory and a Unix
/// socket under [`Self::state_dir`], then wires QEMU's `-chardev`/`-tpmdev`/
/// `-device` triple to it. The sidecar's lifetime is owned by the returned
/// [`QemuProcess`](super::qemu::QemuProcess): it is terminated and its state
/// directory removed when the process is stopped or dropped, so every run
/// starts from a fresh PCR state (the measured-boot tests require a clean TPM
/// per run).
#[derive(Debug, Clone)]
pub struct TpmConfig {
    /// Front-end device model to present to the guest.
    pub kind: TpmKind,
    /// Per-run directory for the swtpm state and control socket. Created on
    /// start and removed on teardown; must be unique per concurrent VM.
    pub state_dir: PathBuf,
}

/// Secure-Boot firmware configuration: a Secure-Boot-enforcing OVMF build plus
/// a writable, db-enrolled VARS copy, and `smm=on` on the machine type.
///
/// When present on a [`QemuConfig`](super::qemu::QemuConfig) this REPLACES the
/// pflash drives that the [`BootMode::Uefi`](super::qemu::BootMode::Uefi) arm
/// would otherwise emit, and switches the `-machine` argument to
/// `q35,…,smm=on` (System Management Mode is required for the firmware to
/// protect the Secure-Boot variables). The `code` blob is the `OVMFFull`
/// Secure-Boot-built `OVMF_CODE`; the `vars` blob must be a PER-RUN WRITABLE
/// copy of a db-enrolled `OVMF_VARS` (so the firmware will refuse an
/// unsigned/badly-signed EFI binary).
#[derive(Debug, Clone)]
pub struct SecureBoot {
    /// Path to the Secure-Boot OVMF code firmware (read-only pflash).
    pub code: PathBuf,
    /// Path to a writable, db-enrolled OVMF VARS copy (read-write pflash).
    pub vars: PathBuf,
}

impl SecureBoot {
    /// Append the Secure-Boot pflash drives (and the `secure=on` global) to
    /// `cmd`. The caller is responsible for the matching `smm=on` machine type.
    pub fn add_pflash_args(&self, cmd: &mut Command) {
        debug!("Using Secure-Boot OVMF firmware (smm=on)");
        cmd.arg("-global")
            .arg("driver=cfi.pflash01,property=secure,value=on")
            .arg("-drive")
            .arg(format!(
                "if=pflash,format=raw,unit=0,readonly=on,file={}",
                self.code.display()
            ))
            .arg("-drive")
            .arg(format!(
                "if=pflash,format=raw,unit=1,file={}",
                self.vars.display()
            ));
    }
}

/// A spawned swtpm sidecar and the per-run state directory it owns. Dropping
/// this kills the process (best-effort) and removes the state directory, so a
/// TPM never outlives its VM and the next run starts from clean PCRs.
pub struct SwtpmSidecar {
    child: Child,
    state_dir: PathBuf,
}

impl SwtpmSidecar {
    /// Terminate the swtpm process and remove its per-run state directory.
    /// Best-effort: errors are logged, not propagated, so teardown never
    /// blocks VM shutdown.
    pub async fn shutdown(mut self) {
        if let Err(e) = self.child.start_kill() {
            warn!("failed to signal swtpm: {e}");
        }
        let _ = self.child.wait().await;
        if let Err(e) = fs::remove_dir_all(&self.state_dir).await {
            debug!(
                "swtpm state dir {} already gone or unremovable: {e}",
                self.state_dir.display()
            );
        }
    }
}

impl Drop for SwtpmSidecar {
    fn drop(&mut self) {
        // Synchronous fallback for the drop-without-shutdown path: signal the
        // child and remove the state dir so nothing is left behind even if the
        // async `shutdown` was never awaited.
        let _ = self.child.start_kill();
        let _ = std::fs::remove_dir_all(&self.state_dir);
    }
}

/// Spawn a `swtpm socket` sidecar for `tpm`, appending QEMU's TPM
/// `-chardev`/`-tpmdev`/`-device` triple to `cmd`.
///
/// Creates a fresh per-run state directory and a Unix socket inside it
/// (`--tpm2` ⇒ a TPM 2.0), then points QEMU at that socket. The returned
/// [`SwtpmSidecar`] owns the process and the directory; dropping or
/// shutting it down terminates swtpm and removes the directory.
pub async fn spawn_swtpm(tpm: &TpmConfig, cmd: &mut Command) -> Result<SwtpmSidecar> {
    // Fresh state dir per run ⇒ clean PCRs; recreate from scratch so a stale
    // directory can never carry measurements between runs.
    if tpm.state_dir.exists() {
        fs::remove_dir_all(&tpm.state_dir)
            .await
            .with_context(|| format!("clearing swtpm state dir {}", tpm.state_dir.display()))?;
    }
    fs::create_dir_all(&tpm.state_dir)
        .await
        .with_context(|| format!("creating swtpm state dir {}", tpm.state_dir.display()))?;

    let sock = tpm.state_dir.join("swtpm-sock");
    debug!(
        "Starting swtpm sidecar (state {}, socket {})",
        tpm.state_dir.display(),
        sock.display()
    );

    let child = Command::new("swtpm")
        .arg("socket")
        .arg("--tpm2")
        .arg("--tpmstate")
        .arg(format!("dir={}", tpm.state_dir.display()))
        .arg("--ctrl")
        .arg(format!("type=unixio,path={}.ctrl", sock.display()))
        .arg("--server")
        .arg(format!("type=unixio,path={}", sock.display()))
        .arg("--flags")
        .arg("startup-clear")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("Failed to spawn swtpm (is `swtpm` on PATH?)")?;

    cmd.arg("-chardev")
        .arg(format!("socket,id=chrtpm,path={}", sock.display()))
        .arg("-tpmdev")
        .arg("emulator,id=tpm0,chardev=chrtpm")
        .arg("-device")
        .arg(format!("{},tpmdev=tpm0", tpm.kind.device_model()));

    Ok(SwtpmSidecar {
        child,
        state_dir: tpm.state_dir.clone(),
    })
}
