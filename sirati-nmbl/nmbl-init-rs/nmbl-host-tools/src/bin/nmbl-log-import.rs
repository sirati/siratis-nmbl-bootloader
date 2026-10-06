//! `nmbl-log-import [--socket PATH] SRC`: replay NMBL's pre-kexec transcript
//! into the journal. See [`nmbl_host_tools::log_import`].

use std::process::ExitCode;

fn main() -> ExitCode {
    match nmbl_host_tools::log_import::main_with(std::env::args_os().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("nmbl-log-import: {error}");
            ExitCode::FAILURE
        }
    }
}
