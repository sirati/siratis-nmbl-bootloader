use std::path::{Path, PathBuf};
use std::process::ExitCode;

use nmbl_init::config::Config;
use nmbl_init::error::{Result, format_chain};
use nmbl_init::{log, nmbl_info, nmbl_warn};

const DEFAULT_CONFIG_PATH: &str = "/etc/nmbl/config.toml";

struct Args {
    config_path: PathBuf,
    errored_report: Option<PathBuf>,
}

fn parse_args() -> Args {
    let mut config_path = PathBuf::from(DEFAULT_CONFIG_PATH);
    let mut errored_report: Option<PathBuf> = None;

    for arg in std::env::args().skip(1) {
        if let Some(rest) = arg.strip_prefix("--config=") {
            config_path = PathBuf::from(rest);
        } else if let Some(rest) = arg.strip_prefix("--errored=") {
            errored_report = Some(PathBuf::from(rest));
        }
    }

    Args {
        config_path,
        errored_report,
    }
}

fn run(args: Args) -> Result<()> {
    let config = Config::load(&args.config_path)?;
    log::init(config.general.verbosity);

    // panic recovery wiring lands with shell module
    if let Some(report_path) = args.errored_report {
        let recovered = read_panic_report(&report_path);
        nmbl_warn!(
            "recovered from a panic; report at {}",
            report_path.display()
        );
        nmbl_warn!("panic report follows:\n{recovered}");
        return Ok(());
    }

    nmbl_info!("nmbl-init starting");
    Ok(())
}

fn read_panic_report(path: &Path) -> String {
    match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) => format!("(panic report at {} unreadable: {err})", path.display()),
    }
}

fn main() -> ExitCode {
    let args = parse_args();
    match run(args) {
        Ok(()) => ExitCode::from(0),
        Err(err) => {
            let chain = format_chain(&err as &dyn std::error::Error);
            // Bypass the logger so a config failure (which prevents log init)
            // still surfaces the chain to the operator.
            eprintln!("[nmbl] fatal: {chain}");
            ExitCode::from(1)
        }
    }
}
