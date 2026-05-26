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

    #[error("mount({target}) failed: {source}")]
    Mount {
        #[source]
        source: nix::Error,
        target: PathBuf,
    },

    #[error("umount({target}) failed: {source}")]
    Umount {
        #[source]
        source: nix::Error,
        target: PathBuf,
    },

    #[error("kernel module {name} failed to load: {source}")]
    Module {
        #[source]
        source: nix::Error,
        name: String,
    },

    #[error("kexec {stage} failed: {source}")]
    Kexec {
        #[source]
        source: nix::Error,
        stage: &'static str,
    },

    #[error("required block device {device} did not appear in time")]
    DeviceTimeout { device: PathBuf },

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
    Panicked {
        report_path: PathBuf,
        recovered: String,
    },

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
