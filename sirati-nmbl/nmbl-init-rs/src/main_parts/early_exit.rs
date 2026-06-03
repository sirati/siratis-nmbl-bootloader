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

    // Installer-side staged-boot fragment check: load+parse a partial
    // config overlay (NOT a full Config) and report OK/errors. The
    // signature is verified separately at boot; this only catches schema
    // mistakes (unknown keys, malformed TOML) before the fragment ships.
    #[cfg(feature = "staged-boot")]
    if let Some(path) = args.validate_fragment.as_deref() {
        return Some(match nmbl_init::config::load_fragment(path) {
            Ok(_) => {
                println!("nmbl-init: config fragment OK: {}", path.display());
                ExitCode::from(0)
            }
            Err(err) => {
                eprintln!(
                    "nmbl-init: config fragment invalid at {}: {err}",
                    path.display()
                );
                ExitCode::from(1)
            }
        });
    }

    // Target-machine hardware check (read-only, zero side effects). Loads
    // the toml, probes each declared device against the real hardware,
    // and ALWAYS hard-errors on any failure — the warn-vs-abort decision
    // is the Nix install script's, not ours.
    if let Some(path) = args.validate_hardware.as_deref() {
        return Some(match Config::load(path) {
            Ok(config) => {
                let failures = nmbl_init::validate::validate_hardware(&config, &args.tools);
                if failures.is_empty() {
                    println!("nmbl-init: hardware OK for {}", path.display());
                    ExitCode::from(0)
                } else {
                    eprintln!(
                        "nmbl-init: hardware validation FAILED for {} ({} problem(s)):",
                        path.display(),
                        failures.len()
                    );
                    for f in &failures {
                        eprintln!("  - {f}");
                    }
                    ExitCode::from(1)
                }
            }
            Err(err) => {
                eprintln!("nmbl-init: config invalid at {}: {err}", path.display());
                ExitCode::from(1)
            }
        });
    }

    // NixOS-only sandbox check: the toml must MATCH the NixOS filesystem
    // closure JSON. `config_toml` is guaranteed present by arg parsing.
    if let Some(json) = args.validate_closure.as_deref() {
        let toml = args
            .config_toml
            .as_deref()
            .unwrap_or_else(|| std::path::Path::new("/dev/null"));
        return Some(
            match nmbl_init::validate::validate_nix_filesystem_closure(json, toml) {
                Ok(()) => {
                    println!(
                        "nmbl-init: config {} matches NixOS filesystem closure {}",
                        toml.display(),
                        json.display()
                    );
                    ExitCode::from(0)
                }
                Err(err) => {
                    eprintln!("nmbl-init: NixOS filesystem closure validation FAILED: {err}");
                    ExitCode::from(1)
                }
            },
        );
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
