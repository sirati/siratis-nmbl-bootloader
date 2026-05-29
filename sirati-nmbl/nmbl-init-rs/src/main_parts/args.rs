use std::path::PathBuf;

const DEFAULT_CONFIG_PATH: &str = "/etc/nmbl/config.toml";

#[derive(Debug)]
pub(super) struct Args {
    pub(super) config_path: PathBuf,
    pub(super) errored_report: Option<PathBuf>,
    pub(super) validate_config: Option<PathBuf>,
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
        } else if let Some(rest) = arg.strip_prefix("--init-state=") {
            #[cfg(feature = "stateful")]
            {
                init_state_dir = Some(PathBuf::from(rest));
            }
            #[cfg(not(feature = "stateful"))]
            {
                let _ = rest;
                return Err(
                    "--init-state requires nmbl-init to be built with the `stateful` feature"
                        .to_string(),
                );
            }
        } else if arg == "--init-state" {
            #[cfg(feature = "stateful")]
            {
                let Some(v) = iter.next() else {
                    return Err("--init-state requires a directory argument".to_string());
                };
                init_state_dir = Some(PathBuf::from(v));
            }
            #[cfg(not(feature = "stateful"))]
            {
                return Err(
                    "--init-state requires nmbl-init to be built with the `stateful` feature"
                        .to_string(),
                );
            }
        } else if let Some(rest) = arg.strip_prefix("--boot-succeeded=") {
            #[cfg(feature = "stateful")]
            {
                boot_succeeded_dir = Some(PathBuf::from(rest));
            }
            #[cfg(not(feature = "stateful"))]
            {
                let _ = rest;
                return Err(
                    "--boot-succeeded requires nmbl-init to be built with the `stateful` feature"
                        .to_string(),
                );
            }
        } else if arg == "--boot-succeeded" {
            #[cfg(feature = "stateful")]
            {
                let Some(v) = iter.next() else {
                    return Err("--boot-succeeded requires a directory argument".to_string());
                };
                boot_succeeded_dir = Some(PathBuf::from(v));
            }
            #[cfg(not(feature = "stateful"))]
            {
                return Err(
                    "--boot-succeeded requires nmbl-init to be built with the `stateful` feature"
                        .to_string(),
                );
            }
        }
    }

    // Mutual exclusion across the three early-exit modes. Each mode
    // funnels into a different exit path (validate, init-state,
    // boot-succeeded); combining them would silently pick one and
    // drop the others, masking an operator typo.
    #[cfg(feature = "stateful")]
    {
        let count = u8::from(validate_config.is_some())
            + u8::from(init_state_dir.is_some())
            + u8::from(boot_succeeded_dir.is_some());
        if count > 1 {
            return Err(
                "--validate-config, --init-state, and --boot-succeeded are mutually exclusive"
                    .to_string(),
            );
        }
    }

    Ok(Args {
        config_path,
        errored_report,
        validate_config,
        #[cfg(feature = "stateful")]
        init_state_dir,
        #[cfg(feature = "stateful")]
        boot_succeeded_dir,
    })
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
}
