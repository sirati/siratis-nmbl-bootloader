//! The signer's error type — a flat, message-carrying enum.
//!
//! `nmbl-sign` is a host CLI, so its errors are operator-facing strings rather
//! than the structured `NmblError` the in-initramfs verifier threads. Every
//! fallible step returns [`SignError`]; `main` prints it and exits non-zero.

use std::fmt;

/// A signer failure. Each variant carries enough context for the operator to
/// fix the invocation without reading source.
#[derive(Debug)]
pub enum SignError {
    /// A command-line argument was missing, malformed, or unknown.
    Usage(String),
    /// An I/O step (read input, write key/sidecar) failed.
    Io {
        context: String,
        source: std::io::Error,
    },
    /// A `fips204` keygen/sign call returned an error string.
    Crypto { context: String, reason: String },
    /// Key material on disk had the wrong length or failed to decode.
    Key(String),
}

impl SignError {
    /// Build an [`SignError::Io`] from an I/O error plus a human context.
    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }

    /// Build an [`SignError::Crypto`] from a context and a `fips204` reason.
    pub fn crypto(context: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::Crypto {
            context: context.into(),
            reason: reason.into(),
        }
    }
}

impl fmt::Display for SignError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(msg) => write!(f, "usage error: {msg}"),
            Self::Io { context, source } => write!(f, "{context}: {source}"),
            Self::Crypto { context, reason } => write!(f, "{context}: {reason}"),
            Self::Key(msg) => write!(f, "key error: {msg}"),
        }
    }
}

impl std::error::Error for SignError {}

/// Convenience alias for the signer's fallible results.
pub type Result<T> = std::result::Result<T, SignError>;
