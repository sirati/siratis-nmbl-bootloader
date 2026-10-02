//! `nmblctl` command-line parsing.
//!
//! Hand-rolled (no clap) to match the workspace's small-dependency posture.
//! Every parse outcome is a value so `parse_args` is unit-testable without a
//! process; `main.rs` maps the parsed [`Cli`] to actions.

/// The `--color` policy, git-style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorChoice {
    /// Colour when stdout is a TTY (the default).
    #[default]
    Auto,
    /// Always emit colour.
    Always,
    /// Never emit colour.
    Never,
}

/// A parsed subcommand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Show the configured boot chain.
    Chain,
    /// Show the current boot status/health.
    Status,
    /// Set the rescue sentinel and reboot. `yes` skips the confirmation.
    RebootRescue { yes: bool },
    /// One-shot boot-into a generation next reboot. `generation` selects a
    /// specific one non-interactively; `None` opens the TUI selector.
    RebootInto { generation: Option<u32> },
    /// Explicit one-use operator retry despite automatic recovery exhaustion.
    RetryGeneration { generation: u32 },
    /// Set/show the persistent default generation. `generation` sets a specific
    /// number, `Some(None)`-via-`latest` selects the artificial "latest", and
    /// `None` opens the TUI selector; `show` only prints the current default.
    Default {
        generation: Option<u32>,
        latest: bool,
        show: bool,
    },
    /// Print top-level usage.
    Help,
}

/// The fully-parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cli {
    /// The subcommand.
    pub command: Command,
    /// The colour policy.
    pub color: ColorChoice,
    /// `--no-pager` was passed.
    pub no_pager: bool,
    pub config: Option<std::path::PathBuf>,
    pub state_dir: Option<std::path::PathBuf>,
    pub system_root: Option<std::path::PathBuf>,
    pub profiles_dir: Option<std::path::PathBuf>,
    pub json: bool,
    pub no_reboot: bool,
}

/// Top-level usage text.
pub const USAGE: &str = "\
nmblctl — control and inspect the NMBL bootloader (root required)

USAGE:
  nmblctl [--color=auto|always|never] [--no-pager] <command>

COMMANDS:
  chain                    show the whole boot chain as configured
  status [--json]          show the current boot, setup and health
  reboot-rescue [--yes]    set the rescue flag and reboot into rescue
  reboot-into [--generation N]
                           boot a chosen generation ONCE next reboot
  retry-generation --generation N
                           retry an installed profile once despite exhaustion
  default [--generation N | --latest | --show]
                           set or show the persistent default generation

GLOBAL OPTIONS:
  --color=auto|always|never  colourise output (default: auto)
  --no-pager                 do not pipe output through a pager
  --config PATH              installed NMBL config in mounted rescue system
  --system-root PATH         mounted installed root (also remaps profiles)
  --profiles-dir PATH        explicit installed profiles directory
  --state-dir PATH           installed persistent NMBL state directory
  --no-reboot                retry-generation: record request without reboot
  -h, --help                 print this help";

/// Parse `args` (argv without the program name) into a [`Cli`].
///
/// Global flags (`--color`, `--no-pager`, `--help`) may appear before or after
/// the subcommand. Returns `Err(message)` on an unknown flag, an unknown
/// command, or a malformed value — the binary prints the message and usage.
pub fn parse_args(args: &[String]) -> Result<Cli, String> {
    let mut config = None;
    let mut state_dir = None;
    let mut system_root = None;
    let mut profiles_dir = None;
    let mut json = false;
    let mut no_reboot = false;
    let mut color = ColorChoice::Auto;
    let mut no_pager = false;
    let mut command: Option<Command> = None;
    // Per-command flags collected as we go, applied when the command is known.
    let mut yes = false;
    let mut generation: Option<u32> = None;
    let mut latest = false;
    let mut show = false;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(cli(Command::Help, color, no_pager)),
            "--no-pager" => no_pager = true,
            "--json" => json = true,
            "--no-reboot" => no_reboot = true,
            "--config" | "--state-dir" | "--profiles-dir" | "--system-root" => {
                let path = std::path::PathBuf::from(
                    it.next().ok_or_else(|| format!("{arg} needs a path"))?,
                );
                if !path.is_absolute() {
                    return Err(format!("{arg} requires an absolute path"));
                }
                match arg.as_str() {
                    "--config" => config = Some(path),
                    "--state-dir" => state_dir = Some(path),
                    "--system-root" => system_root = Some(path),
                    _ => profiles_dir = Some(path),
                }
            }
            "--color" => {
                let v = it
                    .next()
                    .ok_or_else(|| "--color needs a value (auto|always|never)".to_string())?;
                color = parse_color(v)?;
            }
            s if s.starts_with("--color=") => {
                color = parse_color(s.trim_start_matches("--color="))?;
            }
            "--yes" | "-y" => yes = true,
            "--latest" => latest = true,
            "--show" => show = true,
            "--generation" => {
                let v = it
                    .next()
                    .ok_or_else(|| "--generation needs a number".to_string())?;
                generation = Some(parse_gen(v)?);
            }
            s if s.starts_with("--generation=") => {
                generation = Some(parse_gen(s.trim_start_matches("--generation="))?);
            }
            "chain" if command.is_none() => command = Some(Command::Chain),
            "status" if command.is_none() => command = Some(Command::Status),
            "reboot-rescue" if command.is_none() => {
                command = Some(Command::RebootRescue { yes: false });
            }
            "reboot-into" if command.is_none() => {
                command = Some(Command::RebootInto { generation: None });
            }
            "retry-generation" if command.is_none() => {
                command = Some(Command::RetryGeneration { generation: 0 });
            }
            "default" if command.is_none() => {
                command = Some(Command::Default {
                    generation: None,
                    latest: false,
                    show: false,
                });
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    // Bind the collected per-command flags to the chosen command.
    let command = match command {
        None => Command::Help,
        Some(Command::RebootRescue { .. }) => Command::RebootRescue { yes },
        Some(Command::RebootInto { .. }) => Command::RebootInto { generation },
        Some(Command::RetryGeneration { .. }) => {
            if yes || latest || show {
                return Err("retry-generation accepts only --generation".into());
            }
            Command::RetryGeneration {
                generation: generation
                    .filter(|n| *n > 0 && *n < u32::MAX)
                    .ok_or("retry-generation requires --generation N (1..u32::MAX)")?,
            }
        }
        Some(Command::Default { .. }) => {
            if latest && generation.is_some() {
                return Err("--latest and --generation are mutually exclusive".to_string());
            }
            Command::Default {
                generation,
                latest,
                show,
            }
        }
        Some(other) => other,
    };

    if json && command != Command::Status {
        return Err("--json requires status".into());
    }
    if no_reboot && !matches!(command, Command::RetryGeneration { .. }) {
        return Err("--no-reboot requires retry-generation".into());
    }
    let mut result = cli(command, color, no_pager);
    result.config = config;
    result.state_dir = state_dir;
    result.system_root = system_root;
    result.profiles_dir = profiles_dir;
    result.json = json;
    result.no_reboot = no_reboot;
    Ok(result)
}

fn cli(command: Command, color: ColorChoice, no_pager: bool) -> Cli {
    Cli {
        command,
        color,
        no_pager,
        config: None,
        state_dir: None,
        profiles_dir: None,
        system_root: None,
        json: false,
        no_reboot: false,
    }
}

fn parse_color(v: &str) -> Result<ColorChoice, String> {
    match v {
        "auto" => Ok(ColorChoice::Auto),
        "always" => Ok(ColorChoice::Always),
        "never" => Ok(ColorChoice::Never),
        other => Err(format!(
            "invalid --color value: {other} (auto|always|never)"
        )),
    }
}

fn parse_gen(v: &str) -> Result<u32, String> {
    v.parse::<u32>()
        .map_err(|_| format!("invalid generation number: {v}"))
}

#[cfg(test)]
#[allow(clippy::panic, clippy::expect_used, reason = "tests assert")]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, String> {
        parse_args(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn bare_invocation_is_help() {
        assert_eq!(parse(&[]).expect("ok").command, Command::Help);
    }

    #[test]
    fn chain_status_defaults() {
        let c = parse(&["chain"]).expect("ok");
        assert_eq!(c.command, Command::Chain);
        assert_eq!(c.color, ColorChoice::Auto);
        assert!(!c.no_pager);
    }

    #[test]
    fn color_and_no_pager_flags() {
        let c = parse(&["--color=never", "--no-pager", "status"]).expect("ok");
        assert_eq!(c.command, Command::Status);
        assert_eq!(c.color, ColorChoice::Never);
        assert!(c.no_pager);
    }

    #[test]
    fn color_separate_value_form() {
        let c = parse(&["--color", "always", "chain"]).expect("ok");
        assert_eq!(c.color, ColorChoice::Always);
    }

    #[test]
    fn reboot_rescue_yes() {
        assert_eq!(
            parse(&["reboot-rescue", "--yes"]).expect("ok").command,
            Command::RebootRescue { yes: true }
        );
        assert_eq!(
            parse(&["reboot-rescue"]).expect("ok").command,
            Command::RebootRescue { yes: false }
        );
    }

    #[test]
    fn reboot_into_generation() {
        assert_eq!(
            parse(&["reboot-into", "--generation", "42"])
                .expect("ok")
                .command,
            Command::RebootInto {
                generation: Some(42)
            }
        );
        assert_eq!(
            parse(&["reboot-into"]).expect("ok").command,
            Command::RebootInto { generation: None }
        );
    }

    #[test]
    fn default_variants() {
        assert_eq!(
            parse(&["default", "--show"]).expect("ok").command,
            Command::Default {
                generation: None,
                latest: false,
                show: true
            }
        );
        assert_eq!(
            parse(&["default", "--latest"]).expect("ok").command,
            Command::Default {
                generation: None,
                latest: true,
                show: false
            }
        );
        assert_eq!(
            parse(&["default", "--generation=7"]).expect("ok").command,
            Command::Default {
                generation: Some(7),
                latest: false,
                show: false
            }
        );
    }

    #[test]
    fn default_latest_and_generation_conflict() {
        assert!(parse(&["default", "--latest", "--generation=7"]).is_err());
    }

    #[test]
    fn unknown_flag_errors() {
        assert!(parse(&["--frobnicate", "chain"]).is_err());
    }

    #[test]
    fn invalid_color_errors() {
        assert!(parse(&["--color=purple", "chain"]).is_err());
    }

    #[test]
    fn invalid_generation_errors() {
        assert!(parse(&["reboot-into", "--generation", "notanumber"]).is_err());
    }
    #[test]
    fn explicit_retry_requires_valid_generation() {
        assert_eq!(
            parse(&["retry-generation", "--generation", "42"])
                .expect("retry")
                .command,
            Command::RetryGeneration { generation: 42 }
        );
        for args in [
            &["retry-generation"][..],
            &["retry-generation", "--generation", "0"],
            &["retry-generation", "--generation", "4294967295"],
            &["retry-generation", "--latest"],
        ] {
            assert!(parse(args).is_err());
        }
    }
}
