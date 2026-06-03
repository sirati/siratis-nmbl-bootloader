use std::ffi::CString;
use std::path::Path;

use nix::sys::reboot::{RebootMode, reboot};
use nix::unistd::execve;

use nmbl_init::error::format_chain;
use nmbl_init::shell::{print_banner, print_halt_banner};
use nmbl_init::terminal::{TerminalAction, redirect_stdio_for_execve};
use nmbl_init::{log, nmbl_info, nmbl_warn};

use log::NMBL_LOG_PATH;

/// Dispatch the final [`TerminalAction`] produced by the inner
/// layers. Single point of `execve(2)` / `reboot(2)` / `reboot
/// (RB_KEXEC)` in the entire crate — by the time control reaches
/// here every `Drop` has run via normal stack unwinding, so the
/// freshly-execve'd shell or freshly-kexec'd kernel sees a clean VT.
///
/// All four variants diverge on success; on failure they fall through
/// to [`halt_final`] which performs a final `reboot(RB_HALT_SYSTEM)`
/// (or `libc::_exit` if the kernel refuses).
#[allow(
    clippy::needless_pass_by_value,
    reason = "TerminalAction is consumed exactly once at the top of main; \
              taking by value makes the move explicit"
)]
pub(super) fn execute_terminal_action(action: TerminalAction) -> ! {
    // Persist the byte-ring transcript before the no-return syscall.
    // The byte ring lives in RAM only; once we kexec / reboot / execve
    // it is gone. Disk-flushing here means the operator's emergency
    // shell (Execve path), and the kexec-staging step in `kexec_into`
    // (Kexec path), both have a fresh on-disk snapshot to work with.
    // Failures must not block the terminal action — a missing log is
    // strictly less bad than failing to reboot a wedged system.
    let log_path = Path::new(NMBL_LOG_PATH);
    if let Some(parent) = log_path.parent() {
        // EEXIST is the expected case after the first call; any other
        // error gets surfaced by the flush_to attempt below.
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(err) = log::flush_to(log_path) {
        nmbl_warn!("failed to flush log to {}: {err}", log_path.display());
    }

    match action {
        TerminalAction::Reboot => {
            eprintln!("[nmbl] operator (or timeout) chose reboot");
            let _ = reboot(RebootMode::RB_AUTOBOOT);
            halt_final("reboot(RB_AUTOBOOT) returned; halting")
        }
        TerminalAction::HaltWithBanner { cause } => {
            print_halt_banner(&cause);
            halt_final("halt-with-banner")
        }
        TerminalAction::Execve {
            path,
            argv,
            env,
            banner,
            rescue_handoff,
        } => {
            // SEAL BACKSTOP (G10): the LAST line of defense before the
            // execve syscall hands PID 1 to a shell. An `Execve` action is
            // only ever produced AFTER a G-site seal (G4 `rescue::dispatch`
            // is the authoritative, `requireTpm`-aware seal), so this hits
            // the idempotent latch and returns instantly. If — by some
            // future refactor — an unsealed `Execve` reaches here, this
            // caps fail-closed (`require_tpm=false`: degrade-open on no-TPM,
            // but a present-but-uncappable TPM still halts). `dispatch_execve`
            // REQUIRES the witness by type, so it cannot run without a seal.
            match nmbl_init::policy::seal_secrets_blocking(false) {
                Ok(sealed) => dispatch_execve(sealed, path, argv, env, banner, rescue_handoff),
                Err(seal_err) => {
                    print_halt_banner(&seal_err.into_cause());
                    halt_final("seal-on-execve failed; halting")
                }
            }
        }
        TerminalAction::RebootIntoRescue { cause, sealed } => {
            // The untrusted-image / policy refuse terminus (R-1/R-13). By
            // the time we reach here `relock_and_refuse` has already capped
            // the lock PCR, closed every TPM-unsealed mapper, relocked LUKS,
            // and written the rescue sentinel, and the non-interactive
            // refuse countdown has run to its Enter/timeout. The `Sealed`
            // witness rode along inside the value as the type-level proof
            // that the seal happened before this terminus was built; drop it
            // here — its job (gating construction) is done.
            let _: nmbl_init::policy::Sealed = sealed;
            print_halt_banner(&cause);
            eprintln!("[nmbl] policy refuse: rebooting into rescue (sentinel set, TPM locked)");
            let _ = reboot(RebootMode::RB_AUTOBOOT);
            halt_final("reboot(RB_AUTOBOOT) returned after refuse; halting")
        }
        TerminalAction::Kexec => {
            nmbl_info!("kexec: handing off to new kernel");
            // sys::kexec::execute returns Result<Infallible>; either
            // branch surfaces an error we cannot recover from at this
            // point (the image was already loaded and mounts were
            // detached), so fall through to halt_final.
            match nmbl_init::sys::kexec::execute() {
                Ok(infallible) => match infallible {},
                Err(err) => {
                    eprintln!(
                        "[nmbl] kexec execute returned: {}",
                        format_chain(&err as &dyn std::error::Error)
                    );
                    halt_final("kexec returned; halting")
                }
            }
        }
    }
}

/// Execve arm of [`execute_terminal_action`]. Extracted to keep the
/// parent match arms short (the Execve arm alone carried 35 source
/// lines of redirect + safety comment + execve call).
fn dispatch_execve(
    sealed: nmbl_init::policy::Sealed,
    path: std::ffi::CString,
    argv: Vec<std::ffi::CString>,
    env: Vec<std::ffi::CString>,
    banner: Option<nmbl_init::terminal::EmergencyBanner>,
    rescue_handoff: bool,
) -> ! {
    // The `Sealed` witness proves the lock PCR was capped and every
    // TPM-unsealed mapper closed before this single PID1 execve waist
    // (re-audit C-1). Required by type so the execve cannot run unsealed.
    let _sealed = sealed;
    if let Some(b) = banner {
        print_banner(&b);
    }
    // Re-open /dev/console and dup2 it onto 0/1/2 so the
    // freshly-execve'd shell renders on the operator's primary
    // console (framebuffer for head, ttyS0 for serial). Every
    // boot-console `Drop` has already fired by now via normal
    // stack unwinding, so the fds we just opened are the ones
    // the shell will inherit.
    //
    // On the rescue handoff this is best-effort: the rescue
    // root's /dev may not be fully populated (the full-system
    // `/init` mounts devtmpfs itself as its first step), so a
    // failed redirect must NOT halt — the entrypoint manages
    // its own console (`exec bash < /dev/console`) and halting
    // here would strand the operator. We log and execve anyway
    // with the inherited fds. For a non-rescue execve a redirect
    // failure stays fatal: an execve into invisibility is worse
    // than halting with a banner.
    if let Err(err) = redirect_stdio_for_execve() {
        eprintln!(
            "[nmbl] cannot redirect stdio before execve: {}",
            format_chain(&err as &dyn std::error::Error)
        );
        if rescue_handoff {
            eprintln!("[nmbl] rescue: stdio redirect unavailable, proceeding with inherited fds");
        } else {
            halt_final("stdio redirect failed; halting")
        }
    }
    let argv_refs: Vec<&CString> = argv.iter().collect();
    let env_refs: Vec<&CString> = env.iter().collect();
    // execve safety: single PID1 handoff point — every console/DRM Drop has run via stack unwinding, so the framebuffer/tty is back in the state the target program expects.
    let _ = execve(&path, &argv_refs, &env_refs);
    halt_final("execve returned; halting")
}

/// Print a one-line final-fallback message and halt. Diverges via
/// `reboot(RB_HALT_SYSTEM)` on success or `libc::_exit(1)` if the
/// kernel refuses (lacking CAP_SYS_BOOT in a sandbox, not PID 1, …).
pub(super) fn halt_final(reason: &str) -> ! {
    eprintln!("[nmbl] {reason}");
    let _ = reboot(RebootMode::RB_HALT_SYSTEM);
    // SAFETY: libc::_exit is async-signal-safe and unconditionally
    // terminates the process; no crate wraps it (rustix issue #844).
    unsafe { libc::_exit(1) };
}
