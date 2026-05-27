//! Rescue dispatcher (PLAN.md §6.3, Option 2 + Phase C.1).
//!
//! The emergency-shell entrypoint in `shell::drop_to_emergency` decides
//! between three rescue modes based on the operator's
//! [`RescueConfig::mode`]:
//!
//! * [`RescueMode::Embedded`] — legacy: busybox lives inside the
//!   initramfs cpio. [`exec_embedded`] performs the bare `execve(2)`
//!   into `cfg.paths.shell`, mirroring what `shell::drop_to_emergency`
//!   has always done.
//! * [`RescueMode::External`] — `nmbl-rescue.sfs` lives on the boot
//!   partition; we loop-mount it on demand, `pivot_root` into it, and
//!   exec its `/bin/sh`. See [`disk::try_disk_rescue`].
//! * [`RescueMode::None`] — no rescue toolkit shipped. Print a
//!   structured banner and halt the system via `reboot(RB_HALT_SYSTEM)`.
//!
//! The public surface is the contract that C.3 (shell.rs refactor) and
//! Phase E.1 (`net::try_network_rescue`) build against, so the shapes
//! must not change without orchestrator coordination.

pub mod disk;
// Phase E adds `pub mod net;` here once the network primitives land
// behind the `network-rescue` Cargo feature.

use std::convert::Infallible;
use std::ffi::CString;
use std::path::PathBuf;

use nix::sys::reboot::{RebootMode, reboot};
use nix::unistd::execve;
use serde::Deserialize;

use crate::config::Config;
use crate::error::{NmblError, Result, format_chain};

/// How [`crate::shell::drop_to_emergency`] reaches the operator. Comes
/// from the runtime [`Config`]'s `[rescue]` section; persists to TOML
/// as kebab-case strings (`"embedded"`, `"external"`, `"none"`).
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RescueMode {
    /// Legacy: busybox baked into the initramfs.
    /// [`crate::shell::drop_to_emergency`] execs `cfg.paths.shell`
    /// directly via [`exec_embedded`].
    #[default]
    Embedded,
    /// `nmbl-rescue.sfs` on the boot partition; loop-mounted on demand
    /// by [`disk::try_disk_rescue`].
    External,
    /// No rescue tools shipped; halt with a structured banner via
    /// [`halt_with_banner`].
    None,
}

/// Decide whether to drop into the embedded shell, mount the external
/// squashfs, or halt. `cause` is the error that triggered the rescue
/// (surfaced in the banner). On success this function does not return —
/// it execs the rescue shell or halts the system.
pub fn dispatch(config: &Config, cause: &NmblError) -> Result<Infallible> {
    match config.rescue.mode {
        RescueMode::Embedded => exec_embedded(config),
        RescueMode::External => disk::try_disk_rescue(config, cause),
        RescueMode::None => halt_with_banner(cause),
    }
}

/// `execve(2)` the operator-configured shell (`cfg.paths.shell`) with
/// an empty environment. Mirrors the existing
/// [`crate::shell::drop_to_emergency`] body byte-for-byte so the
/// embedded rescue path retains its long-tested behaviour while
/// [`dispatch`] becomes the single decision point.
///
/// Returns `Result<Infallible>` so callers can chain — on success the
/// function does not return, on failure the returned `Err` carries the
/// underlying [`NmblError::Shell`] for the banner.
pub fn exec_embedded(config: &Config) -> Result<Infallible> {
    let shell_path = config.paths.shell.as_path();
    let argv0_bytes: Vec<u8> = shell_path
        .file_name()
        .map(|n| n.as_encoded_bytes().to_vec())
        .unwrap_or_else(|| shell_path.as_os_str().as_encoded_bytes().to_vec());

    // Interior NUL in a config-supplied path is astronomically unlikely
    // but still has to be handled. Surface as Rescue{stage:"shell-path-nul"}
    // so the banner makes the failure mode obvious.
    let path_c =
        CString::new(shell_path.as_os_str().as_encoded_bytes()).map_err(|_| NmblError::Rescue {
            stage: "shell-path-nul",
            source: Box::new(NmblError::ConfigInvalid {
                reason: "shell path contains interior NUL".to_string(),
                context: format!("preparing execve of {}", shell_path.display()),
            }),
        })?;
    let argv0_c = CString::new(argv0_bytes).map_err(|_| NmblError::Rescue {
        stage: "shell-argv0-nul",
        source: Box::new(NmblError::ConfigInvalid {
            reason: "shell argv0 contains interior NUL".to_string(),
            context: format!("preparing execve of {}", shell_path.display()),
        }),
    })?;

    let argv: [&CString; 1] = [&argv0_c];
    let env: [&CString; 0] = [];

    // execve only returns on error.
    let exec_err = execve(&path_c, &argv, &env).err();
    if let Some(source) = exec_err {
        return Err(NmblError::Rescue {
            stage: "exec-shell",
            source: Box::new(NmblError::Shell { source }),
        });
    }
    // execve returned Ok somehow — the kernel docs say this cannot
    // happen, but the type system forces us to produce something.
    // Treat it as an exec failure.
    Err(NmblError::Rescue {
        stage: "exec-shell",
        source: Box::new(NmblError::ConfigInvalid {
            reason: "execve returned Ok without replacing the process image".to_string(),
            context: format!("execve {}", shell_path.display()),
        }),
    })
}

/// Print a one-screen banner naming the failure cause, then halt the
/// system via `reboot(RB_HALT_SYSTEM)`. Used for [`RescueMode::None`]
/// installs where no toolkit ships and the kindest UX is to stop —
/// rather than leave the operator at an inert PID 1.
///
/// Returns `Result<Infallible>` for signature symmetry with the other
/// dispatch arms; in practice the function diverges (either the kernel
/// halts or `libc::_exit` terminates).
pub fn halt_with_banner(cause: &NmblError) -> Result<Infallible> {
    let separator = "=".repeat(72);
    eprintln!("{separator}");
    eprintln!("NMBL: no rescue toolkit available — halting");
    eprintln!("{separator}");
    eprintln!();
    eprintln!("Configured rescue mode is `none`. The initramfs ships no");
    eprintln!("interactive shell, and the operator did not enable the");
    eprintln!("external squashfs rescue. The system will halt.");
    eprintln!();
    eprintln!("Error chain:");
    let chain = format_chain(cause as &dyn std::error::Error);
    for line in chain.lines() {
        eprintln!("  {line}");
    }
    eprintln!("{separator}");

    // reboot() does not return on success. If the kernel refuses
    // (CAP_SYS_BOOT missing in a test sandbox, not PID 1, …) fall
    // through to libc::_exit, which is itself `!`.
    let _ = reboot(RebootMode::RB_HALT_SYSTEM);
    // SAFETY: libc::_exit is async-signal-safe and unconditionally
    // terminates the process; no crate wraps it (rustix issue #844).
    // Matches the divergent halt path in `src/shell.rs`.
    unsafe { libc::_exit(1) };
}

/// Decide where to look for the external rescue squashfs. If the
/// operator pinned an absolute path via `rescue.sfs_path`, use it;
/// otherwise fall back to `/boot/nmbl-rescue.sfs`. The bootstrap
/// flow's mountpoint is not visible in [`Config`] — operators running
/// in bootstrap mode pass an explicit `sfs_path` pointing at
/// `<boot_fs.mountpoint>/nmbl-rescue.sfs` via their Nix-emitted
/// runtime config (Phase C.2).
pub fn locate_sfs(config: &Config) -> PathBuf {
    config
        .rescue
        .sfs_path
        .clone()
        .unwrap_or_else(|| PathBuf::from("/boot/nmbl-rescue.sfs"))
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;
    use crate::config::RescueConfig;
    use std::path::Path;

    #[test]
    fn rescue_mode_default_is_embedded() {
        assert_eq!(RescueMode::default(), RescueMode::Embedded);
    }

    #[test]
    fn rescue_mode_parses_kebab_case() {
        #[derive(Debug, Deserialize)]
        struct W {
            mode: RescueMode,
        }
        let parse = |s: &str| -> RescueMode {
            let w: W = toml::from_str(s).expect("parses");
            w.mode
        };
        assert_eq!(parse(r#"mode = "embedded""#), RescueMode::Embedded);
        assert_eq!(parse(r#"mode = "external""#), RescueMode::External);
        assert_eq!(parse(r#"mode = "none""#), RescueMode::None);
    }

    #[test]
    fn rescue_mode_rejects_unknown_string() {
        #[derive(Debug, Deserialize)]
        struct W {
            #[allow(dead_code)]
            mode: RescueMode,
        }
        toml::from_str::<W>(r#"mode = "bogus""#).expect_err("unknown mode must reject");
    }

    fn cfg_with(rescue: RescueConfig) -> Config {
        let mut c = Config::recovery_default();
        c.rescue = rescue;
        c
    }

    #[test]
    fn locate_sfs_defaults_to_boot_path() {
        let c = cfg_with(RescueConfig::default());
        assert_eq!(locate_sfs(&c), Path::new("/boot/nmbl-rescue.sfs"));
    }

    #[test]
    fn locate_sfs_honours_override() {
        let c = cfg_with(RescueConfig {
            mode: RescueMode::External,
            sfs_path: Some(PathBuf::from("/mnt/boot/nmbl-rescue.sfs")),
        });
        assert_eq!(locate_sfs(&c), Path::new("/mnt/boot/nmbl-rescue.sfs"));
    }
}
