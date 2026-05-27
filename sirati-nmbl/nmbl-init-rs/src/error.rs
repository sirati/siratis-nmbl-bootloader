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
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;

    fn inner_no_generations() -> NmblError {
        NmblError::NoGenerations {
            searched: PathBuf::from("/sysroot/nix/var/nix/profiles"),
        }
    }

    #[test]
    fn bootstrap_display_mentions_stage_and_inner() {
        let e = NmblError::Bootstrap {
            stage: "load-toml",
            source: Box::new(inner_no_generations()),
        };
        let s = e.to_string();
        assert!(s.contains("bootstrap stage load-toml failed"), "{s}");
        assert!(s.contains("no NixOS generations found"), "{s}");
    }

    #[test]
    fn rescue_display_mentions_stage_and_inner() {
        let e = NmblError::Rescue {
            stage: "loop-alloc",
            source: Box::new(inner_no_generations()),
        };
        let s = e.to_string();
        assert!(s.contains("rescue stage loop-alloc failed"), "{s}");
        assert!(s.contains("no NixOS generations found"), "{s}");
    }

    #[test]
    fn bootstrap_source_chain_reaches_inner() {
        let inner = inner_no_generations();
        let inner_msg = inner.to_string();
        let e = NmblError::Bootstrap {
            stage: "mount-boot",
            source: Box::new(inner),
        };
        let src = Error::source(&e).expect("Bootstrap should expose a source");
        assert_eq!(src.to_string(), inner_msg);
    }

    #[test]
    fn rescue_source_chain_reaches_inner() {
        let inner = inner_no_generations();
        let inner_msg = inner.to_string();
        let e = NmblError::Rescue {
            stage: "http-fetch",
            source: Box::new(inner),
        };
        let src = Error::source(&e).expect("Rescue should expose a source");
        assert_eq!(src.to_string(), inner_msg);
    }

    #[test]
    fn format_chain_walks_bootstrap_then_rescue() {
        // Nest a Rescue inside a Bootstrap to prove format_chain follows
        // both layers transparently through the standard `Error::source`.
        let leaf = inner_no_generations();
        let mid = NmblError::Rescue {
            stage: "hash-mismatch",
            source: Box::new(leaf),
        };
        let top = NmblError::Bootstrap {
            stage: "read-config",
            source: Box::new(mid),
        };
        let formatted = format_chain(&top as &dyn Error);
        assert!(
            formatted.contains("bootstrap stage read-config"),
            "{formatted}"
        );
        assert!(
            formatted.contains("caused by: rescue stage hash-mismatch"),
            "{formatted}"
        );
        assert!(
            formatted.contains("caused by: no NixOS generations found"),
            "{formatted}"
        );
    }
}
