use std::path::PathBuf;

use nmbl_init::validate::ToolPaths;

const DEFAULT_CONFIG_PATH: &str = "/etc/nmbl/config.toml";

#[derive(Debug)]
pub(super) struct Args {
    pub(super) config_path: PathBuf,
    pub(super) errored_report: Option<PathBuf>,
    pub(super) validate_config: Option<PathBuf>,
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
    let mut validate_hardware: Option<PathBuf> = None;
    let mut validate_closure: Option<PathBuf> = None;
    let mut config_toml: Option<PathBuf> = None;
    let mut tools = ToolPaths::default();
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
    let early_exit_count = u8::from(validate_config.is_some())
        + u8::from(validate_hardware.is_some())
        + u8::from(validate_closure.is_some())
        + stateful_modes;
    if early_exit_count > 1 {
        return Err(
            "the early-exit modes (--validate-config, --validate-hardware, \
             --validate-nix-filesystem-closure, --init-state, --boot-succeeded) \
             are mutually exclusive"
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
        validate_hardware,
        validate_closure,
        config_toml,
        tools,
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

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;
    use std::path::Path;

    #[cfg(feature = "stateful")]
    #[test]
    fn init_state_with_path_parses() {
        let args =
            parse_args_from(["--init-state", "/some/path"]).expect("--init-state should parse");
        assert_eq!(
            args.init_state_dir.as_deref(),
            Some(Path::new("/some/path"))
        );
        assert!(args.boot_succeeded_dir.is_none());
        assert!(args.validate_config.is_none());
    }

    #[cfg(feature = "stateful")]
    #[test]
    fn init_state_equals_form_parses() {
        let args =
            parse_args_from(["--init-state=/some/path"]).expect("--init-state=… should parse");
        assert_eq!(
            args.init_state_dir.as_deref(),
            Some(Path::new("/some/path"))
        );
    }

    #[cfg(feature = "stateful")]
    #[test]
    fn boot_succeeded_with_path_parses() {
        let args = parse_args_from(["--boot-succeeded", "/some/path"])
            .expect("--boot-succeeded should parse");
        assert_eq!(
            args.boot_succeeded_dir.as_deref(),
            Some(Path::new("/some/path"))
        );
        assert!(args.init_state_dir.is_none());
        assert!(args.validate_config.is_none());
    }

    #[cfg(feature = "stateful")]
    #[test]
    fn init_state_and_boot_succeeded_are_mutually_exclusive() {
        let err = parse_args_from(["--init-state", "/a", "--boot-succeeded", "/b"])
            .expect_err("both flags at once must be rejected");
        assert!(err.contains("mutually exclusive"), "{err}");
    }

    #[cfg(feature = "stateful")]
    #[test]
    fn validate_config_and_init_state_are_mutually_exclusive() {
        let err = parse_args_from(["--validate-config", "/c", "--init-state", "/a"])
            .expect_err("validate-config + init-state must be rejected");
        assert!(err.contains("mutually exclusive"), "{err}");
    }

    #[cfg(feature = "stateful")]
    #[test]
    fn init_state_without_argument_errors() {
        let err =
            parse_args_from(["--init-state"]).expect_err("--init-state without dir must error");
        assert!(err.contains("requires a directory argument"), "{err}");
    }

    #[cfg(feature = "stateful")]
    #[test]
    fn boot_succeeded_without_argument_errors() {
        let err = parse_args_from(["--boot-succeeded"])
            .expect_err("--boot-succeeded without dir must error");
        assert!(err.contains("requires a directory argument"), "{err}");
    }

    #[cfg(not(feature = "stateful"))]
    #[test]
    fn init_state_without_feature_errors() {
        // The operator built nmbl-init without `stateful` but still
        // passed `--init-state`; we must not silently ignore — that
        // would leave state.bin uninitialised and bricked installers
        // would be invisible at build time.
        let err = parse_args_from(["--init-state", "/a"])
            .expect_err("--init-state without feature must error");
        assert!(err.contains("stateful"), "{err}");
    }

    #[cfg(not(feature = "stateful"))]
    #[test]
    fn boot_succeeded_without_feature_errors() {
        let err = parse_args_from(["--boot-succeeded", "/a"])
            .expect_err("--boot-succeeded without feature must error");
        assert!(err.contains("stateful"), "{err}");
    }

    #[cfg(not(feature = "stateful"))]
    #[test]
    fn init_state_equals_without_feature_errors() {
        let err = parse_args_from(["--init-state=/a"])
            .expect_err("--init-state=… without feature must error");
        assert!(err.contains("stateful"), "{err}");
    }

    #[test]
    fn unknown_args_are_ignored() {
        // PID 1 has no "usage" target; unknown flags must not abort.
        let args = parse_args_from(["--no-such-flag", "garbage"])
            .expect("unknown flags should be silently dropped");
        assert_eq!(args.config_path, PathBuf::from(DEFAULT_CONFIG_PATH));
        assert!(args.errored_report.is_none());
        assert!(args.validate_config.is_none());
    }

    #[test]
    fn validate_config_parses_in_default_build() {
        let args = parse_args_from(["--validate-config", "/etc/nmbl/config.toml"])
            .expect("--validate-config should parse without stateful feature");
        assert_eq!(
            args.validate_config.as_deref(),
            Some(Path::new("/etc/nmbl/config.toml"))
        );
    }

    #[test]
    fn validate_hardware_parses_both_forms() {
        let a = parse_args_from(["--validate-hardware=/c.toml"]).expect("equals form");
        assert_eq!(a.validate_hardware.as_deref(), Some(Path::new("/c.toml")));
        let b = parse_args_from(["--validate-hardware", "/c.toml"]).expect("space form");
        assert_eq!(b.validate_hardware.as_deref(), Some(Path::new("/c.toml")));
    }

    #[test]
    fn validate_hardware_collects_tool_paths() {
        let a = parse_args_from([
            "--validate-hardware=/c.toml",
            "--tool=cryptsetup:/store/bin/cryptsetup",
        ])
        .expect("tool path should parse");
        assert_eq!(
            a.tools.cryptsetup(),
            Some(PathBuf::from("/store/bin/cryptsetup"))
        );
    }

    #[test]
    fn bad_tool_spec_errors() {
        let err = parse_args_from(["--tool=cryptsetup"]).expect_err("missing ':' must error");
        assert!(err.contains("<kind>:<path>"), "{err}");
    }

    #[test]
    fn validate_closure_requires_config_toml() {
        let err = parse_args_from(["--validate-nix-filesystem-closure=/fs.json"])
            .expect_err("closure without --config-toml must error");
        assert!(err.contains("--config-toml"), "{err}");
    }

    #[test]
    fn validate_closure_parses_with_config_toml() {
        let a = parse_args_from([
            "--validate-nix-filesystem-closure=/fs.json",
            "--config-toml=/c.toml",
        ])
        .expect("closure + config-toml should parse");
        assert_eq!(a.validate_closure.as_deref(), Some(Path::new("/fs.json")));
        assert_eq!(a.config_toml.as_deref(), Some(Path::new("/c.toml")));
    }

    #[test]
    fn validate_hardware_and_config_are_mutually_exclusive() {
        let err = parse_args_from(["--validate-config=/a", "--validate-hardware=/b"])
            .expect_err("two validate modes at once must be rejected");
        assert!(err.contains("mutually exclusive"), "{err}");
    }

    #[test]
    fn validate_hardware_and_closure_are_mutually_exclusive() {
        let err = parse_args_from([
            "--validate-hardware=/a",
            "--validate-nix-filesystem-closure=/b",
            "--config-toml=/c",
        ])
        .expect_err("hardware + closure at once must be rejected");
        assert!(err.contains("mutually exclusive"), "{err}");
    }
}
