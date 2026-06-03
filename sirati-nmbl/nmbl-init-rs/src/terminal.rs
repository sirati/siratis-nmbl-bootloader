//! No-return boot terminus.
//!
//! Inner layers that used to call `execve(2)`, `reboot(2)`, or
//! `kexec`'s `reboot(RB_KEXEC)` inline now return a [`TerminalAction`]
//! value. `main` is the single point that performs the syscall — by
//! then the call-stack has unwound and every `Drop` (Console,
//! RawModeGuard, glyph caches, …) has already run. Without that
//! ordering `execve` replaces the process image without unwinding the
//! stack, `Drop` never runs, KD_GRAPHICS stays set, and the
//! freshly-execve'd shell renders invisibly under a frozen splash
//! frame.
//!
//! The old emergency-shell fix (commit 6ddb6e6 / fc84e7c) made
//! `drop_to_emergency` take `Option<Box<dyn Console>>` and explicitly
//! `drop(console)` immediately before each `execve`. That worked but
//! required every author of every terminal-action call site to
//! remember the manual drop. This module replaces that pattern with a
//! type-driven one: build a [`TerminalAction`] in the inner layer,
//! return it, and let normal scope exit run all `Drop` impls before
//! `main::execute_terminal_action` performs the no-return syscall.

use std::ffi::CString;
use std::io;
use std::path::PathBuf;

use nix::unistd::dup2;

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::policy::Sealed;

/// Path the dispatcher opens to dup `0/1/2` onto before `execve`. We
/// target `/dev/console` (rather than a hardcoded `/dev/tty1`) so the
/// kernel's `console=` cmdline ordering decides where the rescue shell
/// renders: VGA/framebuffer when the operator booted on a head, serial
/// when they booted on a headless ttyS0 box. Hardcoding `tty1` would
/// silently break every serial-console workflow.
///
/// Operators who want the rescue shell mirrored onto extra devices
/// will opt in via a future `boot.nmbl.emergencyShell.extraConsoles`
/// option; until that option lands we honour exactly one console (the
/// kernel-chosen primary) so user-space writes are not multiplexed
/// behind a console driver the operator did not ask for.
const EXECVE_STDIO_PATH: &str = "/dev/console";

/// A no-return action to perform after every stack-allocated boot
/// resource has been dropped.
///
/// Returned by inner layers (`drop_to_emergency`, `rescue::dispatch`,
/// `kexec_into`, …) so `main` is the single point that performs the
/// terminal syscall (`execve`, `reboot`, kexec). By the time the
/// value reaches the dispatcher, [`crate::ui::console::Console`],
/// `RawModeGuard`, glyph caches, and every other stack-allocated
/// boot resource have already been dropped via normal unwinding.
///
/// Halt and emergency-shell variants carry the formatted operator
/// banner they want printed; the dispatcher prints it inside the same
/// arm that performs the syscall, so the operator sees a stable
/// "banner → action" flow regardless of which inner layer produced
/// the value.
#[must_use = "TerminalAction must be executed at the top of main; \
              ignoring it leaks Drop side effects (KD_TEXT restore, termios, fds)"]
#[derive(Debug)]
pub enum TerminalAction {
    /// `reboot(RB_AUTOBOOT)`. Operator (or the 30s emergency-screen
    /// timeout) chose reboot, or some other inner layer requested
    /// the same fallback.
    Reboot,

    /// `reboot(RB_HALT_SYSTEM)` after printing the no-rescue-toolkit
    /// banner. Used by [`crate::rescue::halt_with_banner`] when
    /// `rescue.mode = "none"` or every external rescue path failed.
    HaltWithBanner {
        /// The original failure that triggered the rescue attempt.
        /// Printed verbatim in the banner so the operator sees the
        /// full chain.
        cause: NmblError,
    },

    /// `execve(path, argv, env)`. `argv[0]` is included in `argv`.
    Execve {
        /// Absolute path to the binary the kernel resolves and loads.
        path: CString,
        /// Full argv vector, including the customary `argv[0]`.
        argv: Vec<CString>,
        /// Process environment for the new image. Often empty
        /// (embedded busybox path); the rescue squashfs path passes
        /// `TERM=linux` and a minimal `PATH`.
        env: Vec<CString>,
        /// Pre-execve banner. The emergency-shell path supplies one
        /// so the operator sees the error chain and the suggested
        /// action; rescue paths that have their own UI pass `None`.
        banner: Option<EmergencyBanner>,
        /// Whether this execve is a rescue handoff. When `true`, the
        /// dispatcher treats a failed `/dev/console` stdio redirect as
        /// non-fatal: it logs a warning and execve's anyway with the
        /// inherited fds, because the rescue entrypoint (full-system
        /// `/init` or busybox `sh`) manages its own console and halting
        /// here would strand the operator. When `false` (reserved for a
        /// future non-rescue execve, e.g. a direct re-exec), a redirect
        /// failure stays fatal.
        rescue_handoff: bool,
    },

    /// Fire the previously-loaded kexec image via
    /// `reboot(LINUX_REBOOT_CMD_KEXEC)`. The image was already loaded
    /// (and the filesystems were detached) inside `boot::kexec_into`
    /// before the value reached this variant — only the cutover
    /// syscall is deferred to the dispatcher.
    Kexec,

    /// `reboot(RB_AUTOBOOT)` — the SOLE untrusted-image / policy refuse
    /// terminus (R-1). Reached after [`crate::policy::relock_and_refuse`]
    /// has capped the lock PCR, closed every TPM-unsealed mapper,
    /// relocked LUKS, and written the rescue sentinel; the non-interactive
    /// refuse countdown then ran (Enter / timeout) before unwinding here.
    /// The dispatcher reboots straight back into firmware, which — with
    /// the sentinel now present — boots the rescue path with the TPM still
    /// locked.
    ///
    /// **Constructible ONLY via [`TerminalAction::reboot_into_rescue`]**,
    /// which requires a [`Sealed`] witness by value. The variant carries
    /// that witness as a field, so a `RebootIntoRescue { … }` literal can
    /// only be written by code that already holds a real [`Sealed`] — and
    /// `Sealed`'s sole constructor lives behind [`crate::policy::seal_secrets`]
    /// (cap PCR + close every TPM-unsealed mapper). By type this variant
    /// therefore cannot be produced without having sealed first (R-2 /
    /// FIX-29). [`HaltWithBanner`] is SUPERSEDED by this variant (R-1); do
    /// not construct it on any new refuse path.
    ///
    /// [`HaltWithBanner`]: TerminalAction::HaltWithBanner
    RebootIntoRescue {
        /// The original failure that triggered the refuse. Printed in the
        /// refuse banner / logs so the operator sees the full chain.
        cause: NmblError,
        /// Unforgeable proof the lock PCR was capped and every
        /// TPM-unsealed mapper closed before this terminus was built. The
        /// field exists purely to make the seal a TYPE precondition of the
        /// variant: you cannot name a `Sealed` value without minting one
        /// through [`crate::policy::seal_secrets`].
        sealed: Sealed,
    },
}

impl TerminalAction {
    /// Build the [`TerminalAction::RebootIntoRescue`] terminus. Requires a
    /// [`Sealed`] witness BY VALUE: a `RebootIntoRescue` is reachable only
    /// after [`crate::policy::seal_secrets`] (cap PCR + close every
    /// TPM-unsealed mapper) has minted the proof, so the refuse terminus
    /// is type-gated on a successful seal (R-2 / FIX-29).
    ///
    /// This is the ergonomic constructor; the variant also stores the
    /// witness so even a hand-written literal needs a real `Sealed`.
    /// Callers reach it through [`crate::policy::relock_and_refuse`] /
    /// [`crate::policy::refuse_unsigned`].
    ///
    /// The type-gate is compile-level: there is no way to obtain a
    /// [`Sealed`] outside the `policy` module's seal functions, so building
    /// the refuse terminus by hand does not compile —
    ///
    /// ```compile_fail
    /// use nmbl_init::error::NmblError;
    /// use nmbl_init::terminal::TerminalAction;
    /// // No public `Sealed` constructor exists, so neither the literal nor
    /// // the constructor can be reached without a real seal:
    /// let cause = NmblError::Signature { stage: "x", detail: String::new() };
    /// let _ = TerminalAction::RebootIntoRescue { cause }; // missing `sealed`, and `Sealed` is unconstructible
    /// ```
    pub fn reboot_into_rescue(sealed: Sealed, cause: NmblError) -> Self {
        TerminalAction::RebootIntoRescue { cause, sealed }
    }
}

/// Operator-facing emergency banner. Carried by
/// [`TerminalAction::Execve`] when the emergency-shell path is taken,
/// so the dispatcher can print it immediately before the `execve`.
///
/// We keep the originating `NmblError` and the shell path here
/// instead of pre-formatting the banner text. This lets the
/// dispatcher decide line wrapping / formatting once, and keeps the
/// `NmblError` available for any logging the dispatcher wants to do
/// outside the banner itself.
#[derive(Debug)]
pub struct EmergencyBanner {
    /// Path the dispatcher is about to `execve`. Surfaced verbatim
    /// in the banner so the operator can confirm which shell binary
    /// will inherit PID 1.
    pub shell_path: PathBuf,
    /// The failure that landed us in the emergency screen. Printed
    /// as a chained "caused by:" cascade so the operator sees every
    /// layer.
    pub err: NmblError,
}

impl EmergencyBanner {
    /// Build a banner against the configured shell path and the
    /// failure chain. The constructor lives here (rather than in
    /// `shell.rs`) so every terminal-action producer can build one
    /// without duplicating the field-by-field initialiser.
    pub fn new(config: &Config, err: NmblError) -> Self {
        Self {
            shell_path: config.paths.shell.clone(),
            err,
        }
    }
}

/// Open [`EXECVE_STDIO_PATH`] and redirect fds 0/1/2 onto it via
/// `dup2(2)`, so the upcoming `execve(2)` lands a process whose
/// stdin/stdout/stderr reach the operator's primary console.
///
/// Why this matters: every stack-allocated `Console` / `RawModeGuard`
/// has already dropped by the time the dispatcher in `main` runs, so
/// the freshly-execve'd shell would otherwise inherit whatever fds
/// 0/1/2 happened to point at when nmbl-init was first execve'd by
/// the kernel — typically the kernel-side `/dev/console` but with
/// unspecified termios state. Re-opening `/dev/console` here gives
/// the shell a clean read/write handle with default termios and
/// guarantees the prompt lands on the device the kernel cmdline
/// elected as the primary console (the last `console=` token; see
/// `Documentation/admin-guide/serial-console.rst`).
///
/// We deliberately target `/dev/console` rather than a hardcoded
/// `/dev/tty1` so serial-console boots (`console=ttyS0,115200`) keep
/// working — hardcoding `tty1` would dump the rescue shell on a VT
/// the operator never sees. Operators who want the prompt mirrored
/// onto extra devices will opt in via a future
/// `boot.nmbl.emergencyShell.extraConsoles` option (tracked
/// separately; not in scope here).
///
/// Errors carry `Rescue { stage: "shell-tty-…" }` so the emergency
/// banner names the failed step. The dispatcher in `main` surfaces
/// the error to the caller instead of falling through to a deaf
/// `execve` whose output the operator could not see.
pub fn redirect_stdio_for_execve() -> Result<()> {
    // Read+Write so the same fd serves stdin (read) and stdout/stderr
    // (write). The default OpenOptions do not set O_NOCTTY; we rely
    // on the shell to call setsid()/TIOCSCTTY itself if it wants the
    // controlling tty (busybox `sh -l` does the right thing here).
    let tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(EXECVE_STDIO_PATH)
        .map_err(|source| NmblError::Rescue {
            stage: "shell-tty-open",
            source: Box::new(NmblError::Io {
                source,
                context: format!("opening {EXECVE_STDIO_PATH} for execve stdio"),
            }),
        })?;

    use std::os::unix::io::AsRawFd;
    let fd = tty.as_raw_fd();
    for target in [0, 1, 2] {
        dup2(fd, target).map_err(|source| NmblError::Rescue {
            stage: "shell-tty-dup2",
            source: Box::new(NmblError::Io {
                source: io::Error::from_raw_os_error(source as i32),
                context: format!("dup2({EXECVE_STDIO_PATH}, fd={target})"),
            }),
        })?;
    }
    // `tty` is dropped here: its original fd is closed but fds 0/1/2
    // remain valid dup'd references to the same open file description.
    drop(tty);
    Ok(())
}
