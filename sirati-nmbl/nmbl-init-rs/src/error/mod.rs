use std::error::Error;
use std::fmt::Write as _;
use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum NmblError {
    #[error("config file {path} is not valid TOML: {source}")]
    Config {
        #[source]
        source: toml::de::Error,
        path: PathBuf,
    },

    #[error("io error while {context}: {source}")]
    Io {
        #[source]
        source: std::io::Error,
        context: String,
    },

    #[error("config invalid ({context}): {reason}")]
    ConfigInvalid { reason: String, context: String },

    #[error("mount({src:?} -> {dst}, type={fstype}) failed: {source}", src = src.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "<none>".to_string()))]
    Mount {
        src: Option<PathBuf>,
        dst: PathBuf,
        fstype: String,
        #[source]
        source: nix::Error,
    },

    #[error("umount({dst}) failed: {source}")]
    Umount {
        dst: PathBuf,
        #[source]
        source: nix::Error,
    },

    /// Catastrophic kernel-module load failure: the file could not be
    /// opened (missing, permission denied, …), the dep graph references
    /// an unknown module, or `modules.dep` describes a cycle. This
    /// variant is reserved for situations that no `nmbl` install can
    /// possibly recover from — it is **not** produced when the running
    /// kernel merely refuses a particular module via `EOPNOTSUPP`,
    /// `ENOEXEC`, or `ENODEV`; those are logged as warnings and the
    /// boot is allowed to continue (see `sys::module::LoadOutcome`).
    #[error("kernel module {name} (path {path}) failed to load: {source}")]
    Module {
        name: String,
        path: PathBuf,
        #[source]
        source: nix::Error,
    },

    #[error("kexec_file_load failed (kernel={kernel}, initrd={initrd:?}): {source}", initrd = initrd.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "<none>".to_string()))]
    KexecLoad {
        kernel: PathBuf,
        initrd: Option<PathBuf>,
        #[source]
        source: nix::Error,
    },

    #[error("kexec {stage} returned (should not happen): {source}")]
    KexecReturned {
        stage: &'static str,
        #[source]
        source: nix::Error,
    },

    #[error("required block device {device} did not appear within {timeout_ms}ms")]
    DeviceTimeout { device: PathBuf, timeout_ms: u64 },

    #[error("no NixOS generations found under {searched}")]
    NoGenerations { searched: PathBuf },

    /// The system-root mountpoint NMBL scans for generations is not
    /// actually a mount — nothing is mounted there at all. Almost always
    /// means the operator dropped to the emergency screen and hasn't yet
    /// mounted the NixOS system root, or mounted it somewhere else.
    #[error("nothing is mounted at {mountpoint}")]
    SystemRootNotMounted { mountpoint: PathBuf },

    /// A filesystem *is* mounted at the system root, but it does not
    /// contain the `nix/var/nix/profiles` directory NMBL needs. Signals a
    /// bad hand-mount: the wrong filesystem, or the right one mounted at
    /// the wrong place. `path` is the missing profiles dir; `mountpoint`
    /// is where the system root is expected.
    #[error(
        "required profiles directory {path} does not exist (system root mounted at {mountpoint})"
    )]
    ProfilesDirMissing { path: PathBuf, mountpoint: PathBuf },

    #[error("TUI failed: {source}")]
    Tui {
        #[source]
        source: std::io::Error,
    },

    #[error("activation step {kind} failed: {source}")]
    Activation {
        kind: String,
        #[source]
        source: Box<NmblError>,
    },

    #[error("bootstrap stage {stage} failed: {source}")]
    Bootstrap {
        stage: &'static str,
        #[source]
        source: Box<NmblError>,
    },

    #[error("rescue stage {stage} failed: {source}")]
    Rescue {
        stage: &'static str,
        #[source]
        source: Box<NmblError>,
    },

    #[error("recovered from panic (report at {report_path})")]
    Panicked { report_path: PathBuf },

    #[error("failed to exec emergency shell: {source}")]
    Shell {
        #[source]
        source: nix::Error,
    },

    /// The operator pressed Esc on the boot-status screen while a
    /// blocking wait (device readiness, activation poll, …) was in
    /// flight. Surfaced by [`crate::ui::ProgressSink::tick`] returning
    /// [`crate::ui::TickOutcome::Aborted`]; the caller of the wait
    /// helper wraps the abort with a short `context` string ("waiting
    /// for /dev/sda1", "activation foo", …) so the emergency menu can
    /// tell the operator exactly which step they cut short.
    #[error("operator aborted: {context}")]
    OperatorAborted { context: String },

    /// The operator picked [Reboot] on the wrong-password modal during
    /// a `luks-password` activation. Plumbed up so `main::run_inner`
    /// can short-circuit to [`crate::terminal::TerminalAction::Reboot`]
    /// without dropping into the emergency menu first — the operator
    /// already made the call.
    #[error("operator chose reboot at wrong-password modal ({context})")]
    OperatorChoseReboot { context: String },

    /// The operator picked [Shell] on the wrong-password modal, the
    /// shell was opened, and the shell has now exited. The activation
    /// layer surfaces this so `main` routes through the standard
    /// emergency menu (where [Retry boot from config] re-runs phase 3
    /// and re-prompts for the passphrase).
    #[error("operator dropped to shell from wrong-password modal ({context})")]
    WrongPasswordShellExited { context: String },

    /// The ciborium-encoded `State` overflowed the fixed 16 KiB
    /// `state.bin` slot. Indicates an installer bug: state.bin grew a
    /// field the on-disk layout can't accommodate. Always treat as
    /// fatal — silently truncating would corrupt subsequent reads.
    /// Kept feature-free to avoid `#[cfg]` noise inside this enum.
    #[error("state.bin payload {encoded_len} bytes exceeds {max} byte slot")]
    StateTooLarge { encoded_len: usize, max: usize },

    /// `init_or_validate` decoded an existing `state.bin`, re-encoded
    /// it, and the byte representation diverged from the on-disk one
    /// (modulo trailing-zero padding). Signals schema drift between
    /// the installer and the on-disk file — the installer refuses to
    /// silently rewrite the file because doing so could mask a bug.
    #[error("state.bin at {path} did not round-trip through encode/decode")]
    StateRoundtripMismatch { path: PathBuf },

    /// Signature-verification failure on a trust path: a bad/missing/
    /// wrong-domain/wrong-key signature, a malformed sidecar, or an
    /// internal inconsistency in the verify pipeline. `stage` is a stable
    /// short tag naming the gate that refused (e.g. `"gen-kernel"`,
    /// `"rescue-sfs"`, `"sidecar-parse"`); `detail` carries operator-facing
    /// context. Every secure-boot refuse routes through here before
    /// `refuse_unsigned` turns it into a `RebootIntoRescue` (R-1).
    #[error("signature verification failed at {stage}: {detail}")]
    Signature { stage: &'static str, detail: String },

    /// A TPM 2.0 transport or protocol failure: the `/dev/tpmrm0`
    /// transact failed (IO), a marshal/unmarshal step rejected the
    /// frame, or — critically — the TPM returned a NON-success response
    /// code (FIX-27). `context` names the operation ("pcr_extend",
    /// "pcr_read", "transact", …); `reason` carries the wire/RC detail.
    /// The cap path treats every such failure as fail-closed
    /// (`CapOutcome::Failed`), never as a benign no-TPM.
    #[error("tpm protocol error ({context}): {reason}")]
    TpmProto { context: String, reason: String },

    /// The boot was REFUSED by the policy terminus (R-1 / R-13): an
    /// untrusted image, a failed priority/signature gate, a seal failure
    /// on a rescue path, or any other case routed through
    /// [`crate::policy::refuse_unsigned`]. Carries the originating
    /// `cause` so the refuse banner shows the full chain. The ONLY error
    /// the `run_tui_session` Err arm maps to
    /// [`crate::terminal::TerminalAction::RebootIntoRescue`]; by the time
    /// it is produced the lock PCR has been capped, every TPM-unsealed
    /// mapper closed, LUKS relocked, and the rescue sentinel written
    /// (best-effort), so reaching it means secrets are already sealed.
    #[error("boot refused by policy: {cause}")]
    PolicyRefused {
        #[source]
        cause: Box<NmblError>,
    },
}

pub type Result<T> = std::result::Result<T, NmblError>;

/// Walk the error's `.source()` chain and produce a single multi-line string
/// suitable for the emergency-shell banner. The head error is unindented; each
/// subsequent cause is indented under "caused by:".
pub fn format_chain(err: &dyn Error) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{err}");

    let mut current = err.source();
    while let Some(cause) = current {
        let _ = writeln!(out, "  caused by: {cause}");
        current = cause.source();
    }

    // Drop the trailing newline so callers can decide how to terminate.
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests;
