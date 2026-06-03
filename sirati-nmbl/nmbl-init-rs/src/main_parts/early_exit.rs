use std::process::ExitCode;

use nmbl_init::config::Config;
#[cfg(feature = "stateful")]
use nmbl_init::error::format_chain;

use super::args::Args;

/// Handle the `--validate-config` / `--init-state` / `--boot-succeeded`
/// early-exit modes. `Some(code)` exits; `None` continues a normal boot.
///
/// Each mode is its own helper returning `Option<ExitCode>`; this entry
/// fn tries them in the same order the inline blocks ran, so the first
/// flag present wins (arg parsing already enforces mutual exclusion).
pub(super) fn handle_early_exit_modes(args: &Args) -> Option<ExitCode> {
    if let Some(code) = validate_config_mode(args) {
        return Some(code);
    }
    if let Some(code) = validate_fragment_mode(args) {
        return Some(code);
    }
    if let Some(code) = validate_hardware_mode(args) {
        return Some(code);
    }
    if let Some(code) = validate_initrm_mode(args) {
        return Some(code);
    }
    if let Some(code) = print_gen_id_mode(args) {
        return Some(code);
    }
    if let Some(code) = validate_closure_mode(args) {
        return Some(code);
    }
    if let Some(code) = stateful_modes(args) {
        return Some(code);
    }
    remote_tui_mode()
}

/// Build-time validation hook: load and validate the given config
/// file, print the outcome, and exit.
fn validate_config_mode(args: &Args) -> Option<ExitCode> {
    let path = args.validate_config.as_deref()?;
    Some(match Config::load(path) {
        Ok(_) => {
            println!("nmbl-init: config OK: {}", path.display());
            ExitCode::from(0)
        }
        Err(err) => {
            eprintln!("nmbl-init: config invalid at {}: {err}", path.display());
            ExitCode::from(1)
        }
    })
}

/// Installer-side staged-boot fragment check: load+parse a partial
/// config overlay (NOT a full Config) and report OK/errors. The
/// signature is verified separately at boot; this only catches schema
/// mistakes (unknown keys, malformed TOML) before the fragment ships.
#[cfg(feature = "staged-boot")]
fn validate_fragment_mode(args: &Args) -> Option<ExitCode> {
    let path = args.validate_fragment.as_deref()?;
    Some(match nmbl_init::config::load_fragment(path) {
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
    })
}

#[cfg(not(feature = "staged-boot"))]
fn validate_fragment_mode(_args: &Args) -> Option<ExitCode> {
    None
}

/// Target-machine hardware check (read-only, zero side effects). Loads
/// the toml, probes each declared device against the real hardware,
/// and ALWAYS hard-errors on any failure — the warn-vs-abort decision
/// is the Nix install script's, not ours.
fn validate_hardware_mode(args: &Args) -> Option<ExitCode> {
    let path = args.validate_hardware.as_deref()?;
    Some(match Config::load(path) {
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
    })
}

/// Initramfs dry-run validator: run the REAL boot control flow ×4
/// scenarios against the extracted-initrd closure under the
/// side-effect-free `DryRunSys`, collect "missing file" findings, and
/// (optionally) structurally validate the efi-stub UKI. Sandbox-safe:
/// touches only the closure root + the passed paths. Lists every finding
/// (mirroring validate_hardware's style) and exits 1 when not clean.
fn validate_initrm_mode(args: &Args) -> Option<ExitCode> {
    let path = args.validate_initrm.as_deref()?;
    Some(match Config::load(path) {
        Ok(config) => {
            let closure_root = args
                .initrm_closure
                .as_deref()
                .unwrap_or_else(|| std::path::Path::new("/"));
            let report =
                crate::validate_initrm::validate_initrm(&config, args.uki.as_deref(), closure_root);
            if report.is_clean() {
                println!(
                    "nmbl-init: initramfs OK for {} (closure {})",
                    path.display(),
                    closure_root.display()
                );
                ExitCode::from(0)
            } else {
                eprint!(
                    "nmbl-init: initramfs validation FAILED for {} (closure {}):\n{}",
                    path.display(),
                    closure_root.display(),
                    report.render()
                );
                ExitCode::from(1)
            }
        }
        Err(err) => {
            eprintln!("nmbl-init: config invalid at {}: {err}", path.display());
            ExitCode::from(1)
        }
    })
}

/// Shared generation-id computation (FIX-07): print the content-addressed
/// `gen_id` for a system toplevel / profile-link path and exit. The install
/// signer (#53) calls this to compute the `/boot/nmbl/sigs/<gen-id>/…` path
/// the in-initramfs verifier scans, so signer and verifier share ONE
/// derivation. Pure + side-effect-free (a canonicalize + basename).
fn print_gen_id_mode(args: &Args) -> Option<ExitCode> {
    let path = args.print_gen_id.as_deref()?;
    // The host signer runs this on the real filesystem; a sender-less `RealSys`
    // gives the sync `FsOps::canonicalize` (a plain `std::fs::canonicalize`)
    // without needing a poller.
    let ops = nmbl_init::sys::ops::RealSys::sync_only();
    Some(match nmbl_init::generations::gen_id_of_path(&ops, path) {
        Ok(id) => {
            println!("{id}");
            ExitCode::from(0)
        }
        Err(err) => {
            eprintln!(
                "nmbl-init: --print-gen-id failed for {}: {err}",
                path.display()
            );
            ExitCode::from(1)
        }
    })
}

/// NixOS-only sandbox check: the toml must MATCH the NixOS filesystem
/// closure JSON. `config_toml` is guaranteed present by arg parsing.
fn validate_closure_mode(args: &Args) -> Option<ExitCode> {
    let json = args.validate_closure.as_deref()?;
    let toml = args
        .config_toml
        .as_deref()
        .unwrap_or_else(|| std::path::Path::new("/dev/null"));
    Some(
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
    )
}

/// Installer and systemd-unit early-exit dispatches.
#[cfg(feature = "stateful")]
fn stateful_modes(args: &Args) -> Option<ExitCode> {
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
    None
}

#[cfg(not(feature = "stateful"))]
fn stateful_modes(_args: &Args) -> Option<ExitCode> {
    None
}

/// Remote-TUI client mode: any non-PID-1 invocation of the binary
/// (the operator's `nmbl-init` in a rescue login, or the chrooted
/// rescue view) connects to PID 1's control socket, hands across its
/// controlling terminal, and goes quiescent while PID 1 drives it.
/// PID 1 itself (the boot path) never takes this branch.
#[cfg(feature = "remote-tui")]
fn remote_tui_mode() -> Option<ExitCode> {
    if nix::unistd::getpid().as_raw() != 1 {
        return Some(nmbl_init::ipc::tui_socket::connect_and_serve());
    }
    None
}

#[cfg(not(feature = "remote-tui"))]
fn remote_tui_mode() -> Option<ExitCode> {
    None
}
