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

    /// The operator pressed Esc on the boot-status screen while a
    /// blocking wait (device readiness, activation poll, …) was in
    /// flight. Surfaced by [`crate::ui::ProgressSink::tick`] returning
    /// [`crate::ui::TickOutcome::Aborted`]; the caller of the wait
    /// helper wraps the abort with a short `context` string ("waiting
    /// for /dev/sda1", "activation foo", …) so the emergency menu can
    /// tell the operator exactly which step they cut short.
    #[error("operator aborted: {context}")]
    OperatorAborted { context: String },
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
    fn operator_aborted_display_mentions_context() {
        // The user-facing emergency banner reads this string verbatim,
        // so the exact "operator aborted: <context>" shape is a
        // contract. Pin it.
        let e = NmblError::OperatorAborted {
            context: "waiting for /dev/sda1".to_string(),
        };
        let s = e.to_string();
        assert_eq!(s, "operator aborted: waiting for /dev/sda1");
    }

    #[test]
    fn format_chain_renders_operator_aborted_single_line() {
        // OperatorAborted has no inner source — format_chain must emit
        // just the head error with no trailing "caused by:" line.
        let e = NmblError::OperatorAborted {
            context: "phase 3b: waiting for /dev/nvme0n1p2".to_string(),
        };
        let formatted = format_chain(&e as &dyn Error);
        assert!(
            formatted.contains("operator aborted"),
            "format_chain must lead with the variant prefix: {formatted}"
        );
        assert!(
            formatted.contains("/dev/nvme0n1p2"),
            "format_chain must surface the abort context: {formatted}"
        );
        assert!(
            !formatted.contains("caused by"),
            "OperatorAborted has no source — no caused-by line expected: {formatted}"
        );
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
