//! Emergency-shell exec site (PLAN.md §6.3 / §9).
//!
//! This module is one of the few crate sites permitted to replace the
//! current process via `execve(2)` (see also `src/panic.rs` and
//! `src/sys/activation.rs`). When any top-level phase returns `Err`,
//! `main` routes the error through [`drop_to_emergency`], which:
//!
//! 1. Runs the [`crate::ui::run_emergency_screen`] TUI over the splash
//!    backend (or a tty/serial fallback) to ask the operator whether
//!    they want to reboot or get a shell.
//! 2. On [`EmergencyChoice::Reboot`] calls `reboot(RB_AUTOBOOT)`.
//! 3. On [`EmergencyChoice::Shell`] prints the operator-facing banner
//!    including the full error chain and then `execve(2)`s the
//!    configured shell binary.
//!
//! On success the function does not return — either `reboot` reboots
//! the machine or the new shell process inherits PID 1. The signature
//! uses [`std::convert::Infallible`] to document that contract at the
//! type level: a caller that does `let _: Infallible = drop_to_emergency(...);`
//! will never proceed.
//!
//! Failure modes are themselves "no-return": if the `execve(2)` fails
//! (shell binary missing, ENOEXEC, EACCES, …) we print one last
//! diagnostic and halt the system via `reboot(RB_HALT_SYSTEM)` — a
//! kernel panic is preferable to silently returning to a caller that
//! expects `Infallible`.

use std::convert::Infallible;
use std::ffi::CString;

use nix::sys::reboot::{RebootMode, reboot};
use nix::unistd::execve;

use crate::config::Config;
use crate::error::{NmblError, format_chain};
use crate::ui::{EmergencyChoice, run_emergency_screen};

/// Print the operator-facing emergency banner and `execve(2)` the
/// configured shell. Does not return on success; on `execve` failure
/// halts the system rather than returning to the caller.
pub fn drop_to_emergency(config: &Config, err: NmblError) -> Infallible {
    // Ask the operator what to do via the TUI. The TUI runs over the
    // splash backend when available and falls back to a tty / serial
    // prompt otherwise; on a 30s no-input timeout it returns Reboot.
    let choice = run_emergency_screen(config, &err);

    match choice {
        EmergencyChoice::Reboot => {
            eprintln!("[nmbl] operator (or timeout) chose reboot");
            let _ = reboot(RebootMode::RB_AUTOBOOT);
            // reboot() returned Err — fall through to the same halt
            // path execve uses, so we still preserve Infallible.
            halt_with("reboot(RB_AUTOBOOT) returned; halting");
        }
        EmergencyChoice::Shell => exec_shell(config, &err),
    }
}

/// Execute the chosen-shell path: print the banner with the error
/// chain so the operator has context, then `execve`. Does not return
/// on success; on `execve` failure halts the system.
fn exec_shell(config: &Config, err: &NmblError) -> Infallible {
    print_banner(config, err);

    let shell_path = config.paths.shell.as_path();
    let argv0_bytes: Vec<u8> = shell_path
        .file_name()
        .map(|n| n.as_encoded_bytes().to_vec())
        .unwrap_or_else(|| shell_path.as_os_str().as_encoded_bytes().to_vec());

    // Any interior NUL here means the operator put a NUL in the config
    // path — astronomically unlikely but still has to be handled. We
    // synthesize a Shell error, log it, and halt.
    let path_c = match CString::new(shell_path.as_os_str().as_encoded_bytes()) {
        Ok(c) => c,
        Err(_) => halt_with("shell path contains interior NUL"),
    };
    let argv0_c = match CString::new(argv0_bytes) {
        Ok(c) => c,
        Err(_) => halt_with("shell argv0 contains interior NUL"),
    };

    let argv: [&CString; 1] = [&argv0_c];
    let env: [&CString; 0] = [];

    // execve only returns on error.
    let exec_err = execve(&path_c, &argv, &env).err();

    // Surface the underlying errno (if any) before halting so the
    // operator sees why their shell didn't come up.
    if let Some(source) = exec_err {
        let synthetic = NmblError::Shell { source };
        eprintln!(
            "[nmbl] EMERGENCY SHELL EXEC FAILED: {}",
            format_chain(&synthetic as &dyn std::error::Error)
        );
    }
    halt_with("emergency shell exec failed; halting")
}

/// Print the full operator-facing banner: header, suggested action,
/// the error chain. Plain ASCII — the early-userspace console may not
/// have UTF-8 box-drawing glyphs.
fn print_banner(config: &Config, err: &NmblError) {
    let separator = "=".repeat(72);
    eprintln!("{separator}");
    eprintln!("NMBL: dropped to emergency shell");
    eprintln!("{separator}");
    eprintln!();
    eprintln!("Suggested action:");
    eprintln!("  {}", suggested_action(err));
    eprintln!();
    eprintln!("Error chain:");
    let chain = format_chain(err as &dyn std::error::Error);
    for line in chain.lines() {
        eprintln!("  {line}");
    }
    eprintln!();
    eprintln!(
        "Shell: {}  (will execve next)",
        config.paths.shell.display()
    );
    eprintln!("Type `exit` to reboot, or fix the issue and re-exec /init.");
    eprintln!("{separator}");
}

/// One-line operator hint per error variant. Exhaustive `match` so
/// adding a new variant to [`NmblError`] becomes a compile error here
/// rather than a silently missing diagnostic at boot.
fn suggested_action(err: &NmblError) -> String {
    match err {
        NmblError::Config { .. } => "Check /etc/nmbl/config.toml syntax.".to_string(),
        NmblError::Io { context, .. } => format!("Filesystem op failed: {context}."),
        NmblError::ConfigInvalid { reason, context } => {
            format!("Config invalid: {context}: {reason}.")
        }
        NmblError::Mount {
            src, dst, fstype, ..
        } => {
            let src_display = match src {
                Some(p) => p.display().to_string(),
                None => "<none>".to_string(),
            };
            format!(
                "Try: mount -t {fstype} {src_display} {dst}.",
                dst = dst.display()
            )
        }
        NmblError::Umount { dst, .. } => format!("Try: umount -l {}.", dst.display()),
        NmblError::Module { name, path, .. } => {
            format!("Try: insmod {} for {name}.", path.display())
        }
        NmblError::KexecLoad { .. } => {
            "Check the chosen generation's kernel/initrd paths are valid files.".to_string()
        }
        NmblError::KexecReturned { .. } => {
            "kexec actually executed but returned — kernel rejected the image.".to_string()
        }
        NmblError::DeviceTimeout { device, timeout_ms } => format!(
            "Device {} didn't appear in {timeout_ms}ms. Check /dev and activation logs.",
            device.display()
        ),
        NmblError::NoGenerations { searched } => format!(
            "No system-N-link entries found under {}. Verify the system filesystem is mounted.",
            searched.display()
        ),
        NmblError::Tui { .. } => "TUI failed — fall back to serial mode in config.".to_string(),
        NmblError::Activation { kind, .. } => {
            format!("Activation '{kind}' failed; check the relevant tool's stderr above.")
        }
        NmblError::Panicked { report_path } => format!(
            "Recovered from a panic; report at {}.",
            report_path.display()
        ),
        NmblError::Shell { .. } => "Failed to exec the emergency shell itself. Reboot.".to_string(),
    }
}

/// Print a one-line final-fallback message and halt. Returns
/// `Infallible` because [`reboot`] with `RB_HALT_SYSTEM` does not
/// return on success and we route the error case into a second halt
/// attempt (then `_exit(1)`).
fn halt_with(reason: &str) -> ! {
    eprintln!("[nmbl] {reason}");
    let _ = reboot(RebootMode::RB_HALT_SYSTEM);
    // reboot() returned Err — kernel refused (lacking CAP_SYS_BOOT in
    // a test/sandbox, or we are not PID 1). Best we can do is _exit.
    // SAFETY: libc::_exit is async-signal-safe and unconditionally
    // terminates the process; no crate wraps it (rustix issue #844).
    unsafe { libc::_exit(1) };
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn io_err(ctx: &str) -> NmblError {
        NmblError::Io {
            source: std::io::Error::other("test"),
            context: ctx.to_string(),
        }
    }

    #[test]
    fn suggested_action_for_io_mentions_context() {
        let s = suggested_action(&io_err("mounting /tmp"));
        assert!(s.contains("mounting /tmp"), "{s}");
    }

    #[test]
    fn suggested_action_for_device_timeout_includes_device_and_time() {
        let e = NmblError::DeviceTimeout {
            device: PathBuf::from("/dev/nvme0n1p2"),
            timeout_ms: 15_000,
        };
        let s = suggested_action(&e);
        assert!(s.contains("/dev/nvme0n1p2"), "{s}");
        assert!(s.contains("15000ms"), "{s}");
    }

    #[test]
    fn suggested_action_for_mount_renders_command_hint() {
        let e = NmblError::Mount {
            src: Some(PathBuf::from("/dev/sda1")),
            dst: PathBuf::from("/mnt/system"),
            fstype: "ext4".to_string(),
            source: nix::Error::from(nix::errno::Errno::EINVAL),
        };
        let s = suggested_action(&e);
        assert!(s.contains("mount -t ext4 /dev/sda1 /mnt/system"), "{s}");
    }

    #[test]
    fn suggested_action_for_no_generations_includes_path() {
        let e = NmblError::NoGenerations {
            searched: PathBuf::from("/mnt/system/nix/var/nix/profiles"),
        };
        let s = suggested_action(&e);
        assert!(s.contains("/mnt/system/nix/var/nix/profiles"), "{s}");
    }

    #[test]
    fn suggested_action_for_panicked_includes_report_path() {
        let e = NmblError::Panicked {
            report_path: PathBuf::from("/run/nmbl-panic-1.txt"),
        };
        let s = suggested_action(&e);
        assert!(s.contains("/run/nmbl-panic-1.txt"), "{s}");
    }

    #[test]
    fn suggested_action_for_activation_includes_kind() {
        let e = NmblError::Activation {
            kind: "luks-password".to_string(),
            source: Box::new(io_err("inner")),
        };
        let s = suggested_action(&e);
        assert!(s.contains("luks-password"), "{s}");
    }
}
