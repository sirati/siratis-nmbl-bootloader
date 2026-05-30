use crate::error::{NmblError, format_chain};
use crate::terminal::EmergencyBanner;

/// Print the full operator-facing banner: header, suggested action,
/// the error chain. Plain ASCII — the early-userspace console may
/// not have UTF-8 box-drawing glyphs. Called by the dispatcher in
/// `main` immediately before the execve syscall fires, so the
/// operator sees the chain printed onto the freshly-restored VT.
pub fn print_banner(banner: &EmergencyBanner) {
    let separator = "=".repeat(72);
    eprintln!("{separator}");
    eprintln!("NMBL: dropped to emergency shell");
    eprintln!("{separator}");
    eprintln!();
    eprintln!("Suggested action:");
    eprintln!("  {}", suggested_action(&banner.err));
    eprintln!();
    eprintln!("Error chain:");
    let chain = format_chain(&banner.err as &dyn std::error::Error);
    for line in chain.lines() {
        eprintln!("  {line}");
    }
    eprintln!();
    eprintln!("Shell: {}  (will execve next)", banner.shell_path.display());
    eprintln!("Type `exit` to reboot, or fix the issue and re-exec /init.");
    eprintln!("{separator}");
}

/// Print the halt-with-banner banner: same shape as
/// [`print_banner`] but tailored to the no-rescue-toolkit scenario.
/// Called by the dispatcher in `main` immediately before the
/// `reboot(RB_HALT_SYSTEM)` syscall fires.
pub fn print_halt_banner(cause: &NmblError) {
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
}

/// One-line operator hint per error variant. Exhaustive `match` so
/// adding a new variant to [`NmblError`] becomes a compile error here
/// rather than a silently missing diagnostic at boot.
pub(super) fn suggested_action(err: &NmblError) -> String {
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
        NmblError::SystemRootNotMounted { mountpoint } => format!(
            "Nothing is mounted at {mp}. Mount the NixOS system root there first \
             (e.g. `mount /dev/<root> {mp}`), then re-run [Verify kexec readiness].",
            mp = mountpoint.display()
        ),
        NmblError::ProfilesDirMissing { path, mountpoint } => format!(
            "WARNING: {p} does not exist even though something is mounted at {mp}. \
             This usually means the wrong filesystem (or the wrong directory) was \
             hand-mounted at {mp}. NMBL needs the NixOS system root mounted at {mp} — \
             the filesystem that contains nix/var/nix/profiles/system-*-link and the \
             nix store. Unmount {mp}, mount the correct system root there, then re-run \
             [Verify kexec readiness].",
            p = path.display(),
            mp = mountpoint.display()
        ),
        NmblError::Tui { .. } => "TUI failed — fall back to serial mode in config.".to_string(),
        NmblError::Activation { kind, .. } => {
            format!("Activation '{kind}' failed; check the relevant tool's stderr above.")
        }
        NmblError::Bootstrap { stage, .. } => format!(
            "Bootstrap stage '{stage}' failed; check bootstrap.toml and the boot partition."
        ),
        NmblError::Rescue { stage, .. } => {
            format!("Rescue stage '{stage}' failed; check the rescue squashfs/network state.")
        }
        NmblError::Panicked { report_path } => format!(
            "Recovered from a panic; report at {}.",
            report_path.display()
        ),
        NmblError::Shell { .. } => "Failed to exec the emergency shell itself. Reboot.".to_string(),
        NmblError::OperatorAborted { .. } => {
            "You aborted the wait. Pick a different option.".to_string()
        }
        NmblError::OperatorChoseReboot { .. } => {
            "You picked Reboot on the wrong-password modal.".to_string()
        }
        NmblError::WrongPasswordShellExited { context } => format!(
            "You dropped to a shell after a wrong passphrase ({context}). \
             Pick [Retry boot from config] to re-prompt for the passphrase."
        ),
        NmblError::StateTooLarge { encoded_len, max } => format!(
            "state.bin payload {encoded_len} bytes overflowed the {max} byte slot — installer bug."
        ),
        NmblError::StateRoundtripMismatch { path } => format!(
            "state.bin at {} did not round-trip through encode/decode; refusing to overwrite.",
            path.display()
        ),
        NmblError::DryRunShellPreflight => {
            "Dry-run shell preflight completed without forking — informational only.".to_string()
        }
    }
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
    fn suggested_action_for_system_root_not_mounted_says_nothing_mounted() {
        let e = NmblError::SystemRootNotMounted {
            mountpoint: PathBuf::from("/mnt/system"),
        };
        let s = suggested_action(&e);
        assert!(s.contains("Nothing is mounted"), "{s}");
        assert!(s.contains("/mnt/system"), "{s}");
        assert!(
            s.contains("Mount the NixOS system root"),
            "must tell the operator to mount the system root: {s}"
        );
    }

    #[test]
    fn suggested_action_for_profiles_dir_missing_warns_and_states_requirements() {
        let e = NmblError::ProfilesDirMissing {
            path: PathBuf::from("/mnt/system/nix/var/nix/profiles"),
            mountpoint: PathBuf::from("/mnt/system"),
        };
        let s = suggested_action(&e);
        // Warns the dir is missing.
        assert!(s.contains("WARNING"), "must warn: {s}");
        assert!(s.contains("/mnt/system/nix/var/nix/profiles"), "{s}");
        // Hints the likely cause: a wrong hand-mount.
        assert!(
            s.contains("wrong filesystem") || s.contains("wrong directory"),
            "must blame a bad hand-mount: {s}"
        );
        // States the layout requirements NMBL expects.
        assert!(
            s.contains("nix/var/nix/profiles/system-*-link"),
            "must state the required profiles/store layout: {s}"
        );
        assert!(s.contains("nix store"), "must mention the store: {s}");
        assert!(s.contains("/mnt/system"), "{s}");
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
    fn suggested_action_for_operator_aborted_hints_pick_other_option() {
        let e = NmblError::OperatorAborted {
            context: "waiting for /dev/sda1".to_string(),
        };
        let s = suggested_action(&e);
        assert!(
            s.contains("aborted"),
            "operator hint should mention the abort: {s}"
        );
        assert!(
            s.contains("Pick a different option") || s.contains("different option"),
            "operator hint should suggest picking another action: {s}"
        );
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
