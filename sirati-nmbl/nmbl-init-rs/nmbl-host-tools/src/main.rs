//! `nmbl-sign` — the NMBL ML-DSA image signer (host binary).
//!
//! Parses the command line, dispatches to [`nmbl_host_tools::run`], and maps any
//! [`nmbl_host_tools::error::SignError`] to a printed message + exit code 1.
//! A usage error additionally prints the one-screen [`nmbl_host_tools::cli::USAGE`].

use std::process::ExitCode;

use nmbl_host_tools::cli;
use nmbl_host_tools::error::SignError;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match cli::parse(&args).and_then(nmbl_host_tools::run) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("nmbl-sign: {err}");
            if matches!(err, SignError::Usage(_)) {
                eprintln!("\n{}", cli::USAGE);
            }
            ExitCode::FAILURE
        }
    }
}
