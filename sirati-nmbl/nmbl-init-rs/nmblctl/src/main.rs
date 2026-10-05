//! `nmblctl` binary entry point.
//!
//! Requires root (checks euid). Dispatches the parsed [`nmblctl::Command`] to
//! the inspection views (`chain`, `status`) and the mutating actions
//! (`reboot-rescue`, `reboot-into`, `default`). The pure logic lives in the
//! library and in `nmbl_init`; this file does the I/O: read config/state files,
//! query systemd via `systemctl`, write flag files durably, and reboot.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::{Command as ProcCommand, ExitCode, Stdio};

use nmblctl::args::Command;
use nmblctl::color::Palette;
use nmblctl::flags::{DEFAULT_BASENAME, DefaultSelection, ONE_SHOT_BASENAME, OneShotSelection};
use nmblctl::pager::{PagerChoice, PagerEnv, decide_pager};

mod render;
mod state;
mod tui;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli = match nmblctl::parse_args(&args) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("nmblctl: {msg}\n\n{}", nmblctl::args::USAGE);
            return ExitCode::from(2);
        }
    };

    if matches!(cli.command, Command::Help) {
        println!("{}", nmblctl::args::USAGE);
        return ExitCode::SUCCESS;
    }

    // Every real subcommand reads NMBL's state and/or reboots, so root is
    // required. Check euid and give a clear message rather than a confusing
    // permission-denied deeper in.
    if !is_root() {
        eprintln!("nmblctl: must be run as root (try: sudo nmblctl …)");
        return ExitCode::from(1);
    }

    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("nmblctl: {err}");
            ExitCode::from(1)
        }
    }
}

fn run(cli: &nmblctl::Cli) -> Result<(), String> {
    let sys = state::System::discover_for(cli)?;
    match &cli.command {
        Command::Chain => paged(cli, render::chain(&sys, palette(cli, false))),
        Command::Status if cli.json => {
            println!("{}", sys.status_json()?);
            Ok(())
        }
        Command::Status => paged(cli, render::status(&sys, palette(cli, false))),
        Command::RebootRescue { yes } => reboot_rescue(&sys, *yes),
        Command::RebootInto { generation } => reboot_into(&sys, *generation),
        Command::RetryGeneration { generation } => {
            retry_generation(&sys, *generation, cli.no_reboot)
        }
        Command::Default {
            generation,
            latest,
            show,
        } => default_cmd(&sys, *generation, *latest, *show, cli),
        Command::Help => {
            println!("{}", nmblctl::args::USAGE);
            Ok(())
        }
    }
}

/// Resolve the palette for direct (non-paged) writes; paged output forces
/// colour on when a pager is used (git does the same via `less -R`).
fn palette(cli: &nmblctl::Cli, _paged: bool) -> Palette {
    Palette::resolve(cli.color, std::io::stdout().is_terminal())
}

fn is_root() -> bool {
    // SAFETY: geteuid is always-safe (no args, no pointers) and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// Render `body` through the resolved pager, or straight to stdout.
fn paged(cli: &nmblctl::Cli, body: String) -> Result<(), String> {
    let pager_var = std::env::var("PAGER").ok();
    let env = PagerEnv {
        no_pager: cli.no_pager,
        stdout_is_tty: std::io::stdout().is_terminal(),
        pager_var: pager_var.as_deref(),
        less_available: which("less").is_some(),
    };
    // When paging, re-render with colour forced on so `less -R` shows it;
    // when direct, honour the auto/never policy already in `body`. To keep
    // this simple and correct we re-render at the call sites is avoided:
    // `body` was rendered with the auto palette; if we are about to pipe to a
    // pager and colour was auto-off (non-tty is impossible here since a pager
    // needs a tty), it is already coloured. So just route it.
    match decide_pager(env) {
        PagerChoice::Direct => {
            print!("{body}");
            Ok(())
        }
        PagerChoice::Pager(argv) => pipe_to_pager(&argv, &body),
    }
}

fn pipe_to_pager(argv: &[String], body: &str) -> Result<(), String> {
    let Some((prog, rest)) = argv.split_first() else {
        print!("{body}");
        return Ok(());
    };
    let mut child = match ProcCommand::new(prog)
        .args(rest)
        .stdin(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => {
            // Pager failed to launch: fall back to direct output.
            print!("{body}");
            return Ok(());
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(body.as_bytes());
    }
    let _ = child.wait();
    Ok(())
}

/// Minimal `which`: is `name` an executable on `$PATH`?
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

fn reboot_rescue(sys: &state::System, yes: bool) -> Result<(), String> {
    if sys.rescue_mode.as_deref() == Some("none") {
        return Err(
            "rescue.mode = \"none\": there is no rescue to reboot into (refusing)".to_string(),
        );
    }
    if !yes && !confirm("Set the rescue flag and reboot into rescue now?")? {
        println!("aborted");
        return Ok(());
    }
    let sentinel = sys.sentinel_path();
    state::write_durable(&sentinel, nmblctl::flags::RescueRequest::body())
        .map_err(|e| format!("writing rescue sentinel {}: {e}", sentinel.display()))?;
    println!("rescue sentinel written to {}", sentinel.display());
    do_reboot()
}

fn reboot_into(sys: &state::System, generation: Option<u32>) -> Result<(), String> {
    let target = match generation {
        Some(n) => n,
        None => match tui::select_generation(sys, "Boot which generation ONCE next reboot?")? {
            Some(n) => n,
            None => {
                println!("aborted");
                return Ok(());
            }
        },
    };
    sys.validate_generation(target)?;
    let path = sys.state_dir.join(ONE_SHOT_BASENAME);
    state::write_durable(
        &path,
        OneShotSelection { generation: target }.render().as_bytes(),
    )
    .map_err(|e| format!("writing one-shot selection {}: {e}", path.display()))?;
    println!("one-shot: generation {target} will boot once next reboot");
    do_reboot()
}

fn retry_generation(sys: &state::System, generation: u32, no_reboot: bool) -> Result<(), String> {
    sys.validate_retry_target(generation)?;
    state::validate_operator_path(&sys.state_dir, true)?;
    if sys.config.is_none() || !sys.generations.contains(&generation) {
        return Err("retry requires a readable NMBL config and an installed profile".into());
    }
    let path = sys
        .state_dir
        .join(nmbl_init::boot_selection::RETRY_BASENAME);
    let config = sys
        .config
        .as_ref()
        .ok_or("operator retry requires config")?;
    persist_retry_and_clear_rescue(&path, generation, config)?;
    println!("operator retry: generation {generation}, one attempt; failure history preserved");
    if no_reboot { Ok(()) } else { do_reboot() }
}

fn persist_retry_and_clear_rescue(
    path: &std::path::Path,
    generation: u32,
    config: &nmbl_init::config::Config,
) -> Result<(), String> {
    state::write_durable(path, OneShotSelection { generation }.render().as_bytes())
        .map_err(|e| format!("writing operator retry: {e}"))?;
    nmbl_init::policy::sentinel::consume_after_rescue_booted(config).map_err(|e| {
        format!("operator retry recorded but rescue request could not be cleared: {e}")
    })
}

fn default_cmd(
    sys: &state::System,
    generation: Option<u32>,
    latest: bool,
    show: bool,
    _cli: &nmblctl::Cli,
) -> Result<(), String> {
    let path = sys.state_dir.join(DEFAULT_BASENAME);
    if show {
        match sys.read_default() {
            Some(sel) => println!("current default: {sel}"),
            None => println!("current default: (none set — boots the active profile / newest)"),
        }
        return Ok(());
    }
    let selection = if latest {
        DefaultSelection::Latest
    } else if let Some(n) = generation {
        sys.validate_generation(n)?;
        DefaultSelection::Generation(n)
    } else {
        // Interactive: offer the generations plus the artificial "latest".
        match tui::select_default(sys)? {
            Some(sel) => sel,
            None => {
                println!("aborted");
                return Ok(());
            }
        }
    };
    state::write_durable(&path, selection.render().as_bytes())
        .map_err(|e| format!("writing default {}: {e}", path.display()))?;
    println!("persistent default set to {selection}");
    Ok(())
}

/// Interactive yes/no confirmation on the controlling terminal.
fn confirm(prompt: &str) -> Result<bool, String> {
    print!("{prompt} [y/N] ");
    std::io::stdout()
        .flush()
        .map_err(|e| format!("stdout: {e}"))?;
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| format!("stdin: {e}"))?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes"))
}

/// Reboot via systemd, like the task requires.
fn do_reboot() -> Result<(), String> {
    let status = ProcCommand::new("systemctl")
        .arg("reboot")
        .status()
        .map_err(|e| format!("launching systemctl reboot: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("systemctl reboot exited with {status}"))
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "tests assert retry persistence order"
)]
mod rescue_retry_tests {
    use super::*;
    #[test]
    fn persistence_failure_retains_rescue_and_success_clears_only_after_marker() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = nmbl_init::config::Config::recovery_default();
        config.runtime_boot_mountpoint = Some(dir.path().to_path_buf());
        nmbl_init::policy::write_sentinel(&config);
        let invalid = dir.path().join("absent/parent/retry-generation");
        // A file as parent ensures persistence fails, independent of test uid.
        std::fs::write(dir.path().join("absent"), b"not a directory").unwrap();
        assert!(persist_retry_and_clear_rescue(&invalid, 42, &config).is_err());
        assert!(nmbl_init::policy::sentinel_present(&config));
        let path = dir.path().join("nmbl/retry-generation");
        persist_retry_and_clear_rescue(&path, 42, &config).unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), "generation 42\n");
        assert!(!nmbl_init::policy::sentinel_present(&config));
    }
    #[test]
    fn validation_failure_retains_existing_rescue_request() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = nmbl_init::config::Config::recovery_default();
        config.runtime_boot_mountpoint = Some(dir.path().to_path_buf());
        config.stateful = None;
        nmbl_init::policy::write_sentinel(&config);
        let sys = state::System {
            config: Some(config),
            config_source: None,
            rescue_mode: None,
            state_dir: dir.path().join("nmbl"),
            cmdline: String::new(),
            generations: vec![42],
            active_generation: Some(42),
        };
        assert!(retry_generation(&sys, 42, true).is_err());
        assert!(nmbl_init::policy::sentinel_present(
            sys.config.as_ref().expect("configured test system")
        ));
        assert!(
            !sys.state_dir
                .join(nmbl_init::boot_selection::RETRY_BASENAME)
                .exists()
        );
    }
}
