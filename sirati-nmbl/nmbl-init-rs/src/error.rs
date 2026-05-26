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

    #[error("recovered from panic (report at {report_path})")]
    Panicked { report_path: PathBuf },

    #[error("failed to exec emergency shell: {source}")]
    Shell {
        #[source]
        source: nix::Error,
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
