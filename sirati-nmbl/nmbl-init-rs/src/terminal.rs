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
use std::path::PathBuf;

use crate::config::Config;
use crate::error::NmblError;

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
    },

    /// Fire the previously-loaded kexec image via
    /// `reboot(LINUX_REBOOT_CMD_KEXEC)`. The image was already loaded
    /// (and the filesystems were detached) inside `boot::kexec_into`
    /// before the value reached this variant — only the cutover
    /// syscall is deferred to the dispatcher.
    Kexec,
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
