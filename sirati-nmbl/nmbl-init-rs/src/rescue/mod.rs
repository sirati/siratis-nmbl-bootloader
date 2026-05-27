//! Rescue-dispatch entrypoint (Phase C, Option 2).
//!
//! STUB MODULE. The canonical implementation is being landed by a
//! sibling task (C.1) that will overwrite this file at merge time. The
//! stub exists purely so this branch compiles in isolation; it
//! preserves the public surface C.1's contract advertises and routes
//! the default `Embedded` mode through the same execve flow that used
//! to live in `shell.rs::drop_to_emergency` so the bin keeps booting
//! pre-merge.
//!
//! Contract (mirrors `/home/sirati/.claude/plans/vivid-sprouting-turtle.md`
//! §"Option 2 — External rescue squashfs"):
//!
//! ```ignore
//! pub enum RescueMode { Embedded, External, None }       // default Embedded
//! pub fn dispatch(config: &Config, cause: &NmblError) -> Result<Infallible>;
//! pub fn exec_embedded(config: &Config) -> Result<Infallible>;
//! ```

use std::convert::Infallible;
use std::ffi::CString;

use nix::unistd::execve;

use crate::config::Config;
use crate::error::{NmblError, Result};

/// Which rescue strategy to attempt when the boot phases short-circuit
/// into the emergency path. Real semantics land with C.1; the stub
/// only implements `Embedded`.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RescueMode {
    /// Busybox shipped inside the initramfs (legacy v1 behaviour).
    #[default]
    Embedded,
    /// `nmbl-rescue.sfs` loop-mounted from the boot partition. Real
    /// implementation in C.1; this stub falls back to an error so the
    /// caller can halt.
    External,
    /// No rescue tools shipped. PID 1 halts on the emergency path.
    None,
}

/// Decide which rescue path to take and run it. Always either does
/// not return (success path execs a shell) or produces an error the
/// caller routes to halt. Real branching for `External`/`None` lands
/// with C.1.
pub fn dispatch(config: &Config, _cause: &NmblError) -> Result<Infallible> {
    match config.rescue.mode {
        RescueMode::Embedded => exec_embedded(config),
        RescueMode::External => Err(NmblError::Rescue {
            stage: "stub-external",
            source: Box::new(NmblError::ConfigInvalid {
                reason: "external rescue not implemented in C.3 stub; C.1 owns this path"
                    .to_string(),
                context: "rescue::dispatch".to_string(),
            }),
        }),
        RescueMode::None => Err(NmblError::Rescue {
            stage: "mode-none",
            source: Box::new(NmblError::ConfigInvalid {
                reason: "rescue.mode = none — operator opted out of rescue tools".to_string(),
                context: "rescue::dispatch".to_string(),
            }),
        }),
    }
}

/// `execve(2)` the configured shell. Does not return on success; on
/// failure returns `Err(NmblError::Shell { .. })` so the caller can
/// log + halt without this function deciding policy.
pub fn exec_embedded(config: &Config) -> Result<Infallible> {
    let shell_path = config.paths.shell.as_path();
    let argv0_bytes: Vec<u8> = shell_path
        .file_name()
        .map(|n| n.as_encoded_bytes().to_vec())
        .unwrap_or_else(|| shell_path.as_os_str().as_encoded_bytes().to_vec());

    // Any interior NUL here means the operator put a NUL in the config
    // path — astronomically unlikely but still has to be handled. We
    // surface EINVAL through the standard Shell variant so the caller's
    // halt path renders a useful diagnostic.
    let path_c =
        CString::new(shell_path.as_os_str().as_encoded_bytes()).map_err(|_| NmblError::Shell {
            source: nix::Error::from(nix::errno::Errno::EINVAL),
        })?;
    let argv0_c = CString::new(argv0_bytes).map_err(|_| NmblError::Shell {
        source: nix::Error::from(nix::errno::Errno::EINVAL),
    })?;

    let argv: [&CString; 1] = [&argv0_c];
    let env: [&CString; 0] = [];

    // nix::execve returns Result<Infallible, Errno>: the Ok branch is
    // statically uninhabited (success replaces the process image), so
    // matching against it is the canonical no-op consumer.
    match execve(&path_c, &argv, &env) {
        Ok(infallible) => match infallible {},
        Err(source) => Err(NmblError::Shell { source }),
    }
}
