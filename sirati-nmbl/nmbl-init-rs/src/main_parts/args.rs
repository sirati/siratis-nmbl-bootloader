use std::path::PathBuf;

use nmbl_init::validate::ToolPaths;

const DEFAULT_CONFIG_PATH: &str = "/etc/nmbl/config.toml";

#[derive(Debug)]
pub(super) struct Args {
    pub(super) config_path: PathBuf,
    pub(super) errored_report: Option<PathBuf>,
    pub(super) validate_config: Option<PathBuf>,
    /// `--validate-config-fragment=<toml>`: installer-side load+parse of a
    /// staged-boot config fragment (a partial overlay). Mirrors
    /// `--validate-config` but accepts a partial schema. Staged-boot builds
    /// only; mutually exclusive with the other early-exit modes.
    #[cfg(feature = "staged-boot")]
    pub(super) validate_fragment: Option<PathBuf>,
    /// `--validate-hardware=<toml>`: read-only hardware check on the real
    /// target machine. Mutually exclusive with the other early-exit modes.
    pub(super) validate_hardware: Option<PathBuf>,
    /// `--validate-nix-filesystem-closure=<json>`: NixOS-only sandbox
    /// check. Paired with `config_toml` (its own `--config-toml=<toml>`).
    pub(super) validate_closure: Option<PathBuf>,
    /// Companion toml for `--validate-nix-filesystem-closure` (the plain
    /// `--validate-config` arg is mutually exclusive, so the closure mode
    /// carries its toml separately).
    pub(super) config_toml: Option<PathBuf>,
    /// Tool paths supplied to `--validate-hardware` via `--tool=<kind>:<path>`.
    pub(super) tools: ToolPaths,
    /// `--print-gen-id=<toplevel>`: print the shared content-addressed
    /// generation id (FIX-07) for the given system toplevel / profile-link
    /// path and exit. The install signer (#53) uses this to compute the
    /// `/boot/nmbl/sigs/<gen-id>/…` path the in-initramfs verifier scans, so
    /// signer and verifier share ONE id derivation. Mutually exclusive with
    /// the other early-exit modes.
    pub(super) print_gen_id: Option<PathBuf>,
    /// Installer-side: initialise (or validate) state.bin under the
    /// given directory and exit. Mutually exclusive with
    /// `validate_config` and `boot_succeeded_dir`.
    #[cfg(feature = "stateful")]
    pub(super) init_state_dir: Option<PathBuf>,
    /// systemd-unit side: flip `last_boot_succeeded = true` in
    /// state.bin under the given directory and exit. Mutually
    /// exclusive with `validate_config` and `init_state_dir`.
    #[cfg(feature = "stateful")]
    pub(super) boot_succeeded_dir: Option<PathBuf>,
}

/// Hand-rolled arg parsing: clap is too big for the size budget. We
/// recognise `--config=<v>` / `--config <v>` and the same two forms
/// for `--errored`, `--validate-config`, `--init-state`, and
/// `--boot-succeeded`. Anything else is silently ignored — PID 1 has
/// no useful "usage" target to print to.
///
/// Returns `Err(String)` when the caller asked for a stateful flag in
/// a binary built without the `stateful` feature, when two mutually
/// exclusive early-exit modes were combined, or when an early-exit
/// flag was passed with no path argument. Those three cases are
/// programmer / operator errors, not normal boot failures, and we
/// surface them to stderr before the panic hook or logger come up.
pub(super) fn parse_args() -> std::result::Result<Args, String> {
    parse_args_from(std::env::args_os().skip(1))
}

/// Pure parsing core: takes an iterator of arg-like values so unit
/// tests can drive the parser without touching `std::env::args_os()`.
/// `parse_args` is the production entry point and stays a one-liner.
pub(super) fn parse_args_from<I, S>(args: I) -> std::result::Result<Args, String>
where
    I: IntoIterator<Item = S>,
    S: Into<std::ffi::OsString>,
{
    let mut config_path = PathBuf::from(DEFAULT_CONFIG_PATH);
    let mut errored_report: Option<PathBuf> = None;
    let mut validate_config: Option<PathBuf> = None;
    #[cfg(feature = "staged-boot")]
    let mut validate_fragment: Option<PathBuf> = None;
    let mut validate_hardware: Option<PathBuf> = None;
    let mut validate_closure: Option<PathBuf> = None;
    let mut config_toml: Option<PathBuf> = None;
    let mut tools = ToolPaths::default();
    let mut print_gen_id: Option<PathBuf> = None;
    #[cfg(feature = "stateful")]
    let mut init_state_dir: Option<PathBuf> = None;
    #[cfg(feature = "stateful")]
    let mut boot_succeeded_dir: Option<PathBuf> = None;

    let mut iter = args.into_iter().map(Into::into);
    while let Some(arg_os) = iter.next() {
        let arg = arg_os.to_string_lossy();
        if let Some(rest) = arg.strip_prefix("--config=") {
            config_path = PathBuf::from(rest);
        } else if arg == "--config"
            && let Some(v) = iter.next()
        {
            config_path = PathBuf::from(v);
        } else if let Some(rest) = arg.strip_prefix("--errored=") {
            errored_report = Some(PathBuf::from(rest));
        } else if arg == "--errored"
            && let Some(v) = iter.next()
        {
            errored_report = Some(PathBuf::from(v));
        } else if let Some(rest) = arg.strip_prefix("--validate-config=") {
            validate_config = Some(PathBuf::from(rest));
        } else if arg == "--validate-config"
            && let Some(v) = iter.next()
        {
            validate_config = Some(PathBuf::from(v));
        } else if let Some(value) =
            parse_fragment_flag(&arg, "--validate-config-fragment", &mut iter)?
        {
            #[cfg(feature = "staged-boot")]
            {
                validate_fragment = Some(value);
            }
            #[cfg(not(feature = "staged-boot"))]
            {
                let _ = value;
            }
        } else if let Some(rest) = arg.strip_prefix("--validate-hardware=") {
            validate_hardware = Some(PathBuf::from(rest));
        } else if arg == "--validate-hardware"
            && let Some(v) = iter.next()
        {
            validate_hardware = Some(PathBuf::from(v));
        } else if let Some(rest) = arg.strip_prefix("--validate-nix-filesystem-closure=") {
            validate_closure = Some(PathBuf::from(rest));
        } else if arg == "--validate-nix-filesystem-closure"
            && let Some(v) = iter.next()
        {
            validate_closure = Some(PathBuf::from(v));
        } else if let Some(rest) = arg.strip_prefix("--config-toml=") {
            config_toml = Some(PathBuf::from(rest));
        } else if arg == "--config-toml"
            && let Some(v) = iter.next()
        {
            config_toml = Some(PathBuf::from(v));
        } else if let Some(rest) = arg.strip_prefix("--tool=") {
            tools.insert_spec(rest)?;
        } else if arg == "--tool"
            && let Some(v) = iter.next()
        {
            tools.insert_spec(&v.to_string_lossy())?;
        } else if let Some(rest) = arg.strip_prefix("--print-gen-id=") {
            print_gen_id = Some(PathBuf::from(rest));
        } else if arg == "--print-gen-id"
            && let Some(v) = iter.next()
        {
            print_gen_id = Some(PathBuf::from(v));
        } else if let Some(value) = parse_stateful_flag(&arg, "--init-state", &mut iter)? {
            #[cfg(feature = "stateful")]
            {
                init_state_dir = Some(value);
            }
            #[cfg(not(feature = "stateful"))]
            {
                let _ = value;
            }
        } else if let Some(value) = parse_stateful_flag(&arg, "--boot-succeeded", &mut iter)? {
            #[cfg(feature = "stateful")]
            {
                boot_succeeded_dir = Some(value);
            }
            #[cfg(not(feature = "stateful"))]
            {
                let _ = value;
            }
        }
    }

    // Mutual exclusion across all early-exit modes. Each funnels into a
    // different exit path; combining them would silently pick one and
    // drop the rest, masking an operator typo. The three validate modes
    // are always present; the stateful pair only in stateful builds.
    #[cfg(feature = "stateful")]
    let stateful_modes =
        u8::from(init_state_dir.is_some()) + u8::from(boot_succeeded_dir.is_some());
    #[cfg(not(feature = "stateful"))]
    let stateful_modes = 0u8;
    #[cfg(feature = "staged-boot")]
    let fragment_mode = u8::from(validate_fragment.is_some());
    #[cfg(not(feature = "staged-boot"))]
    let fragment_mode = 0u8;
    let early_exit_count = u8::from(validate_config.is_some())
        + u8::from(validate_hardware.is_some())
        + u8::from(validate_closure.is_some())
        + fragment_mode
        + u8::from(print_gen_id.is_some())
        + stateful_modes;
    if early_exit_count > 1 {
        return Err(
            "the early-exit modes (--validate-config, --validate-config-fragment, \
             --validate-hardware, --validate-nix-filesystem-closure, --print-gen-id, \
             --init-state, --boot-succeeded) are mutually exclusive"
                .to_string(),
        );
    }

    // The closure check needs its companion toml.
    if validate_closure.is_some() && config_toml.is_none() {
        return Err("--validate-nix-filesystem-closure requires --config-toml=<toml>".to_string());
    }

    Ok(Args {
        config_path,
        errored_report,
        validate_config,
        #[cfg(feature = "staged-boot")]
        validate_fragment,
        validate_hardware,
        validate_closure,
        config_toml,
        tools,
        print_gen_id,
        #[cfg(feature = "stateful")]
        init_state_dir,
        #[cfg(feature = "stateful")]
        boot_succeeded_dir,
    })
}

/// Recognise a stateful-only flag (`--init-state` / `--boot-succeeded`)
/// in both `--flag=<v>` and `--flag <v>` forms. Returns `Ok(None)` when
/// `arg` is not this flag, `Ok(Some(path))` when it matched and the path
/// was consumed (stateful builds only), `Err` when the path argument is
/// missing or — on a non-stateful build — the flag was used at all.
fn parse_stateful_flag<I>(
    arg: &str,
    flag: &'static str,
    iter: &mut I,
) -> std::result::Result<Option<PathBuf>, String>
where
    I: Iterator<Item = std::ffi::OsString>,
{
    let equals_prefix = format!("{flag}=");
    if let Some(rest) = arg.strip_prefix(&equals_prefix) {
        #[cfg(feature = "stateful")]
        {
            return Ok(Some(PathBuf::from(rest)));
        }
        #[cfg(not(feature = "stateful"))]
        {
            let _ = rest;
            return Err(format!(
                "{flag} requires nmbl-init to be built with the `stateful` feature"
            ));
        }
    }
    if arg == flag {
        #[cfg(feature = "stateful")]
        {
            let Some(v) = iter.next() else {
                return Err(format!("{flag} requires a directory argument"));
            };
            return Ok(Some(PathBuf::from(v)));
        }
        #[cfg(not(feature = "stateful"))]
        {
            let _ = iter;
            return Err(format!(
                "{flag} requires nmbl-init to be built with the `stateful` feature"
            ));
        }
    }
    Ok(None)
}

/// Recognise `--validate-config-fragment` in both `--flag=<v>` and
/// `--flag <v>` forms. Returns `Ok(None)` when `arg` is not this flag,
/// `Ok(Some(path))` when it matched (staged-boot builds), `Err` when the
/// path argument is missing or — on a non-staged-boot build — the flag was
/// used at all (so the operator is not silently handed a no-op).
fn parse_fragment_flag<I>(
    arg: &str,
    flag: &'static str,
    iter: &mut I,
) -> std::result::Result<Option<PathBuf>, String>
where
    I: Iterator<Item = std::ffi::OsString>,
{
    let equals_prefix = format!("{flag}=");
    if let Some(rest) = arg.strip_prefix(&equals_prefix) {
        #[cfg(feature = "staged-boot")]
        {
            return Ok(Some(PathBuf::from(rest)));
        }
        #[cfg(not(feature = "staged-boot"))]
        {
            let _ = rest;
            return Err(format!(
                "{flag} requires nmbl-init to be built with the `staged-boot` feature"
            ));
        }
    }
    if arg == flag {
        #[cfg(feature = "staged-boot")]
        {
            let Some(v) = iter.next() else {
                return Err(format!("{flag} requires a path argument"));
            };
            return Ok(Some(PathBuf::from(v)));
        }
        #[cfg(not(feature = "staged-boot"))]
        {
            let _ = iter;
            return Err(format!(
                "{flag} requires nmbl-init to be built with the `staged-boot` feature"
            ));
        }
    }
    Ok(None)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
#[path = "args_tests.rs"]
mod tests;
