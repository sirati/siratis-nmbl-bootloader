use std::process::ExitCode;

use nmbl_init::config::Config;
#[cfg(feature = "stateful")]
use nmbl_init::error::format_chain;

use super::args::Args;

/// Handle the `--validate-config` / `--init-state` / `--boot-succeeded`
/// early-exit modes. `Some(code)` exits; `None` continues a normal boot.
pub(super) fn handle_early_exit_modes(args: &Args) -> Option<ExitCode> {
    // Build-time validation hook: load and validate the given config
    // file, print the outcome, and exit.
    if let Some(path) = args.validate_config.as_deref() {
        return Some(match Config::load(path) {
            Ok(_) => {
                println!("nmbl-init: config OK: {}", path.display());
                ExitCode::from(0)
            }
            Err(err) => {
                eprintln!("nmbl-init: config invalid at {}: {err}", path.display());
                ExitCode::from(1)
            }
        });
    }

    // Installer and systemd-unit early-exit dispatches.
    #[cfg(feature = "stateful")]
    {
        if let Some(dir) = args.init_state_dir.as_deref() {
            return Some(match nmbl_init::state::init_or_validate(dir) {
                Ok(_) => {
                    println!("nmbl-init: state.bin OK under {}", dir.display());
                    ExitCode::from(0)
                }
                Err(err) => {
                    eprintln!(
                        "nmbl-init: --init-state failed for {}: {}",
                        dir.display(),
                        format_chain(&err as &dyn std::error::Error),
                    );
                    ExitCode::from(1)
                }
            });
        }
        if let Some(dir) = args.boot_succeeded_dir.as_deref() {
            return Some(match nmbl_init::state::mark_boot_succeeded(dir) {
                Ok(()) => ExitCode::from(0),
                Err(err) => {
                    eprintln!(
                        "nmbl-init: --boot-succeeded failed for {}: {}",
                        dir.display(),
                        format_chain(&err as &dyn std::error::Error),
                    );
                    ExitCode::from(1)
                }
            });
        }
    }

    // Remote-TUI client mode: any non-PID-1 invocation of the binary
    // (the operator's `nmbl-init` in a rescue login, or the chrooted
    // rescue view) connects to PID 1's control socket, hands across its
    // controlling terminal, and goes quiescent while PID 1 drives it.
    // PID 1 itself (the boot path) never takes this branch.
    #[cfg(feature = "remote-tui")]
    {
        if nix::unistd::getpid().as_raw() != 1 {
            return Some(nmbl_init::ipc::tui_socket::connect_and_serve());
        }
    }

    None
}
